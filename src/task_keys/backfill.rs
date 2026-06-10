//! Phase 4 personal Task Scope lazy task-key backfill (server#130,
//! `task-write-contract.md` § Task Keys).
//!
//! Existing tasks (created before the feature shipped) need both an
//! allocation row in `task_key_allocations` and a `cmdock_key` UDA on
//! the TC task. The backfill is **personal Task Scope lazy**: triggered on the
//! first server access for each user after Phase 4 ships. The fast path
//! is `RuntimeRecoveryCoordinator::task_keys_migration_marked(user_id)`
//! — a `DashMap` lookup; the slow path is a DB column check under the
//! per-user mutation lock followed by Phase B/A+C below.
//!
//! Coordinator-side logic lives in this module (alongside `reaper.rs`
//! and the future Phase 5 drift recovery) — DB primitives in
//! `src/store/sqlite/task_keys.rs`. Per ADR-0002 §Independence: one
//! sub-tree owns the whole feature; no top-level `migration/` module.
//!
//! ## Single-server-process assumption
//!
//! The per-user mutex (`RuntimeRecoveryCoordinator::task_mutation_lock`)
//! is in-memory and not shared across processes. The current deploy
//! model is single-process per config DB (Docker single-container or
//! systemd unit). A future multi-process deployment sharing the same
//! config DB would need a DB-level advisory lock; pinned in
//! `docs/reference/storage-layout-reference.md`.
//!
//! ## Cache invalidation
//!
//! `migration_status_cache` is invalidated via
//! `RuntimeRecoveryCoordinator::evict_user`, which is the single owner
//! of the per-user runtime-cache eviction recipe (CLAUDE.md § Runtime
//! cache eviction). Restore (`OperatorMaintenanceBackend::restore`),
//! `delete_user`, and offline-quarantine all already flow through
//! `evict_user`, so the migration cache stays aligned with the DB across
//! every reset path without any inline `.clear()` calls.
//!
//! ## Lock-hold during Phase B
//!
//! Phase B writes the `cmdock_key` UDA to every existing task in TC. For
//! a user with thousands of tasks this can be a noticeable wall-clock
//! cost; the per-user mutation lock is held for the duration, blocking
//! that user's mutations and their reaper turns. This trade-off is
//! contract-driven (`task-write-contract.md` § Wire vs cache nullability
//! — synchronous-on-first-access, not background) and intentionally
//! affects only the first-access request per user lifetime.
//!
//! ## Order of operations
//!
//! Once the lock is held and the DB column is observed `NULL`:
//!
//! 1. Read user prefix from `users.prefix`.
//! 2. Read `MAX(n)` for `(user_id, prefix)` over **all states** (burned
//!    rows count — this is the same rule reservation uses).
//! 3. Open the user's TC replica and snapshot all `Task` UUIDs +
//!    `entry` timestamps.
//! 4. Sort by `entry asc` with `uuid asc` tie-breaker (per
//!    `task-write-contract.md` deterministic ordering).
//! 5. Filter out tasks that already have a non-burned allocation row
//!    (defence-in-depth — fresh users won't have any). Any pre-existing
//!    `committed` row from prior provisioning is kept as-is.
//! 6. Compute anticipated `n_i = max + i` for each remaining task in
//!    sorted order.
//! 7. **Phase B**: build one `Operations` batch. For each task whose
//!    current `cmdock_key` UDA does not equal `<prefix>-<n_i>`, emit a
//!    `set_value` op. Commit once. Idempotent on retry — re-running
//!    skips tasks whose UDA already matches.
//! 8. **Phase A+C** (atomic in DB): one `BEGIN IMMEDIATE` transaction
//!    inserts every allocation row with `state='committed'` and updates
//!    `users.task_keys_migrated_at`. SQLite rolls back on any failure;
//!    a partial migration cannot be observed from outside.
//! 9. Mark the cache.
//!
//! Recovery from a crash before step 8 commits: the next call replays
//! steps 1–8. Phase B writes are idempotent (existing UDA values match
//! what we'd write); Phase A+C is fresh because no rows were committed.
//! Recovery from a crash after step 8 commits: the next call sees
//! `task_keys_migrated_at IS NOT NULL` and returns at the lock-acquire
//! re-check, marking the cache.

