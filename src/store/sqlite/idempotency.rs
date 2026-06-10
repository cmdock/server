//! Idempotency-Key dedup record CRUD per `task-write-contract.md`
//! § Idempotency (cmdock/architecture commit a3f242a).
//!
//! Implements the three-phase write-ahead pattern:
//!
//! - **Phase 1** (`lookup_or_insert_pending_impl`) — atomic lookup +
//!   insert under `BEGIN IMMEDIATE`. Drives the dispatch state machine
//!   per § Replay behaviour by record state. Stranded `pending` rows
//!   past the timeout are treated as expired and removed in the same
//!   transaction (lookup-time expiry, deterministic regardless of
//!   reaper status).
//! - **Phase 3** (`finalize_completed_impl`) — `UPDATE ... WHERE
//!   tuple_key = ? AND attempt_id = ? AND state = 'pending'`. The
//!   attempt-id guard defeats the stale-finalizer race: a delayed
//!   Phase 3 from an attempt whose row has already been replaced by
//!   a fresh retry finds zero matching rows and is silently discarded.
//! - **Rollback** (`rollback_pending_impl`) — only for known-no-commit
//!   Phase 2 failures (validation, business-rule rejection). Same
//!   attempt-id guard.
//!
//! Pruners (`prune_completed_impl`, `prune_stranded_pending_impl`) run
//! alongside the existing webhook-delivery-log purge as operational
//! hygiene — correctness does NOT depend on them, lookup-time expiry
//! handles that.

use rusqlite::OptionalExtension;
use uuid::Uuid;

use crate::store::models::IdempotencyLookupOutcome;

use super::{map_err, BoxErr, SqliteConfigStore};

// All the "too many arguments" lints below trigger because the contract
// requires distinct primitive args (user_id, request_path, key,
// fingerprint, two timeouts, now). Wrapping in a struct is mechanical
// noise — the args are flat primitives that pass through to bound SQL
// parameters one-to-one. Allow the lint locally.
#[allow(clippy::too_many_arguments)]
impl SqliteConfigStore {
    pub(super) async fn lookup_or_insert_idempotency_pending_impl(
        &self,
        user_id: &str,
        request_path: &str,
        idempotency_key: &str,
        body_fingerprint: &[u8; 32],
        pending_timeout_seconds: u32,
        completed_retention_hours: u32,
        now_unix_seconds: i64,
    ) -> anyhow::Result<IdempotencyLookupOutcome> {
        let user_id = user_id.to_string();
        let request_path = request_path.to_string();
        let idempotency_key = idempotency_key.to_string();
        let fingerprint = body_fingerprint.to_vec();
        let pending_timeout = i64::from(pending_timeout_seconds);
        let completed_retention = i64::from(completed_retention_hours) * 3600;

        let outcome = self
            .conn
            .call(move |conn| {
                // BEGIN IMMEDIATE serialises with other writers — required so
                // two concurrent retries don't both observe "no row" and both
                // proceed to fresh execution. Spec § Server behaviour Phase 1.
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                #[allow(clippy::type_complexity)]
                let existing: Option<(
                    String,          // attempt_id
                    String,          // state
                    Vec<u8>,         // body_fingerprint
                    Option<i64>,     // status_code
                    Option<Vec<u8>>, // response_body
                    Option<String>,  // content_type
                    Option<i64>,     // content_length
                    i64,             // created_at
                )> = tx
                    .query_row(
                        "SELECT attempt_id, state, body_fingerprint,
                                status_code, response_body, content_type,
                                content_length, created_at
                         FROM idempotency_records
                         WHERE user_id = ?1 AND request_path = ?2
                               AND idempotency_key = ?3",
                        rusqlite::params![user_id, request_path, idempotency_key],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                                row.get(5)?,
                                row.get(6)?,
                                row.get(7)?,
                            ))
                        },
                    )
                    .optional()?;

