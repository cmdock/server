//! Sync-bridge drift recovery (Phase 5b — `cmdock/server#130`).
//!
//! Approach B (post-canonical-apply read-back) per
//! `docs/internal/implementation/server-130-phase5-spike.md` and
//! `task-write-contract.md` § Drift recovery (commit `7516969`).
//!
//! # Invariant
//!
//! `reconcile_drift` MUST run while the same `replica_arc.lock()` is held
//! that wrapped `replica.sync(...)`. The hook lives inside the OS-thread
//! closure in `src/sync_bridge.rs::do_sync` so the lock is held across:
//!   1. `replica.sync(...)` (TC canonical apply)
//!   2. `reconcile_drift(...)` (this module — reverse-UDA emit)
//!
//! Releasing and reacquiring between (1) and (2) opens a REST-observable
//! window via the per-user replica mutex and is a violating pattern.
//!
//! # Decision table
//!
//! For each task in the canonical replica that carries a `cmdock_key` UDA:
//!
//! | Allocation row state    | UDA matches row?   | Action                                          | Audit kind             |
//! |-------------------------|--------------------|-------------------------------------------------|------------------------|
//! | committed               | matches            | no-op (scope/account drift observed only)       | —                      |
//! | committed               | differs            | emit reverse-UDA op restoring canonical         | `value_mismatch`       |
//! | pending                 | matches            | finalise: `commit_task_key(attempt_id)` (scope/account drift observed only) | `post_commit_finalize` |
//! | pending                 | differs            | finalise pending row + emit reverse-UDA op      | `pending_with_drift`   |
//! | no allocation row       | (n/a)              | SKIP (do NOT emit, do NOT audit)                | — (operational metric) |
//!
//! The `no-row` case is contract-mandated to skip both the reverse op
//! and the `task.key.drift_recovered` audit entry. Operational metric
//! `task_keys_drift_skipped_no_row_total` is the only signal — Phase 5e
//! orphan reconciliation handles the recovery.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use metrics::counter;
use taskchampion::storage::Storage;
use taskchampion::{Operations, Replica};
use uuid::Uuid;

use crate::store::models::{DriftAllocationRow, KeyState};
use crate::store::ConfigStore;
use crate::task_keys::udas::{CMDOCK_ACCOUNT_UDA, CMDOCK_KEY_UDA, CMDOCK_TASK_SCOPE_UDA};

/// Allocation prefix-N parsing — used for `commit_task_key` finalisation
/// where the store API takes `(prefix, n)` rather than the formatted key.
fn split_key(key: &str) -> Option<(&str, i64)> {
    let (prefix, rest) = key.rsplit_once('-')?;
    let n: i64 = rest.parse().ok()?;
    Some((prefix, n))
}

/// Outcome of a single drift-recovery pass for one user. Useful in tests
/// and telemetry — production callers can ignore.
#[derive(Debug, Default, Clone)]
pub struct DriftPassOutcome {
    pub value_mismatch: usize,
    pub post_commit_finalize: usize,
    pub pending_with_drift: usize,
    pub no_row_skipped: usize,
    /// `cmdock_key` matched the allocation row, but canonical
    /// `cmdock_task_scope` or deprecated `cmdock_account` was missing or
    /// different. The sync-bridge drift path intentionally observes but does
    /// not repair this prefix-only drift inline; the first-access upgrade /
    /// backfill path stamps both aliases from allocation rows.
    pub account_only_drift_observed: usize,
}