use std::sync::Arc;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use metrics::counter;
use taskchampion::{Operations, Replica, SqliteStorage, Task};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::task_keys::udas::{CMDOCK_KEY_UDA, CMDOCK_TASK_SCOPE_UDA};

fn stamp_task_scope_udas(
    task: &mut Task,
    prefix: &str,
    ops: &mut Operations,
    context: &str,
) -> anyhow::Result<bool> {
    let mut changed = false;
    if !matches!(task.get_value(CMDOCK_TASK_SCOPE_UDA), Some(existing) if existing == prefix) {
        task.set_value(CMDOCK_TASK_SCOPE_UDA, Some(prefix.to_string()), ops)
            .with_context(|| format!("set_value cmdock_task_scope {context}"))?;
        changed = true;
    }
    Ok(changed)
}

/// Run the Phase 4 backfill for `user_id` if it hasn't already completed.
/// Idempotent and safe to call from any handler entry point. The fast
/// path (cache hit) is a single `DashMap` read; the slow path acquires
/// the per-user mutation lock, double-checks the DB, and runs the
/// personal Task Scope migration before returning.
pub async fn ensure_user_task_keys_migrated(state: &AppState, user_id: &str) -> anyhow::Result<()> {
    if state.recovery_runtime.task_keys_migration_marked(user_id) {
        return Ok(());
    }

    // Cache miss: serialise on the per-user mutation lock so concurrent
    // first-accesses for the same user run the backfill exactly once.
    // Same lock as `service::add_task` and the reaper — no separate
    // migration lock per ADR-0002 §Independence.
    let lock = state.recovery_runtime.task_mutation_lock(user_id);
    let _guard = lock.lock().await;

    // Double-check under the lock — a concurrent first-access may have
    // raced past the cache-miss window and finished the backfill while
    // we were acquiring the lock.
    if let Some(_marker) = state
        .store
        .get_user_task_keys_migrated_at(user_id)
        .await
        .with_context(|| format!("get_user_task_keys_migrated_at for {user_id}"))?
    {
        // Upgrade path for pre-`cmdock_task_scope` deployments: allocation
        // rows and `cmdock_key` already exist, so Phase 4 must not allocate
        // new keys, but TC tasks still need the canonical Task Scope UDA
        // stamped from existing allocation prefixes. Non-fatal: if the
        // replica is unavailable (e.g. corrupted), skip silently so the
        // normal open_user_replica corruption/quarantine path can surface
        // the 503 to the caller.
        if let Err(e) = ensure_task_scope_udas_for_allocated_tasks_locked(state, user_id).await {
            tracing::warn!(
                error = %e,
                user_id = %user_id,
                "cmdock_task_scope UDA upgrade skipped (replica unavailable)"
            );
        }
        state
            .recovery_runtime
            .mark_task_keys_migration_complete(user_id);
        return Ok(());
    }

    backfill_user_locked(state, user_id).await?;

    state
        .recovery_runtime
        .mark_task_keys_migration_complete(user_id);
    Ok(())
}

