//! Personal-source projection into the durable merged TaskChampion chain.
//!
//! Phase 4 owns the source-truth → merged-chain direction. The HTTP TC
//! handlers are not cut over yet; this module exposes a gateway-owned entry
//! point that projects the user's canonical personal replica into the merged
//! replica and then appends those local merged operations to
//! `data/users/{user_id}/merged/sync.sqlite`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use taskchampion::storage::{AccessMode, Storage};
use taskchampion::{Annotation, Operations, Replica, SqliteStorage, Status, Tag, Task};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::merged_sync_gateway::planner::resolve_personal_visible_task_scope;
use crate::task_keys::udas::{CMDOCK_ACCOUNT_UDA, CMDOCK_KEY_UDA, CMDOCK_TASK_SCOPE_UDA};
use crate::tc_sync::storage::SyncStorage;

static MERGED_APPEND_LOCKS: OnceLock<dashmap::DashMap<String, Arc<tokio::sync::Mutex<()>>>> =
    OnceLock::new();

pub fn evict_merged_append_lock(user_id: &str) {
    if let Some(locks) = MERGED_APPEND_LOCKS.get() {
        locks.remove(user_id);
    }
}

const PROJECT_PROP: &str = "project";
const PRIORITY_PROP: &str = "priority";
const SCHEDULED_PROP: &str = "scheduled";
const START_PROP: &str = "start";
const END_PROP: &str = "end";

/// Result summary for one personal projection pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersonalProjectionSummary {
    /// Number of source tasks inspected.
    pub source_tasks: usize,
    /// Whether the merged replica received local operations before syncing to
    /// the durable merged protocol chain.
    pub changed: bool,
}

/// Project the canonical personal source replica into the user's durable merged
/// chain and append any resulting operations to merged sync storage.
///
/// This is the Phase-4 gateway entry point. It intentionally does not route
/// through REST DTOs or `tc_sync::handlers`; callers provide only `user_id`.
pub async fn project_personal_now(
    state: &AppState,
    user_id: &str,
) -> Result<PersonalProjectionSummary> {
    let started = Instant::now();
    let result = project_personal_now_inner(state, user_id).await;
    let changed = result
        .as_ref()
        .map(|summary| summary.changed)
        .unwrap_or(false);
    crate::metrics::record_merged_gateway_projection(
        started.elapsed().as_secs_f64(),
        if result.is_ok() { "ok" } else { "error" },
        changed,
    );
    result
}

async fn project_personal_now_inner(
    state: &AppState,
    user_id: &str,
) -> Result<PersonalProjectionSummary> {
    let append_lock = merged_append_lock(user_id);
    let _append_guard = append_lock.lock().await;

    let visible_scope = resolve_personal_visible_task_scope(state, user_id)
        .await
        .map_err(|reject| anyhow::anyhow!("{}: {}", reject.code, reject.message))?;

    crate::task_keys::backfill::ensure_user_task_keys_migrated(state, user_id)
        .await
        .with_context(|| {
            format!("ensure task-key migration before personal projection for {user_id}")
        })?;

    let source_replica = state.replica_manager.get_replica(user_id).await?;
    let source_tasks = {
        let mut source = source_replica.lock().await;
        source
            .all_tasks()
            .await
            .with_context(|| format!("read canonical source tasks for {user_id}"))?
    };

    let task_uuids = source_tasks.keys().map(Uuid::to_string).collect::<Vec<_>>();
    let projected_keys = state
        .store
        .lookup_task_keys_for_projection(
            user_id,
            &task_uuids,
            chrono::Utc::now().timestamp(),
            state.config.task_write.idempotency_pending_timeout_seconds,
        )
        .await
        .map_err(|err| anyhow::anyhow!(err))?;

    let user_dir = state.data_dir.join("users").join(user_id);
    let merged_replica_dir = user_dir.join("merged").join("replica");
    let mut merged_replica = open_merged_replica(&merged_replica_dir).await?;

    // Pull the latest durable merged-chain state into the projection replica
    // BEFORE diffing against canonical. A Taskwarrior client can push a
    // reserved-UDA "drift" (e.g. set `cmdock_key` to a forged value) on an
    // EXISTING task; that op lands in merged sync storage but not in this
    // projection replica's local state. Without this pre-sync the mirror below
    // compares canonical against the projection replica's own stale (already
    // canonical) value, sees no diff, and emits no corrective op — so the forged
    // value persists on the client until a *second* triggered projection pass.
    // Pulling first makes the drift visible to the mirror and corrected in a
    // single pass. Both syncs run under the per-user append lock acquired above,
    // so they cannot interleave with another projection for this user.
    sync_merged_replica(&mut merged_replica, &user_dir, user_id)
        .await
        .with_context(|| format!("pull merged projection replica before mirror for {user_id}"))?;

    let changed = apply_personal_source_snapshot(
        &source_tasks,
        &projected_keys,
        &visible_scope.key_prefix,
        &mut merged_replica,
    )
    .await?;

    if changed {
        sync_merged_replica(&mut merged_replica, &user_dir, user_id)
            .await
            .with_context(|| format!("sync merged projection replica for {user_id}"))?;
    }

    Ok(PersonalProjectionSummary {
        source_tasks: source_tasks.len(),
        changed,
    })
}

