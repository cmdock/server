//! Task-key allocation reaper. Owns the AppState-coupled half of stale
//! pending-row cleanup; the DB primitive `select_stale_pending_task_keys`
//! lives in `src/store/sqlite/task_keys.rs`.
//!
//! ## Procedure
//!
//! 1. **Short DB read (no lock):** select stale `pending` rows older than
//!    `pending_timeout_seconds`, ordered by user_id, capped at
//!    `batch_limit` (default 1000).
//! 2. **Group by user_id.** For each user batch:
//!    - Acquire the per-user mutation lock via
//!      `RuntimeRecoveryCoordinator::task_mutation_lock`. Bounded wait
//!      (1s); skip user this pass on contention so live mutations aren't
//!      blocked.
//!    - If any candidate has `task_uuid IS NOT NULL`, also acquire the
//!      per-user canonical-replica lock (`replica_arc.lock()`) and hold
//!      it across the entire candidate loop. This is the lock-discipline
//!      foundation for Phase 5c's burn-with-UDA-clear path: the reverse
//!      `cmdock_key` UDA op MUST commit under the same replica lock as
//!      the config-DB burn so a concurrent TC sync/read cannot observe
//!      "DB burned but TC still carries the UDA."
//!    - For each candidate row:
//!      * `task_uuid IS NULL` — reservation never reached
//!        `attach_task_uuid_to_pending`, so no in-flight create is racing.
//!        Safe to burn (transition `pending → burned`).
//!      * `task_uuid IS NOT NULL` — Phase 2 ambiguous-recovery
//!        candidate. Open the user's replica ONCE per batch and build
//!        a `TcIndex` that records every present task UUID and the
//!        `cmdock_key` UDA value for tasks that have one. The
//!        per-candidate decision is three-way: TC missing the task →
//!        burn (rolled back); TC has the task with no `cmdock_key`
//!        UDA → burn (orphan); TC has the task with `cmdock_key`
//!        matching `<PREFIX>-<n>` → finalise via `commit_task_key`
//!        (idempotent on already-committed-with-same-attempt); TC has
//!        the task with a *different* `cmdock_key` UDA → skip, leave
//!        pending, audit `task.key.reaper_uda_mismatch` for operator
//!        review (auto-burning would defeat Phase 4's
//!        `reconcile_pending_attached_rows` mismatch-bail policy — the
//!        next backfill would happily overwrite the wrong UDA).
//!        Indexing by UUID rather than UDA value protects against
//!        duplicate-UDA-value collisions in the scan map (`all_tasks()`
//!        iteration order is not a stable contract).
//!      * **Phase 5c — Finalise → `commit_task_key` fails:** if
//!        `commit_task_key` returns Err on a Finalise-classified row
//!        (rare; transient DB error or constraint), escalate to
//!        burn-with-UDA-clear: emit a reverse `cmdock_key` UDA op
//!        clearing the value, commit it under the replica lock we
//!        already hold, then call `burn_task_key`. Both writes occur
//!        within the same replica-lock acquisition so REST/sync readers
//!        cannot observe an intermediate state.
//!    - Drop the locks (replica then mutation) and sleep
//!      `INTER_USER_SLEEP` before the next user.
//! 3. Return outcome counts for metrics.
//!
//! ## Lock-order invariant
//!
//! Mutation handlers acquire the per-user mutation lock BEFORE any
//! `task_key_allocations` DB call, then acquire the per-user replica
//! lock for TC ops (Phase 2 wires this in `service::add_task`). The
//! reaper follows the same order: mutation lock first, then replica
//! lock (for uuid-attached batches), then issue
//! `commit_task_key` / `burn_task_key` and any reverse-UDA ops. Symmetric
//! ordering eliminates deadlock between mutations and reaper.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use metrics::{counter, histogram};
use taskchampion::{Operations, Replica, SqliteStorage};
use tokio::sync::MutexGuard;
use tokio::time::Instant;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::store::models::StalePendingCandidate;
use crate::task_keys::udas::CMDOCK_KEY_UDA;
use crate::user_runtime::open_user_replica;

const BATCH_LIMIT: usize = 1000;
const PER_USER_LOCK_TIMEOUT: Duration = Duration::from_secs(1);
const INTER_USER_SLEEP: Duration = Duration::from_millis(50);