async fn ensure_task_scope_udas_for_allocated_tasks_locked(
    state: &AppState,
    user_id: &str,
) -> anyhow::Result<()> {
    let rep_arc = state
        .replica_manager
        .get_replica(user_id)
        .await
        .with_context(|| format!("open replica for task-scope UDA upgrade {user_id}"))?;
    let mut rep = rep_arc.lock().await;
    let all_tasks = rep
        .all_tasks()
        .await
        .with_context(|| format!("all_tasks for task-scope UDA upgrade {user_id}"))?;
    if all_tasks.is_empty() {
        return Ok(());
    }

    let uuid_strs: Vec<String> = all_tasks.keys().map(ToString::to_string).collect();
    let keys = state
        .store
        .lookup_task_keys_by_uuids(user_id, &uuid_strs)
        .await
        .with_context(|| {
            format!("lookup_task_keys_by_uuids for task-scope UDA upgrade {user_id}")
        })?;

    let mut ops = Operations::new();
    let mut changed = 0usize;
    for (uuid, mut task) in all_tasks {
        let Some(key) = keys.get(&uuid.to_string()) else {
            continue;
        };
        let Some((prefix, _)) = key.rsplit_once('-') else {
            continue;
        };
        if stamp_task_scope_udas(
            &mut task,
            prefix,
            &mut ops,
            &format!("on {uuid} during task-scope UDA upgrade"),
        )? {
            changed += 1;
        }
    }

    if changed > 0 {
        rep.commit_operations(ops).await.with_context(|| {
            format!("commit_operations for task-scope UDA upgrade on user {user_id}")
        })?;
        tracing::info!(
            target: "audit",
            action = "task.key.task_scope_uda_upgrade_completed",
            source = "api",
            user_id = %user_id,
            tasks_upgraded = changed as u64,
        );
    }
    Ok(())
}