fn merged_append_lock(user_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    MERGED_APPEND_LOCKS
        .get_or_init(dashmap::DashMap::new)
        .entry(user_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

async fn open_merged_replica(path: &Path) -> Result<Replica<SqliteStorage>> {
    tokio::fs::create_dir_all(path)
        .await
        .with_context(|| format!("create merged replica dir at {}", path.display()))?;
    let storage = SqliteStorage::new(path, AccessMode::ReadWrite, true)
        .await
        .with_context(|| format!("open merged replica at {}", path.display()))?;
    Ok(Replica::new(storage))
}

/// Sync the merged projection replica against the user's durable merged sync
/// storage (`merged/sync.sqlite`). `Replica::sync` is bidirectional: it pushes
/// any pending local projection ops and pulls remote (client-pushed) ops. Used
/// both to pull client state in before mirroring and to push corrective ops
/// out after. Callers must already hold the per-user merged append lock.
async fn sync_merged_replica<S: Storage>(
    merged_replica: &mut Replica<S>,
    user_dir: &Path,
    user_id: &str,
) -> Result<()> {
    let storage = SyncStorage::open_merged(user_dir)
        .with_context(|| format!("open merged sync storage for {user_id}"))?;
    let mut server: Box<dyn taskchampion::Server> = Box::new(PlainSyncServer::new(storage));
    merged_replica.sync(&mut server, false).await?;
    Ok(())
}

/// Apply a source snapshot into a merged replica.
///
/// This helper is generic over storage so tests can use `InMemoryStorage`.
async fn apply_personal_source_snapshot<S: Storage>(
    source_tasks: &HashMap<Uuid, Task>,
    projected_keys: &HashMap<String, String>,
    task_scope_prefix: &str,
    merged_replica: &mut Replica<S>,
) -> Result<bool> {
    let merged_tasks = merged_replica
        .all_tasks()
        .await
        .context("read merged projection tasks")?;
    let source_uuids = source_tasks.keys().copied().collect::<HashSet<_>>();

    let mut ops = Operations::new();
    let mut changed = false;

    for (uuid, source) in source_tasks {
        let mut target = match merged_tasks.get(uuid) {
            Some(existing) => existing.clone(),
            None => {
                changed = true;
                merged_replica
                    .create_task(*uuid, &mut ops)
                    .await
                    .with_context(|| format!("create merged projected task {uuid}"))?
            }
        };

        changed |= mirror_task_fields(
            source,
            &mut target,
            projected_keys.get(&uuid.to_string()).map(String::as_str),
            task_scope_prefix,
            &mut ops,
        )
        .with_context(|| format!("mirror source task {uuid} into merged projection"))?;
    }

    // If a task no longer exists in source truth, keep the forward-only merged
    // Keep the merged chain convergent even if source maintenance ever prunes
    // a UUID from all_tasks(): without this tombstone pass a previously
    // projected task could remain visibly pending in the merged replica.
    for (uuid, target) in merged_tasks {
        if !source_uuids.contains(&uuid) && target.get_status() != Status::Deleted {
            let mut target = target;
            target.set_status(Status::Deleted, &mut ops)?;
            changed = true;
        }
    }

    if changed {
        merged_replica
            .commit_operations(ops)
            .await
            .context("commit merged projection operations")?;
    }

    Ok(changed)
}

fn mirror_task_fields(
    source: &Task,
    target: &mut Task,
    projected_key: Option<&str>,
    task_scope_prefix: &str,
    ops: &mut Operations,
) -> Result<bool> {
    let mut changed = false;

    changed |= set_if_changed(target.get_status() != source.get_status(), || {
        target.set_status(source.get_status(), ops)
    })?;
    changed |= set_if_changed(target.get_description() != source.get_description(), || {
        target.set_description(source.get_description().to_string(), ops)
    })?;
    changed |= set_if_changed(target.get_entry() != source.get_entry(), || {
        target.set_entry(source.get_entry(), ops)
    })?;
    changed |= set_if_changed(target.get_due() != source.get_due(), || {
        target.set_due(source.get_due(), ops)
    })?;
    changed |= set_if_changed(target.get_wait() != source.get_wait(), || {
        target.set_wait(source.get_wait(), ops)
    })?;

    changed |= mirror_value(target, PROJECT_PROP, source.get_value(PROJECT_PROP), ops)?;
    let source_priority = source.get_priority();
    let priority_value = if source_priority.is_empty() {
        None
    } else {
        Some(source_priority)
    };
    changed |= mirror_value(target, PRIORITY_PROP, priority_value, ops)?;
    changed |= mirror_value(
        target,
        SCHEDULED_PROP,
        source.get_value(SCHEDULED_PROP),
        ops,
    )?;
    changed |= mirror_value(target, START_PROP, source.get_value(START_PROP), ops)?;
    changed |= mirror_value(target, END_PROP, source.get_value(END_PROP), ops)?;

    changed |= mirror_tags(source, target, ops)?;
    changed |= mirror_dependencies(source, target, ops)?;
    changed |= mirror_annotations(source, target, ops)?;
    changed |= mirror_udas(source, target, projected_key, task_scope_prefix, ops)?;

    Ok(changed)
}

fn set_if_changed<F>(condition: bool, f: F) -> Result<bool>
where
    F: FnOnce() -> std::result::Result<(), taskchampion::Error>,
{
    if condition {
        f()?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn mirror_value(
    target: &mut Task,
    key: &str,
    desired: Option<&str>,
    ops: &mut Operations,
) -> Result<bool> {
    if target.get_value(key) == desired {
        return Ok(false);
    }
    target.set_value(key, desired.map(str::to_string), ops)?;
    Ok(true)
}

fn mirror_tags(source: &Task, target: &mut Task, ops: &mut Operations) -> Result<bool> {
    let source_tags = source
        .get_tags()
        .filter(Tag::is_user)
        .collect::<HashSet<_>>();
    let target_tags = target
        .get_tags()
        .filter(Tag::is_user)
        .collect::<HashSet<_>>();
    let mut changed = false;

    for tag in target_tags.difference(&source_tags) {
        target.remove_tag(tag, ops)?;
        changed = true;
    }
    for tag in source_tags.difference(&target_tags) {
        target.add_tag(tag, ops)?;
        changed = true;
    }

    Ok(changed)
}

fn mirror_dependencies(source: &Task, target: &mut Task, ops: &mut Operations) -> Result<bool> {
    let source_deps = source.get_dependencies().collect::<HashSet<_>>();
    let target_deps = target.get_dependencies().collect::<HashSet<_>>();
    let mut changed = false;

    for dep in target_deps.difference(&source_deps) {
        target.remove_dependency(*dep, ops)?;
        changed = true;
    }
    for dep in source_deps.difference(&target_deps) {
        target.add_dependency(*dep, ops)?;
        changed = true;
    }

    Ok(changed)
}

fn mirror_annotations(source: &Task, target: &mut Task, ops: &mut Operations) -> Result<bool> {
    let source_annotations = source
        .get_annotations()
        .map(|ann| (ann.entry, ann.description))
        .collect::<HashMap<_, _>>();
    let target_annotations = target
        .get_annotations()
        .map(|ann| (ann.entry, ann.description))
        .collect::<HashMap<_, _>>();
    let mut changed = false;

    for (entry, description) in &source_annotations {
        if target_annotations.get(entry) != Some(description) {
            target.add_annotation(
                Annotation {
                    entry: *entry,
                    description: description.clone(),
                },
                ops,
            )?;
            changed = true;
        }
    }
    for entry in target_annotations.keys() {
        if !source_annotations.contains_key(entry) {
            target.remove_annotation(*entry, ops)?;
            changed = true;
        }
    }

    Ok(changed)
}

fn mirror_udas(
    source: &Task,
    target: &mut Task,
    projected_key: Option<&str>,
    task_scope_prefix: &str,
    ops: &mut Operations,
) -> Result<bool> {
    let mut desired = source
        .get_user_defined_attributes()
        .filter(|(key, _)| !projection_owned_key(key))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect::<HashMap<_, _>>();

    desired.insert(
        CMDOCK_TASK_SCOPE_UDA.to_string(),
        task_scope_prefix.to_string(),
    );
    match projected_key {
        Some(key) => {
            desired.insert(CMDOCK_KEY_UDA.to_string(), key.to_string());
        }
        None => {
            desired.remove(CMDOCK_KEY_UDA);
        }
    }

    let current = target
        .get_user_defined_attributes()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect::<HashMap<_, _>>();

    let mut changed = false;
    for (key, value) in &desired {
        if current.get(key) != Some(value) {
            target.set_value(key, Some(value.clone()), ops)?;
            changed = true;
        }
    }
    for key in current.keys() {
        if !desired.contains_key(key) && !preserve_target_only_key(key) {
            target.set_value(key, None, ops)?;
            changed = true;
        }
    }

    Ok(changed)
}

fn projection_owned_key(key: &str) -> bool {
    matches!(
        key,
        CMDOCK_TASK_SCOPE_UDA | CMDOCK_ACCOUNT_UDA | CMDOCK_KEY_UDA
    )
}

fn preserve_target_only_key(key: &str) -> bool {
    // TC may expose internal recurrence/mask properties as user-defined. Phase
    // 4 mirrors ordinary source properties and UDAs; it should not invent a
    // destructive cleanup policy for internal target-only metadata.
    matches!(key, "recur" | "until" | "mask" | "imask" | "parent")
}

/// Plaintext `taskchampion::Server` backed by [`SyncStorage`].
///
/// The gateway stores plaintext merged-chain segments internally; per-device
/// encryption translation remains at the protocol edge in a later cutover
/// phase. Keeping this server local to the gateway prevents accidental reuse of
/// the old encrypted personal sync path.
struct PlainSyncServer {
    storage: SyncStorage,
}

impl PlainSyncServer {
    fn new(storage: SyncStorage) -> Self {
        Self { storage }
    }
}

#[async_trait(?Send)]
impl taskchampion::Server for PlainSyncServer {
    async fn add_version(
        &mut self,
        parent_version_id: Uuid,
        history_segment: Vec<u8>,
    ) -> std::result::Result<
        (
            taskchampion::server::AddVersionResult,
            taskchampion::server::SnapshotUrgency,
        ),
        taskchampion::Error,
    > {
        let add_result = match self
            .storage
            .add_version(parent_version_id, &history_segment)
        {
            Ok(result) => result,
            Err(err)
                if crate::merged_sync_gateway::sqlite_error::is_sqlite_constraint_violation(
                    &err,
                ) =>
            {
                Err(self
                    .storage
                    .get_latest_version_id()
                    .map_err(|err| taskchampion::Error::Server(err.to_string()))?)
            }
            Err(err) => return Err(taskchampion::Error::Server(err.to_string())),
        };
        match add_result {
            Ok(version_id) => Ok((
                taskchampion::server::AddVersionResult::Ok(version_id),
                taskchampion::server::SnapshotUrgency::None,
            )),
            Err(expected_parent) => Ok((
                taskchampion::server::AddVersionResult::ExpectedParentVersion(expected_parent),
                taskchampion::server::SnapshotUrgency::None,
            )),
        }
    }

    async fn get_child_version(
        &mut self,
        parent_version_id: taskchampion::server::VersionId,
    ) -> std::result::Result<taskchampion::server::GetVersionResult, taskchampion::Error> {
        let (child, _parent_known, _has_versions) = self
            .storage
            .get_child_version_with_context(parent_version_id)
            .map_err(|err| taskchampion::Error::Server(err.to_string()))?;
        Ok(match child {
            Some((version_id, parent_version_id, history_segment)) => {
                taskchampion::server::GetVersionResult::Version {
                    version_id,
                    parent_version_id,
                    history_segment,
                }
            }
            // Projection-only server: the caller is the local merged Replica::sync(),
            // not an external TaskChampion client, so the stale-client `Gone`
            // distinction belongs only in the HTTP protocol adapter.
            None => taskchampion::server::GetVersionResult::NoSuchVersion,
        })
    }

    async fn add_snapshot(
        &mut self,
        version_id: Uuid,
        snapshot: Vec<u8>,
    ) -> std::result::Result<(), taskchampion::Error> {
        self.storage
            .add_snapshot(version_id, &snapshot)
            .map_err(|err| taskchampion::Error::Server(err.to_string()))?;
        Ok(())
    }

    async fn get_snapshot(
        &mut self,
    ) -> std::result::Result<Option<(Uuid, Vec<u8>)>, taskchampion::Error> {
        self.storage
            .get_snapshot()
            .map_err(|err| taskchampion::Error::Server(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taskchampion::storage::inmemory::InMemoryStorage;
    use taskchampion::{Replica, Status};
    use tempfile::TempDir;

    fn one_source_task(uuid: Uuid, description: &str) -> HashMap<Uuid, Task> {
        let mut source = HashMap::new();
        let mut replica = Replica::new(InMemoryStorage::new());
        futures::executor::block_on(async {
            let mut ops = Operations::new();
            let mut task = replica.create_task(uuid, &mut ops).await.unwrap();
            task.set_status(Status::Pending, &mut ops).unwrap();
            task.set_description(description.to_string(), &mut ops)
                .unwrap();
            task.set_value("energy", Some("high".to_string()), &mut ops)
                .unwrap();
            task.add_tag(&Tag::try_from("work").unwrap(), &mut ops)
                .unwrap();
            replica.commit_operations(ops).await.unwrap();
            source = replica.all_tasks().await.unwrap();
        });
        source
    }

    #[tokio::test]
    async fn personal_projection_stamps_task_scope_prefix_and_canonical_key() {
        let task_uuid = Uuid::new_v4();
        let source = one_source_task(task_uuid, "project me");
        let mut keys = HashMap::new();
        keys.insert(task_uuid.to_string(), "WORK-1".to_string());
        let mut merged = Replica::new(InMemoryStorage::new());

        let changed = apply_personal_source_snapshot(&source, &keys, "WORK", &mut merged)
            .await
            .unwrap();
        assert!(changed);

        let projected = merged.get_task(task_uuid).await.unwrap().unwrap();
        assert_eq!(projected.get_description(), "project me");
        assert_eq!(projected.get_value(CMDOCK_TASK_SCOPE_UDA), Some("WORK"));
        assert_eq!(projected.get_value(CMDOCK_KEY_UDA), Some("WORK-1"));
        assert_eq!(projected.get_value("energy"), Some("high"));
        assert!(projected
            .get_tags()
            .any(|tag| tag == Tag::try_from("work").unwrap()));
    }

    #[tokio::test]
    async fn personal_projection_is_idempotent_when_source_is_unchanged() {
        let task_uuid = Uuid::new_v4();
        let source = one_source_task(task_uuid, "stable");
        let mut keys = HashMap::new();
        keys.insert(task_uuid.to_string(), "HOME-7".to_string());
        let mut merged = Replica::new(InMemoryStorage::new());

        assert!(
            apply_personal_source_snapshot(&source, &keys, "HOME", &mut merged)
                .await
                .unwrap()
        );
        assert!(
            !apply_personal_source_snapshot(&source, &keys, "HOME", &mut merged)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn projected_merged_chain_can_be_cloned_by_fresh_replica() {
        let task_uuid = Uuid::new_v4();
        let source = one_source_task(task_uuid, "clone me");
        let mut keys = HashMap::new();
        keys.insert(task_uuid.to_string(), "PERS-3".to_string());

        let mut merged = Replica::new(InMemoryStorage::new());
        assert!(
            apply_personal_source_snapshot(&source, &keys, "PERS", &mut merged)
                .await
                .unwrap()
        );

        let tmp = TempDir::new().unwrap();
        let storage = SyncStorage::open_merged(tmp.path()).unwrap();
        let server = PlainSyncServer::new(storage);
        let mut server: Box<dyn taskchampion::Server> = Box::new(server);
        merged.sync(&mut server, false).await.unwrap();

        let mut fresh = Replica::new(InMemoryStorage::new());
        let storage = SyncStorage::open_merged(tmp.path()).unwrap();
        let clone_server = PlainSyncServer::new(storage);
        let mut clone_server: Box<dyn taskchampion::Server> = Box::new(clone_server);
        fresh.sync(&mut clone_server, false).await.unwrap();

        let cloned = fresh.get_task(task_uuid).await.unwrap().unwrap();
        assert_eq!(cloned.get_description(), "clone me");
        assert_eq!(cloned.get_value(CMDOCK_TASK_SCOPE_UDA), Some("PERS"));
        assert_eq!(cloned.get_value(CMDOCK_KEY_UDA), Some("PERS-3"));
    }
}