/// Walk `cmdock_key`-bearing tasks on the canonical replica, batch-look
/// up against `task_key_allocations`, and apply the decision table.
///
/// Caller must hold `replica` locked across `replica.sync(...)` AND
/// this call so REST readers cannot interleave between sync apply and
/// reverse-op commit (see § Invariant).
///
/// `audit_source` is folded into the structured `task.key.drift_recovered`
/// log lines (`"sync_bridge"` for scheduled / on-mutation runs;
/// `"add_version"` and `"add_snapshot"` for TC-handler-triggered runs are
/// reachable through the same hook because both flow through `do_sync`).
pub async fn reconcile_drift<S: Storage>(
    store: &Arc<dyn ConfigStore>,
    replica: &mut Replica<S>,
    user_id: &str,
    audit_source: &'static str,
) -> anyhow::Result<DriftPassOutcome> {
    let mut outcome = DriftPassOutcome::default();

    let uda_tasks = collect_cmdock_key_uda_tasks(replica)
        .await
        .with_context(|| format!("collect cmdock_key UDA tasks for {user_id}"))?;

    if uda_tasks.is_empty() {
        return Ok(outcome);
    }

    let task_uuids: Vec<String> = uda_tasks.keys().cloned().collect();
    let drift_rows = store
        .lookup_task_keys_for_drift(user_id, &task_uuids)
        .await
        .with_context(|| format!("lookup_task_keys_for_drift for {user_id}"))?;

    let by_uuid: HashMap<String, DriftAllocationRow> = drift_rows
        .into_iter()
        .map(|r| (r.task_uuid.clone(), r))
        .collect();

    // Two work lists, both emitted/applied AFTER the bulk reverse-op
    // commit succeeds:
    //   - `audit_after_commit` — emitted only on success so audit log
    //     never claims drift was recovered when the commit actually
    //     failed.
    //   - `pending_finalisations` — `commit_task_key` calls run after
    //     the reverse-op commit (different DB; not under replica lock).
    let mut audit_after_commit: Vec<DriftAuditRecord> = Vec::new();
    let mut pending_finalisations: Vec<PendingFinalisation> = Vec::new();
    let mut reverse_ops = Operations::new();
    let mut emitted_reverse_count: usize = 0;

    for (task_uuid, current_udas) in &uda_tasks {
        let current_uda = &current_udas.key;
        match by_uuid.get(task_uuid) {
            None => {
                outcome.no_row_skipped += 1;
                counter!("task_keys_drift_skipped_no_row_total").increment(1);
            }
            Some(row) => match (row.state, current_uda == &row.key) {
                (KeyState::Committed, true) => {
                    observe_account_only_drift(&mut outcome, current_udas, row);
                }
                (KeyState::Committed, false) => {
                    outcome.value_mismatch += 1;
                    emit_reverse_uda_op(
                        replica,
                        task_uuid,
                        &row.key,
                        &row.prefix,
                        &mut reverse_ops,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "emit reverse cmdock_key UDA op for {user_id}/{task_uuid} \
                             (canonical={canonical}, drift={current_uda})",
                            canonical = row.key,
                        )
                    })?;
                    emitted_reverse_count += 1;
                    audit_after_commit.push(DriftAuditRecord {
                        kind: "value_mismatch",
                        task_uuid: task_uuid.clone(),
                        canonical_key: row.key.clone(),
                        drift_value: Some(current_uda.clone()),
                    });
                }
                (KeyState::Pending, true) => {
                    // Finalise via `commit_task_key` — canonical already
                    // matches the allocation row; no reverse op needed.
                    // Scope/account-only drift is observed but deliberately
                    // not repaired by this sync-bridge drift path.
                    observe_account_only_drift(&mut outcome, current_udas, row);
                    outcome.post_commit_finalize += 1;
                    pending_finalisations.push(PendingFinalisation {
                        task_uuid: task_uuid.clone(),
                        key: row.key.clone(),
                        attempt_id: row.attempt_id.clone(),
                        emit_reverse: false,
                    });
                }
                (KeyState::Pending, false) => {
                    // Finalise the pending row AND emit a reverse op —
                    // canonical wins per contract, then row transitions
                    // pending → committed.
                    outcome.pending_with_drift += 1;
                    emit_reverse_uda_op(
                        replica,
                        task_uuid,
                        &row.key,
                        &row.prefix,
                        &mut reverse_ops,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "emit reverse cmdock_key UDA op for pending-with-drift \
                             {user_id}/{task_uuid} (canonical={canonical}, drift={current_uda})",
                            canonical = row.key,
                        )
                    })?;
                    emitted_reverse_count += 1;
                    pending_finalisations.push(PendingFinalisation {
                        task_uuid: task_uuid.clone(),
                        key: row.key.clone(),
                        attempt_id: row.attempt_id.clone(),
                        emit_reverse: true,
                    });
                }
                (KeyState::Burned, _) => {
                    // `lookup_task_keys_for_drift` excludes burned at
                    // the SQL level; reaching here would mean the SQL
                    // filter regressed. Defence-in-depth: skip.
                }
            },
        }
    }

    // Phase 1: commit all reverse-UDA ops in one batch. Under the
    // replica lock the caller already holds — see § Invariant.
    if emitted_reverse_count > 0 {
        replica
            .commit_operations(reverse_ops)
            .await
            .with_context(|| {
                format!(
                    "commit reverse cmdock_key UDA ops for {user_id} \
                     ({emitted_reverse_count} task(s))"
                )
            })?;
    }

    // Phase 2: finalise pending rows via `commit_task_key`. This is a
    // config-DB write; it does NOT need the replica lock. We run it
    // AFTER the reverse-op commit so:
    //   - For `pending_with_drift`: canonical replica is corrected
    //     before the row flips to committed (so a concurrent reader
    //     that arrives after this point sees a consistent
    //     replica + allocation table).
    //   - For `post_commit_finalize`: canonical already matched, so
    //     ordering doesn't matter, but we keep the same flow.
    //
    // `commit_task_key` is idempotent on already-committed-with-same-
    // attempt_id (the reaper-race regression-test guarantee), so a
    // concurrent reaper finalising the row does not cause us to error.
    for finalisation in &pending_finalisations {
        let Some((prefix, n)) = split_key(&finalisation.key) else {
            tracing::warn!(
                "drift recovery: malformed allocation key {key:?} for task {task} \
                 (user {user}); skipping finalise",
                key = finalisation.key,
                task = finalisation.task_uuid,
                user = user_id,
            );
            continue;
        };
        if let Err(e) = store
            .commit_task_key(user_id, prefix, n, &finalisation.attempt_id)
            .await
        {
            // Don't propagate — the reverse-op commit already succeeded
            // and we don't want a config-DB hiccup to look like a drift
            // recovery failure to the bridge caller. Operator visibility
            // is via the warn log + the absence of a counter increment.
            tracing::warn!(
                "drift recovery: commit_task_key failed for {user}/{key} attempt={attempt} \
                 task={task}: {e:#}",
                user = user_id,
                key = finalisation.key,
                attempt = finalisation.attempt_id,
                task = finalisation.task_uuid,
            );
            continue;
        }
        let kind = if finalisation.emit_reverse {
            "pending_with_drift"
        } else {
            "post_commit_finalize"
        };
        // Surface the drift_value only when there genuinely was drift —
        // for `post_commit_finalize` the canonical UDA already matched
        // so there's no drift value to record.
        let drift_value = if finalisation.emit_reverse {
            uda_tasks
                .get(&finalisation.task_uuid)
                .map(|udas| udas.key.clone())
        } else {
            None
        };
        audit_after_commit.push(DriftAuditRecord {
            kind,
            task_uuid: finalisation.task_uuid.clone(),
            canonical_key: finalisation.key.clone(),
            drift_value,
        });
    }

    // Phase 3: emit audits + counters in one place after all work
    // succeeded. Group counter increments by kind to minimise the
    // number of metric vec lookups.
    if outcome.value_mismatch > 0 {
        counter!(
            "task_keys_drift_recovered_total",
            "kind" => "value_mismatch"
        )
        .increment(outcome.value_mismatch as u64);
    }
    if outcome.post_commit_finalize > 0 {
        counter!(
            "task_keys_drift_recovered_total",
            "kind" => "post_commit_finalize"
        )
        .increment(outcome.post_commit_finalize as u64);
    }
    if outcome.pending_with_drift > 0 {
        counter!(
            "task_keys_drift_recovered_total",
            "kind" => "pending_with_drift"
        )
        .increment(outcome.pending_with_drift as u64);
    }
    for record in &audit_after_commit {
        audit_drift_recovered(
            audit_source,
            user_id,
            &record.task_uuid,
            record.kind,
            &record.canonical_key,
            record.drift_value.as_deref(),
        );
    }

    Ok(outcome)
}

