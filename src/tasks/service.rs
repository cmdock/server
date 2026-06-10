use std::collections::{HashMap, HashSet};
use std::time::Instant;

use axum::http::StatusCode;
use metrics::counter;
use taskchampion::{Operations, Status};
use uuid::Uuid;

use super::models::{AddTaskRequest, ModifyTaskRequest, TaskItem};
use super::mutations::{self, TaskMutationAudit, TaskMutationKind};
use crate::app_state::AppState;
use crate::metrics as m;
use crate::replica;
use crate::task_keys::udas::{CMDOCK_KEY_UDA, CMDOCK_TASK_SCOPE_UDA};
use crate::tasks::parser;
use crate::user_runtime::{handle_replica_error, open_user_replica};

/// Phase 4 service-entry gate: ensure pre-feature task-key backfill has
/// run for this user before a mutation operates on existing tasks. Maps
/// any backfill failure to `INTERNAL_SERVER_ERROR`. Fast-path is a single
/// `DashMap` lookup on the `RuntimeRecoveryCoordinator` cache.
async fn ensure_task_keys_migrated_for_mutation(
    state: &AppState,
    user_id: &str,
) -> Result<(), StatusCode> {
    crate::task_keys::backfill::ensure_user_task_keys_migrated(state, user_id)
        .await
        .map_err(|e| {
            tracing::error!(
                error = %e,
                user_id = %user_id,
                "task-keys backfill failed in mutation service",
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

pub struct TaskMutationSuccess {
    pub kind: TaskMutationKind,
    pub uuid: Uuid,
    pub task_item: TaskItem,
    /// Canonical task key (`<PREFIX>-<n>`) drives `TaskActionResponse.key`
    /// on the wire. Populated on `add_task` after a successful
    /// `commit_task_key`. Lifecycle endpoints (`done`, `undo`, `delete`,
    /// `modify`) leave this `None` — their `TaskActionResponse` shape
    /// doesn't surface the key today. The projected `TaskItem` returned
    /// from those mutations DOES carry `key` (sourced via
    /// `lookup_lifecycle_key_map` so webhook payloads match GET reads).
    pub key: Option<String>,
    pub changed_fields: Option<Vec<String>>,
    pub audit: TaskMutationAudit,
}

/// Where in the mutation pipeline an error occurred. Drives the
/// idempotency rollback decision per `task-write-contract.md` § Failure
/// handling: pre-commit → KnownNoCommit (rollback the pending dedup
/// row); at-commit-or-after → Ambiguous (LEAVE the pending row, let
/// lookup-time expiry bound the residual window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitPhase {
    /// Validation, business-rule rejection, or any error raised before
    /// `Replica::commit_operations` was called. Safe to roll back the
    /// pending dedup row — the underlying mutation definitely did not
    /// commit.
    PreCommit,
    /// Error from `Replica::commit_operations` itself, OR any post-commit
    /// fallible operation (e.g. `pending_tasks()` for `changed_fields`
    /// computation on modify). The mutation may or may not have committed
    /// to TC; we cannot safely retry. Caller must NOT roll back the
    /// pending dedup row — lookup-time expiry will eventually free it.
    AmbiguousCommit,
}

/// Error returned from idempotency-aware service entry points
/// (`add_task`, `modify_task`). Carries both the wire status and the
/// commit-phase classification.
///
/// Other service entry points (`complete_task`, `undo_task`,
/// `delete_task`) return plain `Result<_, StatusCode>` since they are
/// out of `Idempotency-Key` scope per the contract.
#[derive(Debug)]
pub struct ServiceError {
    pub status: StatusCode,
    pub phase: CommitPhase,
}

impl ServiceError {
    fn pre_commit(status: StatusCode) -> Self {
        Self {
            status,
            phase: CommitPhase::PreCommit,
        }
    }
    fn ambiguous(status: StatusCode) -> Self {
        Self {
            status,
            phase: CommitPhase::AmbiguousCommit,
        }
    }
}

/// Look up the committed canonical key for a task UUID and return a
/// 1-entry `HashMap` suitable for passing into `task_to_item`. Used by
/// lifecycle mutations (`complete`, `undo`, `delete`, `modify`) so the
/// projected `TaskItem.key` matches what list/get responses surface for
/// the same task — webhook payloads must not diverge from REST reads.
///
/// On store error, logs and returns an empty map (failure to surface
/// `key` on a webhook payload is preferable to failing the whole
/// mutation; the wire response on the action endpoint stays correct
/// because it sources `key` from the freshly-allocated value on
/// `add_task`, and lifecycle mutations don't claim a key on the action
/// response shape today).
async fn lookup_lifecycle_key_map(
    state: &AppState,
    user_id: &str,
    uuid: Uuid,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let uuid_str = uuid.to_string();
    match state
        .store
        .lookup_task_key_by_uuid(user_id, &uuid_str)
        .await
    {
        Ok(Some((canonical_key, key_state))) => {
            if key_state == crate::store::models::KeyState::Committed {
                out.insert(uuid_str, canonical_key);
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                error = %e,
                user_id = %user_id,
                task_id = %uuid,
                "lookup_task_key_by_uuid failed during lifecycle mutation; \
                 webhook payload TaskItem.key will be omitted",
            );
        }
    }
    out
}

pub async fn add_task(
    state: &AppState,
    user_id: &str,
    body: &AddTaskRequest,
) -> Result<TaskMutationSuccess, ServiceError> {
    // Phase 4: ensure pre-feature tasks are backfilled before we read
    // MAX(n) for the new allocation. ensure_user_task_keys_migrated
    // briefly takes + releases the per-user mutation lock; this call is
    // a no-op fast-path on the cache hit. Running it BEFORE we acquire
    // the lock for the create flow keeps lock ordering simple.
    crate::task_keys::backfill::ensure_user_task_keys_migrated(state, user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, user_id = %user_id, "task-keys backfill failed in add_task");
            ServiceError::pre_commit(StatusCode::INTERNAL_SERVER_ERROR)
        })?;

    let rep_arc = open_user_replica(state, user_id, "api")
        .await
        .map_err(ServiceError::pre_commit)?;
    let parsed = parser::parse_raw(&body.raw);
    let uuid = Uuid::new_v4();

    // Per-user mutation lock — gates the reaper's check-and-burn pass so a
    // stalled-mid-commit row can't be wrongly burned. Acquired BEFORE the
    // replica lock per ADR-0002 / `task-write-contract.md` § Task Keys
    // lock-order rule. The reaper acquires the same lock first, then DB
    // — symmetric ordering eliminates the deadlock risk.
    let mutation_lock = state.recovery_runtime.task_mutation_lock(user_id);
    let _mutation_guard = mutation_lock.lock().await;

    // Resolve user prefix. Post-Phase-1 startup backfill, every user has
    // one; missing prefix is an operational bug, not a client error.
    let prefix_t = Instant::now();
    let prefix_res = state.store.get_user_prefix(user_id).await;
    ::metrics::histogram!("config_call_seconds", "call" => "get_user_prefix")
        .record(prefix_t.elapsed().as_secs_f64());
    let prefix = match prefix_res {
        Ok(Some(p)) => p,
        Ok(None) => {
            tracing::error!(
                user_id = %user_id,
                "User has no prefix — Phase 1 backfill never assigned one. \
                 Cannot allocate task key.",
            );
            return Err(ServiceError::pre_commit(StatusCode::INTERNAL_SERVER_ERROR));
        }
        Err(e) => {
            tracing::error!(error = %e, user_id = %user_id, "get_user_prefix failed");
            return Err(ServiceError::pre_commit(StatusCode::INTERNAL_SERVER_ERROR));
        }
    };

    // Reserve the next allocation slot under `BEGIN IMMEDIATE`, attaching the
    // task UUID in the SAME write (3->2 config writes per create, #148). The
    // uuid is already known here, so there is no separate `attach` write and
    // no NULL-uuid phase. `MAX(n)+1` includes burned rows so rolled-back N
    // values cannot be reused; `attempt_id` guards subsequent commit / burn
    // against stale finalisers. A crash before the TC commit below leaves a
    // uuid-attached pending row that the reaper's TC-scan recovers (TC task
    // absent -> burn; present with matching `cmdock_key` -> finalise) — the
    // same recovery shape the merged-sync gateway create path already uses.
    let task_uuid_str = uuid.to_string();
    let reserve_t = Instant::now();
    let reserve_res = state
        .store
        .reserve_task_key_pending_for_uuid(user_id, &prefix, &task_uuid_str)
        .await;
    ::metrics::histogram!("config_call_seconds", "call" => "reserve")
        .record(reserve_t.elapsed().as_secs_f64());
    let (n, attempt_id) = reserve_res.map_err(|e| {
        tracing::error!(error = %e, user_id = %user_id, "reserve_task_key_pending_for_uuid failed");
        ServiceError::pre_commit(StatusCode::INTERNAL_SERVER_ERROR)
    })?;
    let canonical_key = format!("{prefix}-{n}");

    let op_start = Instant::now();
    let result: Result<TaskItem, ServiceError> = async {
        let lock_wait_start = Instant::now();
        let mut rep = rep_arc.lock().await;
        m::record_replica_lock_wait("create_task", lock_wait_start.elapsed().as_secs_f64());
        let mut ops = Operations::new();

        // All steps below build the Operations; none have committed.
        // Errors here are PreCommit per § Failure handling.
        let step_start = Instant::now();
        let mut task = rep
            .create_task(uuid, &mut ops)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "create_task", "api"))
            .map_err(ServiceError::pre_commit)?;
        m::record_task_mutation_step(
            "create_task",
            "create_task",
            step_start.elapsed().as_secs_f64(),
        );

        let step_start = Instant::now();
        task.set_status(Status::Pending, &mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "set_status", "api"))
            .map_err(ServiceError::pre_commit)?;
        m::record_task_mutation_step(
            "create_task",
            "set_status",
            step_start.elapsed().as_secs_f64(),
        );

        let step_start = Instant::now();
        task.set_entry(Some(chrono::Utc::now()), &mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "set_entry", "api"))
            .map_err(ServiceError::pre_commit)?;
        m::record_task_mutation_step(
            "create_task",
            "set_entry",
            step_start.elapsed().as_secs_f64(),
        );

        let step_start = Instant::now();
        replica::apply_parsed_fields(&mut task, &parsed, &mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "apply_fields", "api"))
            .map_err(ServiceError::pre_commit)?;
        m::record_task_mutation_step(
            "create_task",
            "apply_fields",
            step_start.elapsed().as_secs_f64(),
        );

        // Set the `cmdock_key` UDA so external sync clients (TW CLI) can
        // see the canonical key. Wire `key` is sourced from the
        // allocation table — not this UDA — but the UDA is load-bearing
        // for the reaper's TC-scan recovery path (matches by UDA value).
        let step_start = Instant::now();
        task.set_value(CMDOCK_KEY_UDA, Some(canonical_key.clone()), &mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "set_cmdock_key", "api"))
            .map_err(ServiceError::pre_commit)?;
        task.set_value(CMDOCK_TASK_SCOPE_UDA, Some(prefix.clone()), &mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "set_cmdock_task_scope", "api"))
            .map_err(ServiceError::pre_commit)?;
        m::record_task_mutation_step(
            "create_task",
            "set_cmdock_key",
            step_start.elapsed().as_secs_f64(),
        );

        // The pending row was already uuid-attached at reserve time (#148),
        // so there is no separate `attach` write here. If the TC commit below
        // or `commit_task_key` fails, the row stays `pending` with task_uuid
        // set — recoverable by the reaper's TC-scan path.

        // Commit boundary: errors from here are AmbiguousCommit per
        // contract § Failure handling — the mutation may have committed.
        let step_start = Instant::now();
        rep.commit_operations(ops)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "commit", "api"))
            .map_err(ServiceError::ambiguous)?;
        m::record_task_mutation_step("create_task", "commit", step_start.elapsed().as_secs_f64());

        // Allocation finalise: state transition pending → committed. The
        // primitive is idempotent on already-committed-with-same-attempt
        // (the reaper-race regression path: reaper may finalise the row
        // while this call is in flight; resumed call sees committed and
        // returns Ok). On any DB error after a successful TC commit,
        // classify as Ambiguous — the reaper's UUID+UDA scan recovers.
        let commit_key_t = Instant::now();
        let commit_key_res = state
            .store
            .commit_task_key(user_id, &prefix, n, &attempt_id)
            .await;
        ::metrics::histogram!("config_call_seconds", "call" => "commit_task_key")
            .record(commit_key_t.elapsed().as_secs_f64());
        if let Err(e) = commit_key_res {
            tracing::error!(
                error = %e,
                user_id = %user_id,
                prefix = %prefix,
                n,
                "commit_task_key failed after TC commit succeeded — reaper will recover",
            );
            return Err(ServiceError::ambiguous(StatusCode::INTERNAL_SERVER_ERROR));
        }

        // Project the task. Pass a single-entry map so `TaskItem.key`
        // matches the canonical key we just allocated.
        let mut keys = HashMap::with_capacity(1);
        keys.insert(task_uuid_str.clone(), canonical_key.clone());
        Ok(crate::tasks::projection::task_to_item(
            &task,
            None,
            Some(&keys),
        ))
    }
    .await;

    let elapsed = op_start.elapsed().as_secs_f64();
    match &result {
        Ok(_) => m::record_replica_op("create_task", elapsed, "ok"),
        Err(_) => m::record_replica_op("create_task", elapsed, "error"),
    }

    let task_item = result?;

    // First-execution-only audit + counter; replays return the stored
    // response payload and never re-enter this code path.
    counter!("task_keys_allocated_total").increment(1);
    tracing::info!(
        target: "audit",
        action = "task.key.allocated",
        source = "api",
        user_id = %user_id,
        task_id = %uuid,
        key = %canonical_key,
    );

    Ok(TaskMutationSuccess {
        kind: TaskMutationKind::Create,
        uuid,
        task_item,
        key: Some(canonical_key),
        changed_fields: None,
        audit: TaskMutationAudit::Create {
            project: parsed.project.clone(),
            priority: parsed.priority.clone(),
        },
    })
}