/// Outcome of a single reaper pass. Values are reported via metrics and
/// returned for caller-side logging / test assertions.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReaperOutcome {
    pub burned: u64,
    /// Rows finalised by the reaper (pending → committed) on TC-scan
    /// UUID + UDA match.
    pub finalised: u64,
    /// Effectively dead post-Phase 2 — uuid-attached rows now drive the
    /// TC-scan path (finalise-or-burn), not skip. Field retained for
    /// metrics/structural compatibility; expected to remain 0.
    pub skipped_uuid_attached: u64,
    /// Rows skipped because the per-user mutation lock was contended.
    pub skipped_lock_busy: u64,
    /// Rows left pending because the TC task exists and has a
    /// `cmdock_key` UDA that does NOT match the allocation row's
    /// canonical key. Auto-burning would defeat Phase 4's
    /// `reconcile_pending_attached_rows` mismatch-bail policy
    /// (overwriting the wrong UDA with a fresh allocation), so the
    /// reaper now leaves these rows pending and audits the mismatch
    /// for operator review.
    pub skipped_uda_mismatch: u64,
    /// Phase 5c — count of Finalise-classified rows whose
    /// `commit_task_key` retry failed and were escalated to
    /// burn-with-UDA-clear. Incremented BEFORE the burn is attempted,
    /// so this counter may exceed the eventual `burned` increment if
    /// the follow-on `burn_task_key` itself errors (rare; the row stays
    /// pending and is retried on the next pass).
    pub phase3_retry_failed: u64,
    /// Phase 5c — count of rows whose burn path emitted a reverse
    /// `cmdock_key` UDA op (cleared the canonical UDA). Strict subset
    /// of `burned`; today only the Phase 3 retry failure path
    /// triggers this, but reserved for future burn paths that may
    /// also need UDA clearing.
    pub uda_cleared: u64,
}

/// Run one reaper pass. Wired into the existing reaper loop in
/// `src/idempotency.rs::prune_once`.
pub async fn run_reaper_pass(state: &AppState) -> ReaperOutcome {
    let pass_start = Instant::now();
    let now = chrono::Utc::now().timestamp();
    let pending_timeout = state.config.task_write.idempotency_pending_timeout_seconds;

    let candidates: Vec<StalePendingCandidate> = match state
        .store
        .select_stale_pending_task_keys(now, pending_timeout, BATCH_LIMIT)
        .await
    {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "task-keys reaper: failed to select stale pending candidates",
            );
            return ReaperOutcome::default();
        }
    };

    if candidates.is_empty() {
        histogram!("task_keys_reaper_pass_seconds").record(pass_start.elapsed().as_secs_f64());
        return ReaperOutcome::default();
    }

    let mut grouped: BTreeMap<String, Vec<StalePendingCandidate>> = BTreeMap::new();
    for c in candidates {
        grouped.entry(c.user_id.clone()).or_default().push(c);
    }

    let mut outcome = ReaperOutcome::default();

    for (user_id, user_batch) in grouped {
        process_user_batch(state, &user_id, user_batch, &mut outcome).await;
        tokio::time::sleep(INTER_USER_SLEEP).await;
    }

    if outcome.finalised > 0 {
        counter!("task_keys_reaper_finalised_total").increment(outcome.finalised);
    }

    histogram!("task_keys_reaper_pass_seconds").record(pass_start.elapsed().as_secs_f64());

    if outcome.burned > 0
        || outcome.finalised > 0
        || outcome.skipped_lock_busy > 0
        || outcome.skipped_uda_mismatch > 0
        || outcome.phase3_retry_failed > 0
    {
        tracing::info!(
            target: "audit",
            action = "task.key.reaper_pass",
            source = "system",
            burned = outcome.burned,
            finalised = outcome.finalised,
            skipped_uuid_attached = outcome.skipped_uuid_attached,
            skipped_lock_busy = outcome.skipped_lock_busy,
            skipped_uda_mismatch = outcome.skipped_uda_mismatch,
            phase3_retry_failed = outcome.phase3_retry_failed,
            uda_cleared = outcome.uda_cleared,
        );
    }

    outcome
}