/// Reverse-UDA op: rewrite the canonical replica's `cmdock_key` UDA back
/// to the value recorded in `task_key_allocations`. Caller batches into
/// one `Operations` and commits once after the loop.
async fn emit_reverse_uda_op<S: Storage>(
    replica: &mut Replica<S>,
    task_uuid: &str,
    canonical_key: &str,
    prefix: &str,
    ops: &mut Operations,
) -> anyhow::Result<()> {
    let parsed = Uuid::parse_str(task_uuid)
        .with_context(|| format!("parse task_uuid {task_uuid} for drift reverse op"))?;
    let mut task = replica
        .get_task(parsed)
        .await
        .with_context(|| format!("get_task {task_uuid} during drift reverse op"))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "task {task_uuid} disappeared between all_tasks and get_task during \
                 drift reverse-op build"
            )
        })?;
    task.set_value(CMDOCK_KEY_UDA, Some(canonical_key.to_string()), ops)
        .with_context(|| format!("set_value cmdock_key on {task_uuid} for drift reverse op"))?;
    // Source canonical `cmdock_task_scope` and deprecated `cmdock_account`
    // from the allocation row's Task Scope key prefix, not by parsing
    // `<PREFIX>-N` and not by re-reading `users.prefix`. Future prefix
    // rebrand/move work needs an explicit migration.
    task.set_value(CMDOCK_TASK_SCOPE_UDA, Some(prefix.to_string()), ops)
        .with_context(|| {
            format!("set_value cmdock_task_scope on {task_uuid} for drift reverse op")
        })?;
    Ok(())
}

fn observe_account_only_drift(
    outcome: &mut DriftPassOutcome,
    current_udas: &CmdockTaskUdas,
    row: &DriftAllocationRow,
) {
    if current_udas.task_scope.as_deref() != Some(row.prefix.as_str())
        || current_udas.account.as_deref() != Some(row.prefix.as_str())
    {
        outcome.account_only_drift_observed += 1;
        counter!("task_keys_account_only_drift_observed_total").increment(1);
    }
}