pub async fn complete_task(
    state: &AppState,
    user_id: &str,
    uuid: Uuid,
) -> Result<TaskMutationSuccess, StatusCode> {
    ensure_task_keys_migrated_for_mutation(state, user_id).await?;
    let rep_arc = open_user_replica(state, user_id, "api").await?;

    let op_start = Instant::now();
    let result: Result<TaskItem, StatusCode> = async {
        let lock_wait_start = Instant::now();
        let mut rep = rep_arc.lock().await;
        m::record_replica_lock_wait("complete_task", lock_wait_start.elapsed().as_secs_f64());
        let task = rep
            .get_task(uuid)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "get_task", "api"))?;

        let mut task = task.ok_or(StatusCode::NOT_FOUND)?;
        if task.get_status() != Status::Pending {
            return Err(StatusCode::CONFLICT);
        }

        let mut ops = Operations::new();
        task.done(&mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "complete_task", "api"))?;

        rep.commit_operations(ops)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "commit", "api"))?;

        let task_keys = lookup_lifecycle_key_map(state, user_id, uuid).await;
        Ok(crate::tasks::projection::task_to_item(
            &task,
            None,
            Some(&task_keys),
        ))
    }
    .await;

    let elapsed = op_start.elapsed().as_secs_f64();
    match &result {
        Ok(_) => m::record_replica_op("complete_task", elapsed, "ok"),
        Err(_) => m::record_replica_op("complete_task", elapsed, "error"),
    }

    Ok(TaskMutationSuccess {
        kind: TaskMutationKind::Complete,
        uuid,
        task_item: result?,
        key: None,
        changed_fields: None,
        audit: TaskMutationAudit::None,
    })
}