async fn backfill_user_locked(state: &AppState, user_id: &str) -> anyhow::Result<()> {
    let prefix = match state
        .store
        .get_user_prefix(user_id)
        .await
        .with_context(|| format!("get_user_prefix for {user_id}"))?
    {
        Some(p) => p,
        None => {
            // No prefix can mean two things:
            //   1. User was deleted under us while their token is still in
            //      the auth cache (stale-cache window, documented in the
            //      auth layer). The migration is a no-op — no replica to
            //      walk, no rows to allocate. Silently complete so the
            //      handler proceeds; auth cache TTL closes the window.
            //   2. A genuine Phase 1 regression — a user record exists
            //      but never received a prefix. This indicates a bug in
            //      whatever path created the user; surface it as an
            //      error so the operator can investigate.
            // Distinguish via `get_user_by_id`. (Per server#137 follow-
            // up after Phase 6 staging qualification.)
            let exists = state
                .store
                .get_user_by_id(user_id)
                .await
                .with_context(|| format!("get_user_by_id for {user_id}"))?
                .is_some();
            if !exists {
                return Ok(());
            }
            anyhow::bail!(
                "user {user_id} has no prefix — Phase 1 backfill never assigned one; \
                 cannot run task-key backfill"
            );
        }
    };

    counter!("task_keys_migration_started_total").increment(1);
    tracing::info!(
        target: "audit",
        action = "task.key.migration_started",
        source = "api",
        user_id = %user_id,
        prefix = %prefix,
    );

    let rep_arc = state
        .replica_manager
        .get_replica(user_id)
        .await
        .with_context(|| format!("open replica for {user_id}"))?;

    // Reconcile any pending+attached allocation rows BEFORE collecting
    // candidates. A pending row from a previously crashed `add_task` (or
    // an in-flight one whose lock we now hold) carries a `task_uuid` that
    // would collide with the atomic Phase A+C insert via the partial
    // unique index `idx_task_key_allocations_uuid`. Under the per-user
    // mutation lock no other actor can be racing these rows, so we
    // finalise them in place: write/check the canonical UDA, then call
    // `commit_task_key` with the captured `attempt_id`.
    reconcile_pending_attached_rows(state, user_id, &rep_arc).await?;

    let candidates = collect_backfill_candidates(state, user_id, &rep_arc).await?;

    if candidates.is_empty() {
        // No tasks to migrate (fresh user, or all tasks already have a
        // committed allocation row from prior provisioning). Still mark
        // the column so the fast-path cache stops missing.
        state
            .store
            .mark_user_task_keys_migrated(user_id)
            .await
            .with_context(|| format!("mark_user_task_keys_migrated for {user_id} (empty)"))?;
        counter!("task_keys_migration_completed_total").increment(1);
        tracing::info!(
            target: "audit",
            action = "task.key.migration_completed",
            source = "api",
            user_id = %user_id,
            prefix = %prefix,
            tasks_migrated = 0u64,
            recovery = false,
        );
        return Ok(());
    }

    let max_existing_n = state
        .store
        .max_n_for_user_prefix(user_id, &prefix)
        .await
        .with_context(|| format!("max_n_for_user_prefix for {user_id}/{prefix}"))?;

    // Phase B: write `cmdock_key` UDAs in TC. Idempotent — a second run
    // observes the matching UDA value and skips the set_value op. If we
    // observe any pre-existing UDA matching what we would write, that
    // is recovery from a prior crashed backfill. If we observe a
    // pre-existing UDA whose value disagrees with the canonical key the
    // backfill would assign, the task is a Phase 5e orphan: the foreign
    // value is overwritten with fresh-N (per contract § Orphan
    // reconciliation — "burned numbers never re-allocate" trumps
    // adoption-of-foreign-N).
    let phase_b =
        write_cmdock_key_udas(user_id, &rep_arc, &prefix, max_existing_n, &candidates).await?;

    if phase_b.matched_recovery {
        counter!("task_keys_migration_recovery_total", "kind" => "phase_b_uda").increment(1);
        tracing::info!(
            target: "audit",
            action = "task.key.migration_recovery",
            source = "api",
            kind = "phase_b_uda",
            user_id = %user_id,
            prefix = %prefix,
            candidates = candidates.len() as u64,
        );
    }

    // Phase A+C: insert allocation rows + mark migrated_at, atomic.
    // Pass `expected_max_n` so the commit transaction can re-verify
    // MAX(n) hasn't shifted between our Phase B precompute and the
    // commit — single-process deployments under the mutation lock can't
    // trip this, but a multi-process / admin-restore race would, and we
    // want it to surface as an explicit error rather than silently
    // committing rows whose `n` disagrees with the UDAs Phase B wrote.
    let task_uuids: Vec<String> = candidates.iter().map(|c| c.uuid.to_string()).collect();
    state
        .store
        .commit_backfill_allocations_for_user(user_id, &prefix, max_existing_n, &task_uuids)
        .await
        .with_context(|| format!("commit_backfill_allocations_for_user for {user_id}"))?;

    counter!("task_keys_migration_completed_total").increment(1);
    counter!("task_keys_allocated_total").increment(candidates.len() as u64);

    // Audit-after-success for orphan reconciliation: the canonical UDA
    // overwrite committed in Phase B AND the allocation rows committed
    // in Phase A+C — both succeeded — so the audit log can honestly
    // claim the orphans were reconciled. Per CLAUDE.md § Audit-after-
    // success pattern; mirrors `reconcile_drift`.
    if !phase_b.orphan_reconciliations.is_empty() {
        counter!("task_keys_orphans_reconciled_total")
            .increment(phase_b.orphan_reconciliations.len() as u64);
        for orphan in &phase_b.orphan_reconciliations {
            tracing::info!(
                target: "audit",
                action = "task.key.migration_recovery",
                source = "api",
                kind = "orphan_reconciled",
                user_id = %user_id,
                prefix = %prefix,
                task_uuid = %orphan.task_uuid,
                canonical_key = %orphan.canonical_key,
                foreign_value = %orphan.foreign_value,
            );
        }
    }

    tracing::info!(
        target: "audit",
        action = "task.key.migration_completed",
        source = "api",
        user_id = %user_id,
        prefix = %prefix,
        tasks_migrated = candidates.len() as u64,
        recovery = phase_b.matched_recovery,
        orphans_reconciled = phase_b.orphan_reconciliations.len() as u64,
    );

    Ok(())
}

#[derive(Debug, Clone)]
struct BackfillCandidate {
    uuid: Uuid,
    entry: Option<DateTime<Utc>>,
}