/// Process one user's batch under the per-user mutation lock (and the
/// replica lock if any candidate is uuid-attached).
async fn process_user_batch(
    state: &AppState,
    user_id: &str,
    user_batch: Vec<StalePendingCandidate>,
    outcome: &mut ReaperOutcome,
) {
    let lock_arc: Arc<tokio::sync::Mutex<()>> = state.recovery_runtime.task_mutation_lock(user_id);
    let acquire_start = Instant::now();
    let guard_result = tokio::time::timeout(PER_USER_LOCK_TIMEOUT, lock_arc.lock()).await;
    histogram!("task_keys_reaper_lock_acquire_seconds")
        .record(acquire_start.elapsed().as_secs_f64());

    let _mut_guard = match guard_result {
        Ok(g) => g,
        Err(_) => {
            outcome.skipped_lock_busy += user_batch.len() as u64;
            tracing::debug!(
                user_id = %user_id,
                candidates = user_batch.len(),
                "task-keys reaper: lock contended, skipping user this pass",
            );
            return;
        }
    };

    let needs_tc_scan = user_batch.iter().any(|c| c.task_uuid.is_some());

    if !needs_tc_scan {
        // No uuid-attached rows; no replica access required.
        for candidate in user_batch {
            burn_plain(state, &candidate, outcome).await;
        }
        return;
    }

    // Acquire the canonical-replica lock and hold it across the entire
    // candidate loop. This is the load-bearing change for Phase 5c —
    // burn-with-UDA-clear emits a reverse-UDA op AND a config-DB burn
    // under the same lock acquisition so a concurrent TC sync/read
    // cannot observe an intermediate state.
    // Bound BOTH `open_user_replica` and `rep_arc.lock()` under the
    // same `PER_USER_LOCK_TIMEOUT` budget as the mutation-lock acquire
    // above. `open_user_replica` is itself contended on the
    // ReplicaManager's per-user "opening" lock; without a timeout, a
    // cold-open path that's blocked behind another caller would stall
    // the reaper while it still holds the mutation lock, in turn
    // blocking every live mutation for that user. The lock acquire on
    // the returned Arc is the cached-warm contention path. Both must
    // be bounded to honour the "skip user this pass on contention"
    // guarantee.
    let rep_arc = match tokio::time::timeout(
        PER_USER_LOCK_TIMEOUT,
        open_user_replica(state, user_id, "system"),
    )
    .await
    {
        Ok(Ok(a)) => a,
        Ok(Err(status)) => {
            tracing::warn!(
                user_id = %user_id,
                status = %status.as_u16(),
                "task-keys reaper: open_user_replica failed; deferring user to next pass",
            );
            return;
        }
        Err(_) => {
            outcome.skipped_lock_busy += user_batch.len() as u64;
            counter!("task_keys_reaper_skipped_total", "reason" => "replica_open_busy")
                .increment(user_batch.len() as u64);
            tracing::debug!(
                user_id = %user_id,
                candidates = user_batch.len(),
                "task-keys reaper: replica open contended, skipping user this pass",
            );
            return;
        }
    };
    let mut rep_guard = match tokio::time::timeout(PER_USER_LOCK_TIMEOUT, rep_arc.lock()).await {
        Ok(g) => g,
        Err(_) => {
            outcome.skipped_lock_busy += user_batch.len() as u64;
            counter!("task_keys_reaper_skipped_total", "reason" => "replica_busy")
                .increment(user_batch.len() as u64);
            tracing::debug!(
                user_id = %user_id,
                candidates = user_batch.len(),
                "task-keys reaper: replica lock contended, skipping user this pass",
            );
            return;
        }
    };

    let tc_index = match build_tc_index_from_replica(&mut rep_guard).await {
        Ok(idx) => idx,
        Err(err) => {
            tracing::warn!(
                user_id = %user_id,
                error = %err,
                "task-keys reaper: build_tc_index_from_replica failed; \
                 deferring user to next pass",
            );
            return;
        }
    };

    process_uuid_attached_batch(
        state,
        user_id,
        user_batch,
        &tc_index,
        &mut rep_guard,
        outcome,
    )
    .await;
}