pub async fn undo_task(
    state: &AppState,
    user_id: &str,
    uuid: Uuid,
) -> Result<TaskMutationSuccess, StatusCode> {
    ensure_task_keys_migrated_for_mutation(state, user_id).await?;
    let rep_arc = open_user_replica(state, user_id, "api").await?;

    let op_start = Instant::now();
    let result: Result<(TaskItem, Vec<String>), StatusCode> = async {
        let lock_wait_start = Instant::now();
        let mut rep = rep_arc.lock().await;
        m::record_replica_lock_wait("undo_task", lock_wait_start.elapsed().as_secs_f64());
        let task = rep
            .get_task(uuid)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "get_task", "api"))?;

        let mut task = task.ok_or(StatusCode::NOT_FOUND)?;
        let task_keys = lookup_lifecycle_key_map(state, user_id, uuid).await;
        let before = crate::tasks::projection::task_to_item(&task, None, Some(&task_keys));

        if task.get_status() != Status::Completed {
            return Err(StatusCode::CONFLICT);
        }

        let mut ops = Operations::new();
        task.set_status(Status::Pending, &mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "undo_task", "api"))?;

        rep.commit_operations(ops)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "commit", "api"))?;

        let after = crate::tasks::projection::task_to_item(&task, None, Some(&task_keys));
        Ok((after.clone(), mutations::changed_fields(&before, &after)))
    }
    .await;

    let elapsed = op_start.elapsed().as_secs_f64();
    match &result {
        Ok(_) => m::record_replica_op("undo_task", elapsed, "ok"),
        Err(_) => m::record_replica_op("undo_task", elapsed, "error"),
    }

    let (task_item, changed_fields) = result?;
    Ok(TaskMutationSuccess {
        kind: TaskMutationKind::Undo,
        uuid,
        task_item,
        key: None,
        changed_fields: Some(changed_fields),
        audit: TaskMutationAudit::None,
    })
}