async fn collect_backfill_candidates(
    state: &AppState,
    user_id: &str,
    rep_arc: &Arc<Mutex<Replica<SqliteStorage>>>,
) -> anyhow::Result<Vec<BackfillCandidate>> {
    let all = {
        let mut rep = rep_arc.lock().await;
        rep.all_tasks()
            .await
            .with_context(|| format!("all_tasks for {user_id}"))?
    };

    // Skip task UUIDs that already have a committed allocation row.
    // `reconcile_pending_attached_rows` ran first under the per-user
    // mutation lock, so at this point every formerly-pending row is
    // either `committed` (and visible here) or surfaced as an explicit
    // error. Burned rows MUST NOT skip — their `task_uuid` is detached
    // by `burn_task_key` (since iter2 of #130) so the partial unique
    // index `idx_task_key_allocations_uuid` does not block the atomic
    // Phase A+C insert. Migration 028 brings legacy burned rows up to
    // the same shape on upgrade.
    let uuids: Vec<String> = all.keys().map(|u| u.to_string()).collect();
    let already_keyed = state
        .store
        .lookup_task_keys_by_uuids(user_id, &uuids)
        .await
        .with_context(|| format!("lookup_task_keys_by_uuids for {user_id}"))?;

    let mut candidates: Vec<BackfillCandidate> = all
        .into_iter()
        .filter_map(|(uuid, task)| {
            if already_keyed.contains_key(&uuid.to_string()) {
                None
            } else {
                Some(BackfillCandidate {
                    uuid,
                    entry: task.get_entry(),
                })
            }
        })
        .collect();

    // Sort: entry asc, then UUID asc (canonical lowercase-hyphenated
    // string form) per `task-write-contract.md` § Deterministic ordering.
    // Tasks without `entry` sort first via `Option::None < Some(_)` —
    // unusual but consistent.
    candidates.sort_by(|a, b| a.entry.cmp(&b.entry).then_with(|| a.uuid.cmp(&b.uuid)));

    Ok(candidates)
}

/// Per-task record describing an orphan reconciliation: a candidate
/// task that arrived at backfill carrying a `cmdock_key` UDA whose
/// value disagrees with the canonical key the backfill would assign.
/// Captured during the classifier loop, audited only after both Phase
/// B (UDA commit) and Phase A+C (allocation row commit) succeed.
#[derive(Debug, Clone)]
struct OrphanReconciliation {
    task_uuid: String,
    canonical_key: String,
    foreign_value: String,
}

/// Outcome of `write_cmdock_key_udas`. Three classification axes:
///
/// - **Matched-recovery** (`matched_recovery`): at least one candidate
///   already carried the canonical UDA value. Indicates a prior crashed
///   Phase B that committed the UDA but not the allocation rows;
///   re-running stamps the rows.
/// - **Empty path**: candidate had no `cmdock_key` UDA. Steady-state
///   first-time backfill case — no orphan, no recovery, just allocate.
/// - **Orphan-reconciliation** (`orphan_reconciliations`): candidate
///   carried a `cmdock_key` UDA whose value disagrees with the
///   canonical key. Phase B overwrites with fresh-N; Phase A+C inserts
///   the new allocation row. Audit + counter emit AFTER Phase A+C
///   succeeds.
#[derive(Debug, Default)]
struct WriteUdaOutcome {
    matched_recovery: bool,
    orphan_reconciliations: Vec<OrphanReconciliation>,
}