/// Per-row handler for a batch where the replica lock is held. Walks
/// each candidate, classifies against `tc_index`, and applies the
/// finalise / burn / burn-with-UDA-clear / skip decision.
async fn process_uuid_attached_batch(
    state: &AppState,
    _user_id: &str,
    user_batch: Vec<StalePendingCandidate>,
    tc_index: &TcIndex,
    rep: &mut MutexGuard<'_, Replica<SqliteStorage>>,
    outcome: &mut ReaperOutcome,
) {
    for candidate in user_batch {
        let candidate_label = format!("{}-{}", candidate.prefix, candidate.n);

        let decision: ReaperDecision = match &candidate.task_uuid {
            Some(uuid_str) => match Uuid::parse_str(uuid_str) {
                Ok(parsed_uuid) => tc_index.classify(&parsed_uuid, &candidate_label),
                Err(_) => {
                    tracing::warn!(
                        user_id = %candidate.user_id,
                        task_uuid = %uuid_str,
                        "task-keys reaper: candidate task_uuid is not a valid UUID; \
                         falling back to burn",
                    );
                    ReaperDecision::Burn
                }
            },
            None => ReaperDecision::Burn,
        };

        match decision {
            ReaperDecision::Finalise => {
                handle_finalise(state, &candidate, rep, outcome).await;
            }
            ReaperDecision::SkipUdaMismatch { observed } => {
                outcome.skipped_uda_mismatch += 1;
                counter!("task_keys_reaper_skipped_total", "reason" => "uda_mismatch").increment(1);
                tracing::warn!(
                    target: "audit",
                    action = "task.key.reaper_uda_mismatch",
                    source = "system",
                    user_id = %candidate.user_id,
                    prefix = %candidate.prefix,
                    n = candidate.n,
                    task_uuid = candidate.task_uuid.as_deref().unwrap_or(""),
                    expected = %candidate_label,
                    observed = %observed,
                    "task-keys reaper: uuid-attached row's TC cmdock_key UDA does not \
                     match canonical; row stays pending for operator review",
                );
            }
            ReaperDecision::Burn => {
                burn_plain(state, &candidate, outcome).await;
            }
        }
    }
}

/// Finalise path. On `commit_task_key` success: row → committed, audit
/// `task.key.reaper_phase3_retry_succeeded`. On failure: escalate to
/// burn-with-UDA-clear (emit reverse `cmdock_key` op + commit under the
/// already-held replica lock, then `burn_task_key`).
async fn handle_finalise(
    state: &AppState,
    candidate: &StalePendingCandidate,
    rep: &mut MutexGuard<'_, Replica<SqliteStorage>>,
    outcome: &mut ReaperOutcome,
) {
    let result = state
        .store
        .commit_task_key(
            &candidate.user_id,
            &candidate.prefix,
            candidate.n,
            &candidate.attempt_id,
        )
        .await;
    match result {
        Ok(()) => {
            outcome.finalised += 1;
            counter!(
                "task_keys_reaper_phase3_retried_total",
                "outcome" => "succeeded"
            )
            .increment(1);
            tracing::info!(
                target: "audit",
                action = "task.key.reaper_phase3_retry_succeeded",
                source = "system",
                user_id = %candidate.user_id,
                prefix = %candidate.prefix,
                n = candidate.n,
                "task-keys reaper: Phase 3 retry succeeded; row finalised",
            );
        }
        Err(err) => {
            counter!(
                "task_keys_reaper_phase3_retried_total",
                "outcome" => "failed"
            )
            .increment(1);
            tracing::warn!(
                user_id = %candidate.user_id,
                prefix = %candidate.prefix,
                n = candidate.n,
                error = %err,
                "task-keys reaper: commit_task_key failed; \
                 escalating to burn-with-UDA-clear",
            );
            outcome.phase3_retry_failed += 1;
            burn_with_uda_clear(state, candidate, rep, outcome).await;
        }
    }
}

/// Burn a candidate with no associated TC UDA to clear. Emits no
/// canonical write; only transitions the config-DB row.
async fn burn_plain(
    state: &AppState,
    candidate: &StalePendingCandidate,
    outcome: &mut ReaperOutcome,
) {
    let burn_result = state
        .store
        .burn_task_key(
            &candidate.user_id,
            &candidate.prefix,
            candidate.n,
            &candidate.attempt_id,
        )
        .await;
    match burn_result {
        Ok(()) => {
            outcome.burned += 1;
            counter!("task_keys_burned_total", "reason" => "reaper").increment(1);
        }
        Err(err) => {
            tracing::warn!(
                user_id = %candidate.user_id,
                prefix = %candidate.prefix,
                n = candidate.n,
                error = %err,
                "task-keys reaper: burn failed",
            );
        }
    }
}