pub async fn delete_task(
    state: &AppState,
    user_id: &str,
    uuid: Uuid,
) -> Result<TaskMutationSuccess, StatusCode> {
    ensure_task_keys_migrated_for_mutation(state, user_id).await?;
    let rep_arc = open_user_replica(state, user_id, "api").await?;

    let op_start = Instant::now();
    let result: Result<TaskItem, StatusCode> = async {
        let lock_wait_start = Instant::now();
        let mut rep = rep_arc.lock().await;
        m::record_replica_lock_wait("delete_task", lock_wait_start.elapsed().as_secs_f64());
        let mut task = rep
            .get_task(uuid)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "get_task", "api"))?
            .ok_or(StatusCode::NOT_FOUND)?;

        let mut ops = Operations::new();
        task.set_status(Status::Deleted, &mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "delete_task", "api"))?;

        rep.commit_operations(ops)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "commit", "api"))?;

        let task_keys = lookup_lifecycle_key_map(state, user_id, uuid).await;
        Ok(crate::tasks::projection::task_to_item(
            &task,
            None,
            Some(&task_keys),
        ))
    }
    .await;

    let elapsed = op_start.elapsed().as_secs_f64();
    match &result {
        Ok(_) => m::record_replica_op("delete_task", elapsed, "ok"),
        Err(_) => m::record_replica_op("delete_task", elapsed, "error"),
    }

    Ok(TaskMutationSuccess {
        kind: TaskMutationKind::Delete,
        uuid,
        task_item: result?,
        key: None,
        changed_fields: None,
        audit: TaskMutationAudit::None,
    })
}