/// Per-row work item for the pending-row finalisation pass. Held
/// across the reverse-op commit so finalisation runs after the canonical
/// replica has been corrected for the `pending_with_drift` case.
struct PendingFinalisation {
    task_uuid: String,
    key: String,
    attempt_id: String,
    /// `true` for `pending_with_drift` (already added to reverse_ops by
    /// the classifier loop); `false` for `post_commit_finalize`.
    emit_reverse: bool,
}

/// Per-row audit work item, emitted after all writes have succeeded so
/// the audit log never claims drift recovery the bridge actually failed
/// to commit.
struct DriftAuditRecord {
    kind: &'static str,
    task_uuid: String,
    canonical_key: String,
    drift_value: Option<String>,
}

/// Audit emission for drift recovery. Routes through the shared
/// `target: "audit"` tracing layer (see `src/audit.rs`). One log line
/// per task that triggered a recovery action.
fn audit_drift_recovered(
    source: &'static str,
    user_id: &str,
    task_uuid: &str,
    kind: &'static str,
    canonical_key: &str,
    drift_value: Option<&str>,
) {
    tracing::info!(
        target: "audit",
        action = "task.key.drift_recovered",
        source = source,
        user_id = %user_id,
        task_uuid = %task_uuid,
        kind = kind,
        canonical_key = %canonical_key,
        drift_value = drift_value.unwrap_or(""),
    );
}

/// Walk every task in the canonical replica, returning a map of
/// `task_uuid → cmdock_key UDA value` for those carrying the UDA.
/// Skips tasks without the UDA (the steady-state common case for
/// pre-feature accounts that haven't been backfilled yet — those flow
/// through Phase 4, not drift recovery).
#[derive(Debug, Clone)]
struct CmdockTaskUdas {
    key: String,
    task_scope: Option<String>,
    account: Option<String>,
}