/// Phase 5c — burn the row AND emit a reverse `cmdock_key` UDA op
/// clearing the value on the canonical replica. Ordering: emit + commit
/// the reverse-UDA op FIRST (under the replica lock the caller already
/// holds), then call `burn_task_key` against the config DB. If the
/// reverse-UDA commit fails, the burn is skipped — leaving the row
/// pending preserves the next-pass retry, and the canonical replica is
/// untouched.
///
/// Same-lock ordering bounds the REST/sync-observable window: a TC
/// sync pull arriving during this function must wait on the replica
/// lock and will see either the pre-burn state (UDA set, row pending)
/// or the post-burn state (UDA cleared, row burned), never the
/// half-applied "row burned, UDA still set" state.
async fn burn_with_uda_clear(
    state: &AppState,
    candidate: &StalePendingCandidate,
    rep: &mut MutexGuard<'_, Replica<SqliteStorage>>,
    outcome: &mut ReaperOutcome,
) {
    let Some(task_uuid_str) = candidate.task_uuid.as_deref() else {
        // Defensive — burn_with_uda_clear should only be reached on
        // uuid-attached rows.
        burn_plain(state, candidate, outcome).await;
        return;
    };
    let parsed_uuid = match Uuid::parse_str(task_uuid_str) {
        Ok(u) => u,
        Err(_) => {
            // task_uuid couldn't be parsed earlier classification too,
            // but defence-in-depth: burn DB without trying TC clear.
            burn_plain(state, candidate, outcome).await;
            return;
        }
    };

    let mut ops = Operations::new();
    let emit_result = emit_clear_cmdock_key(rep, parsed_uuid, &mut ops).await;
    let uda_cleared: bool = match emit_result {
        Ok(true) => {
            // We have a UDA-clear op queued. Commit it.
            if let Err(err) = rep.commit_operations(ops).await {
                tracing::warn!(
                    user_id = %candidate.user_id,
                    task_uuid = %task_uuid_str,
                    error = %err,
                    "task-keys reaper: reverse cmdock_key UDA commit failed; \
                     burn deferred to next pass",
                );
                return;
            }
            outcome.uda_cleared += 1;
            counter!("task_keys_reaper_uda_cleared_total").increment(1);
            true
        }
        Ok(false) => {
            // Task absent or UDA already absent. Defensive no-op path
            // — should be unreachable under current same-lock
            // discipline (the replica lock is held continuously from
            // `build_tc_index_from_replica` through this emit, so
            // nothing else can clear the UDA between classification
            // and op build). Kept for defence-in-depth: a future
            // refactor that releases-and-reacquires the lock between
            // those points would land here without crashing.
            false
        }
        Err(err) => {
            tracing::warn!(
                user_id = %candidate.user_id,
                task_uuid = %task_uuid_str,
                error = %err,
                "task-keys reaper: reverse cmdock_key UDA build failed; \
                 burn deferred to next pass",
            );
            return;
        }
    };

    let burn_result = state
        .store
        .burn_task_key(
            &candidate.user_id,
            &candidate.prefix,
            candidate.n,
            &candidate.attempt_id,
        )
        .await;
    match burn_result {
        Ok(()) => {
            outcome.burned += 1;
            counter!("task_keys_burned_total", "reason" => "reaper").increment(1);
            tracing::info!(
                target: "audit",
                action = "task.key.reaper_burn_with_uda_clear",
                source = "system",
                user_id = %candidate.user_id,
                prefix = %candidate.prefix,
                n = candidate.n,
                task_uuid = %task_uuid_str,
                uda_cleared = uda_cleared,
                "task-keys reaper: burned row (uda_cleared={})",
                uda_cleared,
            );
        }
        Err(err) => {
            // Burn failed AFTER the reverse-UDA op was committed.
            // Resulting transient state: row=pending, UDA=cleared
            // (the contract-forbidden "row=burned, UDA set" state is
            // explicitly avoided by the UDA-first ordering above).
            // The row will self-heal on the next reaper pass: TC scan
            // finds the task with no `cmdock_key`, classifies as Burn,
            // burn_plain transitions the row to burned. Audit / metric
            // here so operators can see the rare half-applied state.
            counter!(
                "task_keys_reaper_burn_after_uda_clear_failed_total",
                "uda_cleared" => if uda_cleared { "true" } else { "false" }
            )
            .increment(1);
            tracing::warn!(
                target: "audit",
                action = "task.key.reaper_burn_after_uda_clear_failed",
                source = "system",
                user_id = %candidate.user_id,
                prefix = %candidate.prefix,
                n = candidate.n,
                task_uuid = %task_uuid_str,
                uda_cleared = uda_cleared,
                error = %err,
                "task-keys reaper: burn failed after UDA clear; \
                 row stays pending and self-heals on next pass",
            );
        }
    }
}