pub fn parse_modify_dependencies(
    uuid: Uuid,
    depends: Option<&Vec<String>>,
) -> Result<Option<Vec<Uuid>>, &'static str> {
    let Some(depends) = depends else {
        return Ok(None);
    };

    let mut unique = Vec::with_capacity(depends.len());
    let mut seen = HashSet::with_capacity(depends.len());
    for dep in depends {
        let dep_uuid = Uuid::parse_str(dep).map_err(|_| "invalid_dependency_uuid")?;
        if dep_uuid == uuid {
            return Err("self_dependency");
        }
        if seen.insert(dep_uuid) {
            unique.push(dep_uuid);
        }
    }

    Ok(Some(unique))
}

pub async fn modify_task(
    state: &AppState,
    user_id: &str,
    uuid: Uuid,
    body: &ModifyTaskRequest,
    parsed_depends: Option<Vec<Uuid>>,
) -> Result<TaskMutationSuccess, ServiceError> {
    ensure_task_keys_migrated_for_mutation(state, user_id)
        .await
        .map_err(ServiceError::pre_commit)?;
    let rep_arc = open_user_replica(state, user_id, "api")
        .await
        .map_err(ServiceError::pre_commit)?;

    let op_start = Instant::now();
    let result: Result<(TaskItem, Vec<String>), ServiceError> = async {
        let lock_wait_start = Instant::now();
        let mut rep = rep_arc.lock().await;
        m::record_replica_lock_wait("modify_task", lock_wait_start.elapsed().as_secs_f64());
        // All steps before `rep.commit_operations` are PreCommit.
        let mut task = rep
            .get_task(uuid)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "get_task", "api"))
            .map_err(ServiceError::pre_commit)?
            .ok_or(ServiceError::pre_commit(StatusCode::NOT_FOUND))?;
        // Build pending set for accurate depends detection in changed_fields.
        let pending_uuids: std::collections::HashSet<uuid::Uuid> = rep
            .pending_tasks()
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "pending_tasks", "api"))
            .map_err(ServiceError::pre_commit)?
            .iter()
            .map(|t| t.get_uuid())
            .collect();
        let task_keys = lookup_lifecycle_key_map(state, user_id, uuid).await;
        let before =
            crate::tasks::projection::task_to_item(&task, Some(&pending_uuids), Some(&task_keys));
        // Capture wait / scheduled before mutations — TaskItem doesn't expose
        // these date strings directly, so `changed_fields(before, after)` can't
        // detect changes to them. We compare the underlying TC values manually
        // and append to the changed-fields list before audit/webhook dispatch.
        let before_wait = task.get_wait();
        let before_scheduled = crate::tasks::parse_task_scheduled(&task);

        if task.get_status() == Status::Deleted {
            return Err(ServiceError::pre_commit(StatusCode::CONFLICT));
        }

        let mut ops = Operations::new();

        if let Some(ref desc) = body.description {
            task.set_description(desc.clone(), &mut ops)
                .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                .map_err(ServiceError::pre_commit)?;
        }

        // project — JSON-Merge-Patch tri-state per task-write-contract.md
        // § Clear semantics: outer None = absent (leave unchanged);
        // Some(None) = null (clear); Some(Some(v)) = set.
        match &body.project {
            None => {}
            Some(None) => {
                task.set_value("project", None, &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
            Some(Some(project)) => {
                task.set_value("project", Some(project.clone()), &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
        }

        // priority — same tri-state. TC's typed `set_priority` only takes
        // a String (no Option), so clear must go through the generic
        // `set_value("priority", None, …)` path.
        match &body.priority {
            None => {}
            Some(None) => {
                task.set_value("priority", None, &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
            Some(Some(priority)) => {
                task.set_priority(priority.clone(), &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
        }

        // due — tri-state. The broad date parser (named dates, ISO,
        // relative durations) continues to apply on set; the canonical-only
        // requirement on `wait` / `scheduled` is a separate contract
        // asymmetry preserved per the arch ruling on #100.
        match &body.due {
            None => {}
            Some(None) => {
                task.set_due(None, &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
            Some(Some(due_str)) => {
                let dt = crate::tasks::dates::parse_date_value(due_str);
                task.set_due(dt, &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
        }

        if let Some(ref new_tags) = body.tags {
            let existing_tags: Vec<_> = task.get_tags().filter(|t| t.is_user()).collect();
            for tag in &existing_tags {
                task.remove_tag(tag, &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
            for tag_str in new_tags {
                if let Ok(tag) = taskchampion::Tag::try_from(tag_str.as_str()) {
                    task.add_tag(&tag, &mut ops)
                        .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                        .map_err(ServiceError::pre_commit)?;
                }
            }
        }

        if let Some(ref new_depends) = parsed_depends {
            for dep_uuid in new_depends {
                if rep
                    .get_task(*dep_uuid)
                    .await
                    .map_err(|e| handle_replica_error(state, user_id, &e, "get_task", "api"))
                    .map_err(ServiceError::pre_commit)?
                    .is_none()
                {
                    return Err(ServiceError::pre_commit(StatusCode::BAD_REQUEST));
                }
            }

            let existing_deps: Vec<_> = task.get_dependencies().collect();
            for dep in existing_deps {
                task.remove_dependency(dep, &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
            for dep in new_depends {
                task.add_dependency(*dep, &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
        }

        // wait — tri-state per `task-write-contract.md` § Field table:
        // outer None = field absent (leave unchanged); Some(None) = explicit
        // null (clear); Some(Some(s)) = canonical date string (set).
        // The handler validated canonical format before reaching here.
        match &body.wait {
            None => {}
            Some(None) => {
                task.set_wait(None, &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
            Some(Some(canonical)) => {
                let dt = replica::parse_tw_date(canonical)
                    .ok_or(ServiceError::pre_commit(StatusCode::BAD_REQUEST))?;
                task.set_wait(Some(dt), &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
        }

        // scheduled — same tri-state, but TC stores `scheduled` as a
        // generic property holding epoch seconds, not via a typed setter.
        match &body.scheduled {
            None => {}
            Some(None) => {
                task.set_value("scheduled", None, &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
            Some(Some(canonical)) => {
                let dt = replica::parse_tw_date(canonical)
                    .ok_or(ServiceError::pre_commit(StatusCode::BAD_REQUEST))?;
                task.set_value("scheduled", Some(dt.timestamp().to_string()), &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))
                    .map_err(ServiceError::pre_commit)?;
            }
        }

        // Commit boundary — errors here AND below are AmbiguousCommit per
        // contract § Failure handling: `pending_tasks()` runs AFTER the
        // mutation has committed and computes changed_fields for audit /
        // webhooks; if it fails the dedup row must NOT be rolled back
        // because the underlying mutation persisted.
        rep.commit_operations(ops)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "commit", "api"))
            .map_err(ServiceError::ambiguous)?;

        // Rebuild pending set post-commit for accurate depends change detection.
        let pending_after: std::collections::HashSet<uuid::Uuid> = rep
            .pending_tasks()
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "pending_tasks", "api"))
            .map_err(ServiceError::ambiguous)?
            .iter()
            .map(|t| t.get_uuid())
            .collect();
        let after =
            crate::tasks::projection::task_to_item(&task, Some(&pending_after), Some(&task_keys));
        let mut changed = mutations::changed_fields(&before, &after);
        // Append wait / scheduled deltas — see comment at the corresponding
        // before-capture above.
        let after_wait = task.get_wait();
        if before_wait != after_wait {
            changed.push("wait".to_string());
        }
        let after_scheduled = crate::tasks::parse_task_scheduled(&task);
        if before_scheduled != after_scheduled {
            changed.push("scheduled".to_string());
        }
        // Keep the result deterministic / stable for audit + webhook
        // consumers that may diff field-name lists.
        changed.sort_unstable();
        changed.dedup();
        Ok((after.clone(), changed))
    }
    .await;

    let elapsed = op_start.elapsed().as_secs_f64();
    match &result {
        Ok(_) => m::record_replica_op("modify_task", elapsed, "ok"),
        Err(_) => m::record_replica_op("modify_task", elapsed, "error"),
    }

    let (task_item, changed_fields) = result?;
    Ok(TaskMutationSuccess {
        kind: TaskMutationKind::Modify,
        uuid,
        task_item,
        key: None,
        changed_fields: Some(changed_fields),
        audit: TaskMutationAudit::Modify {
            changed_description: body.description.is_some(),
            changed_project: body.project.is_some(),
            changed_priority: body.priority.is_some(),
            changed_due: body.due.is_some(),
            changed_tags: body.tags.is_some(),
            changed_depends: body.depends.is_some(),
        },
    })
}