async fn collect_cmdock_key_uda_tasks<S: Storage>(
    replica: &mut Replica<S>,
) -> anyhow::Result<HashMap<String, CmdockTaskUdas>> {
    let all = replica
        .all_tasks()
        .await
        .context("all_tasks() during drift collect")?;
    let mut out = HashMap::new();
    for (uuid, task) in all {
        if let Some(value) = task.get_value(CMDOCK_KEY_UDA) {
            out.insert(
                uuid.to_string(),
                CmdockTaskUdas {
                    key: value.to_string(),
                    task_scope: task.get_value(CMDOCK_TASK_SCOPE_UDA).map(|s| s.to_string()),
                    account: task.get_value(CMDOCK_ACCOUNT_UDA).map(|s| s.to_string()),
                },
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::models::NewUser;
    use crate::store::sqlite::SqliteConfigStore;
    use std::sync::Arc;
    use taskchampion::storage::inmemory::InMemoryStorage;
    use tempfile::TempDir;

    /// Build a fresh in-memory replica + create a task carrying the given
    /// task-key UDA values. Returns `(replica, task_uuid)`.
    async fn replica_with_task_key_udas(
        key_value: &str,
        account_value: Option<&str>,
    ) -> (Replica<InMemoryStorage>, String) {
        let mut replica = Replica::new(InMemoryStorage::new());
        let mut ops = Operations::new();
        let mut task = replica
            .create_task(uuid::Uuid::new_v4(), &mut ops)
            .await
            .unwrap();
        let task_uuid = task.get_uuid().to_string();
        task.set_value(CMDOCK_KEY_UDA, Some(key_value.to_string()), &mut ops)
            .unwrap();
        if let Some(account_value) = account_value {
            task.set_value(
                CMDOCK_ACCOUNT_UDA,
                Some(account_value.to_string()),
                &mut ops,
            )
            .unwrap();
        }
        replica.commit_operations(ops).await.unwrap();
        (replica, task_uuid)
    }

    async fn replica_with_uda_task(uda_value: &str) -> (Replica<InMemoryStorage>, String) {
        replica_with_task_key_udas(uda_value, None).await
    }

    async fn fresh_store() -> (TempDir, Arc<dyn ConfigStore>, String) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("config.sqlite");
        let sqlite = Arc::new(
            SqliteConfigStore::new(&db_path.to_string_lossy())
                .await
                .unwrap(),
        );
        let store: Arc<dyn ConfigStore> = sqlite;
        store.run_migrations().await.unwrap();
        let user = store
            .create_user(&NewUser {
                username: "alice".to_string(),
                password_hash: "x".to_string(),
            })
            .await
            .unwrap();
        store.set_user_prefix(&user.id, "ALICE").await.unwrap();
        store
            .ensure_personal_task_scope_for_user(&user.id)
            .await
            .unwrap();
        (tmp, store, user.id)
    }

    /// `lookup_task_keys_for_drift` excludes burned rows — only pending
    /// and committed rows are returned, with full row metadata.
    #[tokio::test]
    async fn lookup_drift_excludes_burned_rows() {
        let (_tmp, store, user_id) = fresh_store().await;
        let task_uuid_a = uuid::Uuid::new_v4().to_string();
        let task_uuid_b = uuid::Uuid::new_v4().to_string();
        let task_uuid_c = uuid::Uuid::new_v4().to_string();

        // Reserve, attach, commit (committed row)
        let (n_a, attempt_a) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n_a, &attempt_a, &task_uuid_a)
            .await
            .unwrap();
        store
            .commit_task_key(&user_id, "ALICE", n_a, &attempt_a)
            .await
            .unwrap();

        // Reserve, attach, leave pending
        let (n_b, attempt_b) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n_b, &attempt_b, &task_uuid_b)
            .await
            .unwrap();

        // Reserve, attach, burn (must NOT be returned)
        let (n_c, attempt_c) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n_c, &attempt_c, &task_uuid_c)
            .await
            .unwrap();
        store
            .burn_task_key(&user_id, "ALICE", n_c, &attempt_c)
            .await
            .unwrap();

        let rows = store
            .lookup_task_keys_for_drift(
                &user_id,
                &[
                    task_uuid_a.clone(),
                    task_uuid_b.clone(),
                    task_uuid_c.clone(),
                ],
            )
            .await
            .unwrap();

        assert_eq!(rows.len(), 2, "burned row must be excluded");
        let by_uuid: HashMap<String, DriftAllocationRow> =
            rows.into_iter().map(|r| (r.task_uuid.clone(), r)).collect();
        let row_a = by_uuid.get(&task_uuid_a).expect("committed row present");
        assert_eq!(row_a.state, KeyState::Committed);
        assert_eq!(row_a.key, format!("ALICE-{n_a}"));
        assert_eq!(row_a.attempt_id, attempt_a);
        let row_b = by_uuid.get(&task_uuid_b).expect("pending row present");
        assert_eq!(row_b.state, KeyState::Pending);
        assert_eq!(row_b.key, format!("ALICE-{n_b}"));
        assert_eq!(row_b.attempt_id, attempt_b);
        assert!(
            !by_uuid.contains_key(&task_uuid_c),
            "burned row must be filtered"
        );
    }

    /// Empty input returns empty result without DB hit.
    #[tokio::test]
    async fn lookup_drift_empty_input_returns_empty() {
        let (_tmp, store, user_id) = fresh_store().await;
        let rows = store
            .lookup_task_keys_for_drift(&user_id, &[])
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    /// Unknown UUIDs simply produce no rows — store does not error.
    #[tokio::test]
    async fn lookup_drift_unknown_uuids_no_rows() {
        let (_tmp, store, user_id) = fresh_store().await;
        let unknown = uuid::Uuid::new_v4().to_string();
        let rows = store
            .lookup_task_keys_for_drift(&user_id, &[unknown])
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    /// `value_mismatch` against a committed allocation row — reverse-UDA
    /// op rewrites the canonical replica back to the allocation value.
    /// Outcome counter increments; canonical replica reads the restored
    /// value after the pass returns.
    #[tokio::test]
    async fn reconcile_drift_value_mismatch_emits_reverse_op() {
        let (_tmp, store, user_id) = fresh_store().await;

        // Committed allocation row → ALICE-1 for task_uuid_a.
        let (mut replica, task_uuid_a) = replica_with_uda_task("BOGUS-99").await;
        let parsed_uuid = uuid::Uuid::parse_str(&task_uuid_a).unwrap();
        let (n_a, attempt_a) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n_a, &attempt_a, &task_uuid_a)
            .await
            .unwrap();
        store
            .commit_task_key(&user_id, "ALICE", n_a, &attempt_a)
            .await
            .unwrap();
        let canonical_key = format!("ALICE-{n_a}");

        // Pre-condition: replica carries the bogus value.
        assert_eq!(
            replica
                .get_task(parsed_uuid)
                .await
                .unwrap()
                .unwrap()
                .get_value(CMDOCK_KEY_UDA)
                .map(|s| s.to_string()),
            Some("BOGUS-99".to_string())
        );

        let outcome = reconcile_drift(&store, &mut replica, &user_id, "test")
            .await
            .unwrap();
        assert_eq!(outcome.value_mismatch, 1);
        assert_eq!(outcome.no_row_skipped, 0);
        assert_eq!(outcome.post_commit_finalize, 0);
        assert_eq!(outcome.pending_with_drift, 0);
        assert_eq!(outcome.account_only_drift_observed, 0);

        // Post-condition: replica now reads the canonical value.
        assert_eq!(
            replica
                .get_task(parsed_uuid)
                .await
                .unwrap()
                .unwrap()
                .get_value(CMDOCK_KEY_UDA)
                .map(|s| s.to_string()),
            Some(canonical_key.clone())
        );
    }

    /// `committed` row whose `cmdock_key` already matches → no key
    /// reverse-op. Missing `cmdock_task_scope` / `cmdock_account` is observed
    /// separately in this sync-bridge drift path.
    #[tokio::test]
    async fn reconcile_drift_committed_match_is_no_op() {
        let (_tmp, store, user_id) = fresh_store().await;

        let (mut replica, task_uuid) = replica_with_uda_task("placeholder").await;
        let (n, attempt) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n, &attempt, &task_uuid)
            .await
            .unwrap();
        store
            .commit_task_key(&user_id, "ALICE", n, &attempt)
            .await
            .unwrap();
        let canonical_key = format!("ALICE-{n}");

        // Update replica to carry the canonical value (simulating a
        // post-backfill steady state — no drift to recover).
        let parsed = uuid::Uuid::parse_str(&task_uuid).unwrap();
        let mut ops = Operations::new();
        let mut task = replica.get_task(parsed).await.unwrap().unwrap();
        task.set_value(CMDOCK_KEY_UDA, Some(canonical_key.clone()), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();

        let outcome = reconcile_drift(&store, &mut replica, &user_id, "test")
            .await
            .unwrap();
        assert_eq!(outcome.value_mismatch, 0);
        assert_eq!(outcome.no_row_skipped, 0);
        assert_eq!(outcome.account_only_drift_observed, 1);
    }

    /// Account-only drift is observed for operators but not repaired by
    /// The key matches, while `cmdock_task_scope` / `cmdock_account` is
    /// missing or wrong.
    #[tokio::test]
    async fn reconcile_drift_account_only_mismatch_observed_not_repaired() {
        let (_tmp, store, user_id) = fresh_store().await;

        let (n, attempt) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        let canonical_key = format!("ALICE-{n}");
        let (mut replica, task_uuid) =
            replica_with_task_key_udas(&canonical_key, Some("BOGUS")).await;
        let parsed = uuid::Uuid::parse_str(&task_uuid).unwrap();
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n, &attempt, &task_uuid)
            .await
            .unwrap();
        store
            .commit_task_key(&user_id, "ALICE", n, &attempt)
            .await
            .unwrap();

        let outcome = reconcile_drift(&store, &mut replica, &user_id, "test")
            .await
            .unwrap();
        assert_eq!(outcome.value_mismatch, 0);
        assert_eq!(outcome.account_only_drift_observed, 1);
        let task = replica.get_task(parsed).await.unwrap().unwrap();
        assert_eq!(
            task.get_value(CMDOCK_ACCOUNT_UDA).map(|s| s.to_string()),
            Some("BOGUS".to_string()),
            "sync-bridge drift observes legacy prefix UDA drift but does not repair it"
        );
    }

    /// Drift reverse-op corrects `cmdock_key` and `cmdock_task_scope` using
    /// the allocation row prefix. Deprecated `cmdock_account` is NOT written
    /// by the reverse-op (server no longer emits it as canonical beta projection).
    #[tokio::test]
    async fn reconcile_drift_reverse_corrects_key_and_scope_not_account() {
        let (_tmp, store, user_id) = fresh_store().await;

        let (mut replica, task_uuid) = replica_with_task_key_udas("BOGUS-99", Some("BOGUS")).await;
        let parsed = uuid::Uuid::parse_str(&task_uuid).unwrap();
        let (n, attempt) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n, &attempt, &task_uuid)
            .await
            .unwrap();
        store
            .commit_task_key(&user_id, "ALICE", n, &attempt)
            .await
            .unwrap();

        let outcome = reconcile_drift(&store, &mut replica, &user_id, "test")
            .await
            .unwrap();
        assert_eq!(outcome.value_mismatch, 1);
        let task = replica.get_task(parsed).await.unwrap().unwrap();
        // cmdock_account is NOT written by drift reverse-op; legacy value preserved.
        assert_eq!(
            task.get_value(CMDOCK_ACCOUNT_UDA).map(|s| s.to_string()),
            Some("BOGUS".to_string()),
            "drift reverse-op must not write cmdock_account"
        );
        assert_eq!(
            task.get_value(CMDOCK_KEY_UDA).map(|s| s.to_string()),
            Some(format!("ALICE-{n}"))
        );
    }

    /// `cmdock_key` UDA on canonical with NO allocation row → contract
    /// SKIP rule: do NOT emit reverse op, do NOT audit, but
    /// `task_keys_drift_skipped_no_row_total` increments. Replica
    /// unchanged. Phase 5e orphan reconciliation will pick this up next
    /// pass.
    #[tokio::test]
    async fn reconcile_drift_no_allocation_row_skipped() {
        let (_tmp, store, user_id) = fresh_store().await;

        // Replica has the UDA; allocation table has no row at all.
        let (mut replica, task_uuid) = replica_with_uda_task("ORPHAN-7").await;
        let parsed = uuid::Uuid::parse_str(&task_uuid).unwrap();

        let outcome = reconcile_drift(&store, &mut replica, &user_id, "test")
            .await
            .unwrap();
        assert_eq!(outcome.no_row_skipped, 1);
        assert_eq!(outcome.value_mismatch, 0);

        // Replica UDA preserved (we did NOT emit a reverse op).
        assert_eq!(
            replica
                .get_task(parsed)
                .await
                .unwrap()
                .unwrap()
                .get_value(CMDOCK_KEY_UDA)
                .map(|s| s.to_string()),
            Some("ORPHAN-7".to_string())
        );
    }

    /// Empty replica (no tasks with `cmdock_key` UDA) → fast path,
    /// no DB lookup, all counters zero.
    #[tokio::test]
    async fn reconcile_drift_empty_replica_fast_path() {
        let (_tmp, store, user_id) = fresh_store().await;
        let mut replica: Replica<InMemoryStorage> = Replica::new(InMemoryStorage::new());
        let outcome = reconcile_drift(&store, &mut replica, &user_id, "test")
            .await
            .unwrap();
        assert_eq!(outcome.value_mismatch, 0);
        assert_eq!(outcome.no_row_skipped, 0);
        assert_eq!(outcome.post_commit_finalize, 0);
        assert_eq!(outcome.pending_with_drift, 0);
        assert_eq!(outcome.account_only_drift_observed, 0);
    }

    /// `post_commit_finalize`: canonical UDA matches a `pending` row →
    /// `commit_task_key` finalises the row. This covers the Phase 2
    /// ambiguous-recovery case where TC commit succeeded but the
    /// allocation row never transitioned committed (e.g. process crash
    /// between TC commit and `commit_task_key`).
    #[tokio::test]
    async fn reconcile_drift_pending_match_finalises_row() {
        let (_tmp, store, user_id) = fresh_store().await;

        // Pending allocation row attached to task_uuid.
        let (n, attempt) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        let canonical_key = format!("ALICE-{n}");
        let (mut replica, task_uuid) = replica_with_uda_task(&canonical_key).await;
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n, &attempt, &task_uuid)
            .await
            .unwrap();

        // Pre-condition: row is pending.
        let row = store
            .lookup_task_key_by_uuid(&user_id, &task_uuid)
            .await
            .unwrap()
            .expect("row exists pre-recovery");
        assert_eq!(row.1, KeyState::Pending);

        let outcome = reconcile_drift(&store, &mut replica, &user_id, "test")
            .await
            .unwrap();
        assert_eq!(outcome.post_commit_finalize, 1);
        assert_eq!(outcome.value_mismatch, 0);
        assert_eq!(outcome.pending_with_drift, 0);

        // Post-condition: row is committed.
        let row = store
            .lookup_task_key_by_uuid(&user_id, &task_uuid)
            .await
            .unwrap()
            .expect("row exists post-recovery");
        assert_eq!(row.1, KeyState::Committed);
        assert_eq!(row.0, canonical_key);
    }

    /// `pending_with_drift`: canonical UDA disagrees with a `pending`
    /// row → emit reverse-UDA op AND finalise the row. Both effects
    /// observable: replica reads canonical value, row transitions to
    /// committed.
    #[tokio::test]
    async fn reconcile_drift_pending_mismatch_finalises_and_reverses() {
        let (_tmp, store, user_id) = fresh_store().await;

        let (n, attempt) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        let canonical_key = format!("ALICE-{n}");

        // Replica carries a drift value; allocation row pending +
        // attached.
        let (mut replica, task_uuid) = replica_with_uda_task("DRIFT-1").await;
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n, &attempt, &task_uuid)
            .await
            .unwrap();

        let parsed_uuid = uuid::Uuid::parse_str(&task_uuid).unwrap();

        let outcome = reconcile_drift(&store, &mut replica, &user_id, "test")
            .await
            .unwrap();
        assert_eq!(outcome.pending_with_drift, 1);
        assert_eq!(outcome.value_mismatch, 0);
        assert_eq!(outcome.post_commit_finalize, 0);

        // Replica reads the canonical value (reverse-UDA committed).
        assert_eq!(
            replica
                .get_task(parsed_uuid)
                .await
                .unwrap()
                .unwrap()
                .get_value(CMDOCK_KEY_UDA)
                .map(|s| s.to_string()),
            Some(canonical_key.clone())
        );

        // Row is committed (commit_task_key finalised it).
        let row = store
            .lookup_task_key_by_uuid(&user_id, &task_uuid)
            .await
            .unwrap()
            .expect("row still exists");
        assert_eq!(row.1, KeyState::Committed);
        assert_eq!(row.0, canonical_key);
    }

    /// Mixed drift in a single pass — value_mismatch + pending_with_drift
    /// + post_commit_finalize + no_row_skipped + committed-match all
    /// classified correctly and applied in one sync round-trip.
    /// Regression lock for the batched-Operations + audit-after-commit
    /// shape.
    #[tokio::test]
    async fn reconcile_drift_mixed_kinds_in_one_pass() {
        let (_tmp, store, user_id) = fresh_store().await;
        let mut replica: Replica<InMemoryStorage> = Replica::new(InMemoryStorage::new());

        // Helper: append a task carrying a given UDA value.
        async fn add_task(replica: &mut Replica<InMemoryStorage>, uda_value: &str) -> String {
            let mut ops = Operations::new();
            let mut task = replica
                .create_task(uuid::Uuid::new_v4(), &mut ops)
                .await
                .unwrap();
            let uuid = task.get_uuid().to_string();
            task.set_value(CMDOCK_KEY_UDA, Some(uda_value.to_string()), &mut ops)
                .unwrap();
            replica.commit_operations(ops).await.unwrap();
            uuid
        }

        // value_mismatch: committed row, drift on replica.
        let (n_mm, attempt_mm) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        let mm_uuid = add_task(&mut replica, "DRIFT-A").await;
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n_mm, &attempt_mm, &mm_uuid)
            .await
            .unwrap();
        store
            .commit_task_key(&user_id, "ALICE", n_mm, &attempt_mm)
            .await
            .unwrap();
        let mm_canonical = format!("ALICE-{n_mm}");

        // committed-match: no drift.
        let (n_ok, attempt_ok) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        let ok_canonical = format!("ALICE-{n_ok}");
        let ok_uuid = add_task(&mut replica, &ok_canonical).await;
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n_ok, &attempt_ok, &ok_uuid)
            .await
            .unwrap();
        store
            .commit_task_key(&user_id, "ALICE", n_ok, &attempt_ok)
            .await
            .unwrap();

        // post_commit_finalize: pending row, replica matches.
        let (n_fin, attempt_fin) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        let fin_canonical = format!("ALICE-{n_fin}");
        let fin_uuid = add_task(&mut replica, &fin_canonical).await;
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n_fin, &attempt_fin, &fin_uuid)
            .await
            .unwrap();

        // pending_with_drift: pending row + replica drift.
        let (n_pwd, attempt_pwd) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        let pwd_canonical = format!("ALICE-{n_pwd}");
        let pwd_uuid = add_task(&mut replica, "DRIFT-B").await;
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n_pwd, &attempt_pwd, &pwd_uuid)
            .await
            .unwrap();

        // no_row_skipped: replica UDA, no allocation row.
        let orphan_uuid = add_task(&mut replica, "ORPHAN-9").await;

        let outcome = reconcile_drift(&store, &mut replica, &user_id, "test")
            .await
            .unwrap();
        assert_eq!(outcome.value_mismatch, 1);
        assert_eq!(outcome.post_commit_finalize, 1);
        assert_eq!(outcome.pending_with_drift, 1);
        assert_eq!(outcome.no_row_skipped, 1);

        // value_mismatch: canonical restored.
        assert_eq!(
            replica
                .get_task(uuid::Uuid::parse_str(&mm_uuid).unwrap())
                .await
                .unwrap()
                .unwrap()
                .get_value(CMDOCK_KEY_UDA)
                .map(|s| s.to_string()),
            Some(mm_canonical.clone())
        );
        // pending_with_drift: canonical restored AND row committed.
        assert_eq!(
            replica
                .get_task(uuid::Uuid::parse_str(&pwd_uuid).unwrap())
                .await
                .unwrap()
                .unwrap()
                .get_value(CMDOCK_KEY_UDA)
                .map(|s| s.to_string()),
            Some(pwd_canonical.clone())
        );
        let row = store
            .lookup_task_key_by_uuid(&user_id, &pwd_uuid)
            .await
            .unwrap()
            .expect("pwd row");
        assert_eq!(row.1, KeyState::Committed);
        // post_commit_finalize: row committed.
        let row = store
            .lookup_task_key_by_uuid(&user_id, &fin_uuid)
            .await
            .unwrap()
            .expect("fin row");
        assert_eq!(row.1, KeyState::Committed);
        // orphan: replica UDA preserved (no reverse op was emitted).
        assert_eq!(
            replica
                .get_task(uuid::Uuid::parse_str(&orphan_uuid).unwrap())
                .await
                .unwrap()
                .unwrap()
                .get_value(CMDOCK_KEY_UDA)
                .map(|s| s.to_string()),
            Some("ORPHAN-9".to_string())
        );
    }
}