                if let Some((
                    _,
                    state,
                    stored_fp,
                    status_code,
                    response_body,
                    content_type,
                    content_length,
                    created_at,
                )) = existing
                {
                    let age = now_unix_seconds - created_at;
                    let fingerprint_match = stored_fp == fingerprint;

                    match state.as_str() {
                        "pending" if age >= pending_timeout => {
                            // Stranded pending past timeout — expire in this
                            // transaction (lookup-time expiry per spec).
                            // Then fall through to fresh-insert.
                            tx.execute(
                                "DELETE FROM idempotency_records
                                 WHERE user_id = ?1 AND request_path = ?2
                                       AND idempotency_key = ?3",
                                rusqlite::params![user_id, request_path, idempotency_key],
                            )?;
                        }
                        "pending" if fingerprint_match => {
                            tx.commit()?;
                            return Ok::<_, BoxErr>(IdempotencyLookupOutcome::InFlight);
                        }
                        "pending" => {
                            tx.commit()?;
                            return Ok::<_, BoxErr>(IdempotencyLookupOutcome::Conflict);
                        }
                        "completed" if age >= completed_retention => {
                            // Past retention — expire and fall through to
                            // fresh-insert. Operationally rare given the
                            // background pruner, but kept for correctness
                            // when the pruner lags or is disabled.
                            tx.execute(
                                "DELETE FROM idempotency_records
                                 WHERE user_id = ?1 AND request_path = ?2
                                       AND idempotency_key = ?3",
                                rusqlite::params![user_id, request_path, idempotency_key],
                            )?;
                        }
                        "completed" if fingerprint_match => {
                            let outcome = IdempotencyLookupOutcome::Replay {
                                status_code: status_code.unwrap_or(200) as u16,
                                response_body: response_body.unwrap_or_default(),
                                content_type,
                                content_length,
                            };
                            tx.commit()?;
                            return Ok::<_, BoxErr>(outcome);
                        }
                        "completed" => {
                            tx.commit()?;
                            return Ok::<_, BoxErr>(IdempotencyLookupOutcome::Conflict);
                        }
                        _ => {
                            // Unknown state — defensive; CHECK constraint
                            // should prevent this, but treat as a fresh
                            // insert if it ever happens.
                            tx.execute(
                                "DELETE FROM idempotency_records
                                 WHERE user_id = ?1 AND request_path = ?2
                                       AND idempotency_key = ?3",
                                rusqlite::params![user_id, request_path, idempotency_key],
                            )?;
                        }
                    }
                }

                // No row, or expired row removed above — fresh execution.
                // Generate the attempt id and insert the pending row.
                let attempt_id = Uuid::new_v4().as_hyphenated().to_string();
                tx.execute(
                    "INSERT INTO idempotency_records (
                         user_id, request_path, idempotency_key,
                         attempt_id, state, body_fingerprint, created_at
                     ) VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6)",
                    rusqlite::params![
                        user_id,
                        request_path,
                        idempotency_key,
                        attempt_id,
                        fingerprint,
                        now_unix_seconds,
                    ],
                )?;
                tx.commit()?;
                Ok::<_, BoxErr>(IdempotencyLookupOutcome::FreshExecution { attempt_id })
            })
            .await
            .map_err(map_err)?;
        Ok(outcome)
    }

    pub(super) async fn finalize_idempotency_completed_impl(
        &self,
        user_id: &str,
        request_path: &str,
        idempotency_key: &str,
        attempt_id: &str,
        status_code: u16,
        response_body: &[u8],
        content_type: Option<&str>,
    ) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let request_path = request_path.to_string();
        let idempotency_key = idempotency_key.to_string();
        let attempt_id = attempt_id.to_string();
        let response_body = response_body.to_vec();
        let response_body_len = response_body.len() as i64;
        let content_type = content_type.map(str::to_string);

        let updated = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "UPDATE idempotency_records
                     SET state = 'completed',
                         status_code = ?5,
                         response_body = ?6,
                         content_type = ?7,
                         content_length = ?8
                     WHERE user_id = ?1 AND request_path = ?2
                           AND idempotency_key = ?3
                           AND attempt_id = ?4
                           AND state = 'pending'",
                    rusqlite::params![
                        user_id,
                        request_path,
                        idempotency_key,
                        attempt_id,
                        status_code as i64,
                        response_body,
                        content_type,
                        response_body_len,
                    ],
                )?;
                Ok::<_, BoxErr>(rows == 1)
            })
            .await
            .map_err(map_err)?;
        Ok(updated)
    }

    pub(super) async fn rollback_idempotency_pending_impl(
        &self,
        user_id: &str,
        request_path: &str,
        idempotency_key: &str,
        attempt_id: &str,
    ) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let request_path = request_path.to_string();
        let idempotency_key = idempotency_key.to_string();
        let attempt_id = attempt_id.to_string();

        let deleted = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "DELETE FROM idempotency_records
                     WHERE user_id = ?1 AND request_path = ?2
                           AND idempotency_key = ?3
                           AND attempt_id = ?4
                           AND state = 'pending'",
                    rusqlite::params![user_id, request_path, idempotency_key, attempt_id],
                )?;
                Ok::<_, BoxErr>(rows == 1)
            })
            .await
            .map_err(map_err)?;
        Ok(deleted)
    }

    pub(super) async fn prune_idempotency_completed_impl(
        &self,
        retention_hours: u32,
        now_unix_seconds: i64,
    ) -> anyhow::Result<usize> {
        let cutoff = now_unix_seconds - i64::from(retention_hours) * 3600;
        let deleted = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "DELETE FROM idempotency_records
                     WHERE state = 'completed' AND created_at < ?1",
                    rusqlite::params![cutoff],
                )?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(deleted)
    }

    pub(super) async fn prune_idempotency_stranded_pending_impl(
        &self,
        pending_timeout_seconds: u32,
        now_unix_seconds: i64,
    ) -> anyhow::Result<usize> {
        let cutoff = now_unix_seconds - i64::from(pending_timeout_seconds);
        let deleted = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "DELETE FROM idempotency_records
                     WHERE state = 'pending' AND created_at < ?1",
                    rusqlite::params![cutoff],
                )?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(deleted)
    }
}