async fn write_cmdock_key_udas(
    user_id: &str,
    rep_arc: &Arc<Mutex<Replica<SqliteStorage>>>,
    prefix: &str,
    max_existing_n: i64,
    candidates: &[BackfillCandidate],
) -> anyhow::Result<WriteUdaOutcome> {
    let mut rep = rep_arc.lock().await;
    let mut ops = Operations::new();
    let mut outcome = WriteUdaOutcome::default();
    let mut emitted_count: usize = 0;

    for (i, candidate) in candidates.iter().enumerate() {
        let n = max_existing_n + (i as i64) + 1;
        let canonical_key = format!("{prefix}-{n}");

        let mut task = rep
            .get_task(candidate.uuid)
            .await
            .with_context(|| format!("get_task {} for backfill", candidate.uuid))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "task {} disappeared between all_tasks and get_task during backfill",
                    candidate.uuid
                )
            })?;

        // CONTRACT (#141/S4): whenever this backfill path writes or
        // repairs `cmdock_key`, the legacy `cmdock_account`
        // compatibility UDA is written in the same Operations batch. Its
        // value is the Task Scope key prefix used for the allocation
        // rows created by this pass, not tenant account identity.
        let current = task.get_value(CMDOCK_KEY_UDA).map(|s| s.to_string());
        match current {
            Some(ref existing) if existing == &canonical_key => {
                // A previous crashed backfill already wrote this UDA.
                // Phase A+C never ran (it's atomic and migrated_at was
                // NULL), so we'll re-stamp the allocation rows in this
                // run; the UDA stays as-is.
                outcome.matched_recovery = true;
                if stamp_task_scope_udas(
                    &mut task,
                    prefix,
                    &mut ops,
                    &format!("on {} during backfill recovery", candidate.uuid),
                )? {
                    emitted_count += 1;
                }
            }
            Some(other) => {
                // Foreign UDA — Phase 5e orphan. The contract requires
                // fresh-N here (`task-write-contract.md` § Orphan
                // reconciliation): never adopt the foreign value, even
                // if the encoded N is unallocated. Emit the canonical
                // value, recording the orphan for audit-after-success.
                task.set_value(CMDOCK_KEY_UDA, Some(canonical_key.clone()), &mut ops)
                    .with_context(|| {
                        format!(
                            "set_value cmdock_key on {} during orphan reconciliation",
                            candidate.uuid
                        )
                    })?;
                stamp_task_scope_udas(
                    &mut task,
                    prefix,
                    &mut ops,
                    &format!("on {} during orphan reconciliation", candidate.uuid),
                )?;
                emitted_count += 1;
                outcome.orphan_reconciliations.push(OrphanReconciliation {
                    task_uuid: candidate.uuid.to_string(),
                    canonical_key: canonical_key.clone(),
                    foreign_value: other,
                });
            }
            None => {
                // Empty path — task had no `cmdock_key` UDA. Steady-
                // state first-time backfill case.
                task.set_value(CMDOCK_KEY_UDA, Some(canonical_key.clone()), &mut ops)
                    .with_context(|| {
                        format!("set_value cmdock_key on {} during backfill", candidate.uuid)
                    })?;
                stamp_task_scope_udas(
                    &mut task,
                    prefix,
                    &mut ops,
                    &format!("on {} during backfill", candidate.uuid),
                )?;
                emitted_count += 1;
            }
        }
    }

    if emitted_count > 0 {
        rep.commit_operations(ops).await.with_context(|| {
            format!("commit_operations for cmdock_key UDA backfill batch on user {user_id}")
        })?;
    }

    Ok(outcome)
}