/// Build a `cmdock_key=NULL` UDA-clear op for the given task. Returns
/// `Ok(true)` if an op was queued, `Ok(false)` if the task is absent or
/// the UDA is already absent (no-op), `Err` on TC error.
async fn emit_clear_cmdock_key(
    rep: &mut MutexGuard<'_, Replica<SqliteStorage>>,
    task_uuid: Uuid,
    ops: &mut Operations,
) -> anyhow::Result<bool> {
    let mut task = match rep.get_task(task_uuid).await? {
        Some(t) => t,
        None => return Ok(false),
    };
    if task.get_value(CMDOCK_KEY_UDA).is_none() {
        return Ok(false);
    }
    task.set_value(CMDOCK_KEY_UDA, None, ops)?;
    Ok(true)
}

/// Three-way classification of a uuid-attached pending row against
/// the user's current TC state. The reaper's per-row branch chooses one
/// per candidate.
#[derive(Debug, Clone)]
enum ReaperDecision {
    /// TC has the task and `cmdock_key` matches `<PREFIX>-N`.
    Finalise,
    /// Either the TC task is gone (rolled back) or it exists with no
    /// `cmdock_key` UDA (genuinely orphaned reservation). Burn detaches
    /// `task_uuid` so the slot frees up for re-allocation.
    Burn,
    /// TC has the task and `cmdock_key` is set, but disagrees with the
    /// allocation's canonical key. Leaves the row pending so an
    /// operator can investigate; matches Phase 4's
    /// `reconcile_pending_attached_rows` mismatch-bail policy. Carries
    /// the observed UDA value so the operator-facing audit line can
    /// surface the actual conflict.
    SkipUdaMismatch { observed: String },
}

/// Snapshot of the user's current TC tasks built by
/// `build_tc_index_from_replica`. Carries enough state to drive
/// the three-way `ReaperDecision` per candidate.
struct TcIndex {
    /// Every task UUID present in the user's TC replica.
    present_uuids: HashSet<Uuid>,
    /// Subset of `present_uuids` whose `cmdock_key` UDA is set,
    /// mapping to the UDA value.
    cmdock_keys: HashMap<Uuid, String>,
}

impl TcIndex {
    fn classify(&self, uuid: &Uuid, canonical: &str) -> ReaperDecision {
        if !self.present_uuids.contains(uuid) {
            return ReaperDecision::Burn;
        }
        match self.cmdock_keys.get(uuid) {
            None => ReaperDecision::Burn,
            Some(k) if k == canonical => ReaperDecision::Finalise,
            Some(other) => ReaperDecision::SkipUdaMismatch {
                observed: other.clone(),
            },
        }
    }
}

/// Scan all tasks once and build the `TcIndex`. Caller holds the
/// replica lock for the duration of the scan AND the candidate loop
/// that consumes the index — see the burn-with-UDA-clear same-lock
/// invariant in `run_reaper_pass`'s module docs.
async fn build_tc_index_from_replica(
    rep: &mut MutexGuard<'_, Replica<SqliteStorage>>,
) -> anyhow::Result<TcIndex> {
    let all = rep.all_tasks().await?;
    let mut present_uuids = HashSet::with_capacity(all.len());
    let mut cmdock_keys = HashMap::with_capacity(all.len());
    for (uuid, task) in &all {
        present_uuids.insert(*uuid);
        if let Some(key) = task.get_value(CMDOCK_KEY_UDA) {
            cmdock_keys.insert(*uuid, key.to_string());
        }
    }
    Ok(TcIndex {
        present_uuids,
        cmdock_keys,
    })
}