/// Finalise any `state='pending'` allocation rows whose `task_uuid` is
/// already attached, BEFORE collecting backfill candidates. A pending +
/// attached row is a previously crashed (or in-flight under our lock)
/// `service::add_task` reservation; its `task_uuid` would collide with
/// the atomic Phase A+C `INSERT` via `idx_task_key_allocations_uuid`. We
/// hold the per-user mutation lock, so no other actor is racing these
/// rows. Per-row policy:
///
///   - **TC task missing** → burn the row. The UUID was rolled back at
///     the TC layer (the regular create commits TC + sets the UDA in a
///     single `Operations` batch), so the allocation row is orphaned.
///     Auto-burn matches the reaper's "no UDA match" branch, and the
///     burn detaches `task_uuid` so the index slot is freed.
///   - **UDA matches canonical** → finalise (`commit_task_key`). No TC
///     write needed.
///   - **UDA missing on TC task** → write the canonical UDA + finalise.
///   - **UDA mismatches** → bail with an error. Auto-overwriting could
///     mask data loss; operator review is the safer policy. The mutation
///     lock keeps the offending row pending until a human investigates.
async fn reconcile_pending_attached_rows(
    state: &AppState,
    user_id: &str,
    rep_arc: &Arc<Mutex<Replica<SqliteStorage>>>,
) -> anyhow::Result<()> {
    let pending = state
        .store
        .list_pending_attached_task_keys_for_user(user_id)
        .await
        .with_context(|| format!("list_pending_attached_task_keys_for_user for {user_id}"))?;

    if pending.is_empty() {
        return Ok(());
    }

    enum Decision {
        Finalise,
        Burn,
    }
    let mut decisions: Vec<Decision> = Vec::with_capacity(pending.len());

    {
        let mut rep = rep_arc.lock().await;
        let mut ops = Operations::new();
        let mut emitted = false;
        for row in &pending {
            let task_uuid = uuid::Uuid::parse_str(&row.task_uuid).map_err(|e| {
                anyhow::anyhow!(
                    "pending allocation row {prefix}-{n} has malformed task_uuid {raw}: {e}",
                    prefix = row.prefix,
                    n = row.n,
                    raw = row.task_uuid,
                )
            })?;
            let canonical_key = format!("{}-{}", row.prefix, row.n);
            let task_opt = rep
                .get_task(task_uuid)
                .await
                .with_context(|| format!("get_task {task_uuid} for backfill reconcile"))?;
            match task_opt {
                None => {
                    // TC has no record of this task — the row is orphaned.
                    decisions.push(Decision::Burn);
                }
                Some(mut task) => {
                    let current = task.get_value(CMDOCK_KEY_UDA).map(|s| s.to_string());
                    match current {
                        Some(ref existing) if existing == &canonical_key => {
                            if stamp_task_scope_udas(
                                &mut task,
                                &row.prefix,
                                &mut ops,
                                &format!("on {task_uuid} during reconcile"),
                            )? {
                                emitted = true;
                            }
                            decisions.push(Decision::Finalise);
                        }
                        None => {
                            task.set_value(CMDOCK_KEY_UDA, Some(canonical_key.clone()), &mut ops)
                                .with_context(|| {
                                    format!("set_value cmdock_key on {task_uuid} during reconcile")
                                })?;
                            stamp_task_scope_udas(
                                &mut task,
                                &row.prefix,
                                &mut ops,
                                &format!("on {task_uuid} during reconcile"),
                            )?;
                            emitted = true;
                            decisions.push(Decision::Finalise);
                        }
                        Some(other) => {
                            anyhow::bail!(
                                "pending allocation row {canonical_key} references task_uuid \
                                 {task_uuid} whose cmdock_key UDA is {other:?} (mismatch); \
                                 operator review required",
                            );
                        }
                    }
                }
            }
        }
        if emitted {
            rep.commit_operations(ops).await.with_context(|| {
                format!("commit_operations for backfill reconcile on user {user_id}")
            })?;
        }
    }

    let mut finalised: u64 = 0;
    let mut orphan_burned: u64 = 0;
    for (row, decision) in pending.iter().zip(decisions.iter()) {
        match decision {
            Decision::Finalise => {
                state
                    .store
                    .commit_task_key(user_id, &row.prefix, row.n, &row.attempt_id)
                    .await
                    .with_context(|| {
                        format!(
                            "commit_task_key {prefix}-{n} during backfill reconcile",
                            prefix = row.prefix,
                            n = row.n,
                        )
                    })?;
                finalised += 1;
            }
            Decision::Burn => {
                state
                    .store
                    .burn_task_key(user_id, &row.prefix, row.n, &row.attempt_id)
                    .await
                    .with_context(|| {
                        format!(
                            "burn_task_key {prefix}-{n} during backfill reconcile (orphan)",
                            prefix = row.prefix,
                            n = row.n,
                        )
                    })?;
                counter!("task_keys_burned_total", "reason" => "backfill_reconcile_orphan")
                    .increment(1);
                orphan_burned += 1;
            }
        }
    }

    counter!("task_keys_migration_recovery_total", "kind" => "pending_attached").increment(1);
    tracing::info!(
        target: "audit",
        action = "task.key.migration_recovery",
        source = "api",
        kind = "pending_attached",
        user_id = %user_id,
        finalised = finalised,
        orphan_burned = orphan_burned,
    );

    Ok(())
}
