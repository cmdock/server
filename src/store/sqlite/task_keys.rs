//! Task-key allocation primitives — backing for the `ConfigStore` trait
//! methods declared in `src/store/mod.rs` § Task Keys.
//!
//! The reaper coordinator (decision + lock + TC scan) lives in
//! `src/task_keys/reaper.rs` because it needs `AppState`. This module
//! exposes only DB primitives.
//!
//! See `migrations/025_create_task_key_allocations.sql` for the
//! three-state row model (`pending|committed|burned`) and
//! `task-write-contract.md` § Task Keys for the contract.

use std::collections::HashMap;

use rusqlite::OptionalExtension;
use uuid::Uuid;

use crate::store::models::{
    DriftAllocationRow, KeyState, StalePendingCandidate, UserWithoutPrefix,
};
use crate::store::StoreError;

use super::{map_err, store_err_from_anyhow, BoxErr, SqliteConfigStore};

fn active_personal_task_scope_id(
    tx: &rusqlite::Transaction<'_>,
    user_id: &str,
    prefix: &str,
) -> rusqlite::Result<Option<String>> {
    tx.query_row(
        "SELECT id
         FROM task_scopes
         WHERE kind = 'personal'
           AND status = 'active'
           AND owner_runtime_user_id = ?1
           AND key_prefix = ?2",
        rusqlite::params![user_id, prefix],
        |row| row.get(0),
    )
    .optional()
}

fn missing_task_scope_err(user_id: &str, prefix: &str) -> BoxErr {
    crate::store::error::MissingTaskScopeForAllocationError {
        user_id: user_id.to_string(),
        prefix: prefix.to_string(),
    }
    .into()
}

/// Cap UUID list chunks to stay within SQLite's
/// `SQLITE_MAX_VARIABLE_NUMBER`. The bundled SQLite (3.32+) supports
/// 32766, but we chunk smaller so the same code runs against older
/// builds and so query plans don't bloat. 500 is the documented value
/// in `task-write-contract.md` § Wire-vs-cache nullability.
const UUID_CHUNK_SIZE: usize = 500;

impl SqliteConfigStore {
    pub(super) async fn reserve_task_key_pending_impl(
        &self,
        user_id: &str,
        prefix: &str,
    ) -> Result<(i64, String), StoreError> {
        let user_id = user_id.to_string();
        let prefix = prefix.to_string();
        let attempt_id = Uuid::new_v4().to_string();
        let attempt_id_clone = attempt_id.clone();

        let n: i64 = self
            .conn
            .call(move |conn| {
                // BEGIN IMMEDIATE serialises with other writers so two
                // concurrent reservations on the same (user_id, prefix)
                // can't both compute the same MAX(n)+1.
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                let task_scope_id = active_personal_task_scope_id(&tx, &user_id, &prefix)?
                    .ok_or_else(|| missing_task_scope_err(&user_id, &prefix))?;

                // MAX(n) over ALL states — burned rows MUST be counted so
                // rollback gaps cannot be reused (see `task-write-contract.md`
                // § Burn semantics). During S2, the compatibility
                // `(user_id, prefix)` columns remain the primary key and are
                // still the authoritative scan shape for legacy rows.
                let next_n: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(n), 0) + 1 FROM task_key_allocations
                     WHERE user_id = ?1 AND prefix = ?2",
                    rusqlite::params![user_id, prefix],
                    |row| row.get(0),
                )?;

                tx.execute(
                    "INSERT INTO task_key_allocations
                        (user_id, prefix, n, task_scope_id, task_uuid, state, attempt_id)
                     VALUES (?1, ?2, ?3, ?4, NULL, 'pending', ?5)",
                    rusqlite::params![user_id, prefix, next_n, task_scope_id, attempt_id_clone],
                )?;

                // Time tx.commit() in isolation: this is the fsync/write-exec
                // cost on the config DB, distinct from the queue-wait captured
                // by the caller-side `config_call_seconds{call="reserve"}`.
                // Subtracting this from that caller-side total quantifies the
                // single-connection serialization component (#146 / #152).
                let commit_t = std::time::Instant::now();
                tx.commit()?;
                ::metrics::histogram!("config_store_commit_seconds", "call" => "reserve")
                    .record(commit_t.elapsed().as_secs_f64());
                Ok::<_, BoxErr>(next_n)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;

        Ok((n, attempt_id))
    }

    pub(super) async fn reserve_task_key_pending_for_uuid_impl(
        &self,
        user_id: &str,
        prefix: &str,
        task_uuid: &str,
    ) -> Result<(i64, String), StoreError> {
        let user_id = user_id.to_string();
        let prefix = prefix.to_string();
        let task_uuid = task_uuid.to_string();
        let attempt_id = Uuid::new_v4().to_string();
        let attempt_id_clone = attempt_id.clone();

        let n: i64 = self
            .conn
            .call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let task_scope_id = active_personal_task_scope_id(&tx, &user_id, &prefix)?
                    .ok_or_else(|| missing_task_scope_err(&user_id, &prefix))?;
                let next_n: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(n), 0) + 1 FROM task_key_allocations
                     WHERE user_id = ?1 AND prefix = ?2",
                    rusqlite::params![user_id, prefix],
                    |row| row.get(0),
                )?;
                tx.execute(
                    "INSERT INTO task_key_allocations
                        (user_id, prefix, n, task_scope_id, task_uuid, state, attempt_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
                    rusqlite::params![
                        user_id,
                        prefix,
                        next_n,
                        task_scope_id,
                        task_uuid,
                        attempt_id_clone
                    ],
                )?;
                // Same fsync-isolation metric as `reserve_task_key_pending_impl`
                // (#152) — this is the REST create hot path since #148, so the
                // create-path commit must stay visible in this series.
                let commit_t = std::time::Instant::now();
                tx.commit()?;
                ::metrics::histogram!("config_store_commit_seconds", "call" => "reserve")
                    .record(commit_t.elapsed().as_secs_f64());
                Ok::<_, BoxErr>(next_n)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;

        Ok((n, attempt_id))
    }

    pub(super) async fn attach_task_uuid_to_pending_impl(
        &self,
        user_id: &str,
        prefix: &str,
        n: i64,
        attempt_id: &str,
        task_uuid: &str,
    ) -> Result<(), StoreError> {
        let user_id = user_id.to_string();
        let prefix = prefix.to_string();
        let attempt_id = attempt_id.to_string();
        let task_uuid = task_uuid.to_string();

        let rows: usize = self
            .conn
            .call(move |conn| {
                let n_rows = conn.execute(
                    "UPDATE task_key_allocations
                        SET task_uuid = ?5
                      WHERE user_id = ?1 AND prefix = ?2 AND n = ?3
                            AND state = 'pending' AND attempt_id = ?4
                            AND task_uuid IS NULL",
                    rusqlite::params![user_id, prefix, n, attempt_id, task_uuid],
                )?;
                Ok::<_, BoxErr>(n_rows)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;

        if rows == 1 {
            Ok(())
        } else {
            Err(StoreError::AllocationStaleFinalizer)
        }
    }

    pub(super) async fn commit_task_key_impl(
        &self,
        user_id: &str,
        prefix: &str,
        n: i64,
        attempt_id: &str,
    ) -> Result<(), StoreError> {
        let user_id = user_id.to_string();
        let prefix = prefix.to_string();
        let attempt_id = attempt_id.to_string();

        // Single closure: do the conditional UPDATE, and on rows-affected==0
        // read the row's current state+attempt to decide
        // idempotent-success vs stale-finaliser. Both queries see the same
        // SQLite snapshot since `tokio_rusqlite::Connection::call` runs
        // them sequentially on one connection.
        enum Outcome {
            Updated,
            AlreadyCommittedSameAttempt,
            Stale,
        }

        let outcome: Outcome = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "UPDATE task_key_allocations
                        SET state = 'committed',
                            committed_at = datetime('now')
                      WHERE user_id = ?1 AND prefix = ?2 AND n = ?3
                            AND state = 'pending' AND attempt_id = ?4
                            AND task_uuid IS NOT NULL",
                    rusqlite::params![user_id, prefix, n, attempt_id],
                )?;
                if rows == 1 {
                    return Ok::<_, BoxErr>(Outcome::Updated);
                }

                // rows == 0: either the row is already committed (reaper
                // race or earlier idempotent retry) or the attempt_id
                // doesn't match (stale finaliser).
                let row: Option<(String, String)> = conn
                    .query_row(
                        "SELECT state, attempt_id
                         FROM task_key_allocations
                         WHERE user_id = ?1 AND prefix = ?2 AND n = ?3",
                        rusqlite::params![user_id, prefix, n],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;

                match row {
                    Some((state, attempt)) if state == "committed" && attempt == attempt_id => {
                        Ok::<_, BoxErr>(Outcome::AlreadyCommittedSameAttempt)
                    }
                    _ => Ok::<_, BoxErr>(Outcome::Stale),
                }
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;

        match outcome {
            Outcome::Updated | Outcome::AlreadyCommittedSameAttempt => Ok(()),
            Outcome::Stale => Err(StoreError::AllocationStaleFinalizer),
        }
    }

    pub(super) async fn burn_task_key_impl(
        &self,
        user_id: &str,
        prefix: &str,
        n: i64,
        attempt_id: &str,
    ) -> Result<(), StoreError> {
        let user_id = user_id.to_string();
        let prefix = prefix.to_string();
        let attempt_id = attempt_id.to_string();

        enum Outcome {
            Burned,
            AlreadyBurnedSameAttempt,
            Stale,
        }

        let outcome: Outcome = self
            .conn
            .call(move |conn| {
                // Detach `task_uuid` on burn so the partial unique index
                // `idx_task_key_allocations_uuid` (UNIQUE on task_uuid
                // WHERE task_uuid IS NOT NULL) does not block a future
                // allocation for the same task UUID. The `n` slot stays
                // burned forever — `MAX(n)` over all states cannot reuse
                // it — but the UUID slot is freed. This is the
                // documented post-burn shape (see migration 025 § "May
                // be NULL on burned rows").
                let rows = conn.execute(
                    "UPDATE task_key_allocations
                        SET state = 'burned',
                            task_uuid = NULL
                      WHERE user_id = ?1 AND prefix = ?2 AND n = ?3
                            AND state = 'pending' AND attempt_id = ?4",
                    rusqlite::params![user_id, prefix, n, attempt_id],
                )?;
                if rows == 1 {
                    return Ok::<_, BoxErr>(Outcome::Burned);
                }

                let row: Option<(String, String)> = conn
                    .query_row(
                        "SELECT state, attempt_id
                         FROM task_key_allocations
                         WHERE user_id = ?1 AND prefix = ?2 AND n = ?3",
                        rusqlite::params![user_id, prefix, n],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;

                match row {
                    Some((state, attempt)) if state == "burned" && attempt == attempt_id => {
                        Ok::<_, BoxErr>(Outcome::AlreadyBurnedSameAttempt)
                    }
                    _ => Ok::<_, BoxErr>(Outcome::Stale),
                }
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;

        match outcome {
            Outcome::Burned | Outcome::AlreadyBurnedSameAttempt => Ok(()),
            Outcome::Stale => Err(StoreError::AllocationStaleFinalizer),
        }
    }

    pub(super) async fn select_stale_pending_task_keys_impl(
        &self,
        now_unix_seconds: i64,
        pending_timeout_seconds: u32,
        batch_limit: usize,
    ) -> Result<Vec<StalePendingCandidate>, StoreError> {
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT user_id, prefix, n, attempt_id, task_uuid
                     FROM task_key_allocations
                     WHERE state = 'pending'
                           AND CAST(strftime('%s', created_at) AS INTEGER)
                               < (?1 - ?2)
                     ORDER BY user_id, prefix, n
                     LIMIT ?3",
                )?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![
                            now_unix_seconds,
                            i64::from(pending_timeout_seconds),
                            batch_limit as i64,
                        ],
                        |row| {
                            Ok(StalePendingCandidate {
                                user_id: row.get(0)?,
                                prefix: row.get(1)?,
                                n: row.get(2)?,
                                attempt_id: row.get(3)?,
                                task_uuid: row.get(4)?,
                            })
                        },
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(result)
    }

    pub(super) async fn get_user_prefix_impl(
        &self,
        user_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let user_id = user_id.to_string();
        // Hot read on every add_task; `prefix` is immutable once set, so the
        // read pool's WAL snapshot is always correct. Routed off the writer
        // queue per #147 (this is the read that grew 368× in #146).
        let result = self
            .read_call(move |conn| {
                let row: Option<Option<String>> = conn
                    .query_row("SELECT prefix FROM users WHERE id = ?1", [&user_id], |r| {
                        r.get(0)
                    })
                    .optional()?;
                Ok::<_, BoxErr>(row.flatten())
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(result)
    }

    pub(super) async fn set_user_prefix_impl(
        &self,
        user_id: &str,
        prefix: &str,
    ) -> Result<(), StoreError> {
        let user_id_owned = user_id.to_string();
        let user_id = user_id_owned.clone();
        let prefix = prefix.to_string();

        // Three states to distinguish:
        //   * row missing entirely → propagate as Other (caller bug — user
        //     doesn't exist; surfaced as anyhow chain for visibility)
        //   * any allocation row exists for this user → PrefixLocked
        //   * UPDATE collides on the partial unique index →
        //     StoreError::Constraint(Unique{USERS_PREFIX}) via the
        //     existing rusqlite_unique_resource mapping.
        //
        // The lock check + UPDATE run in a transaction so a race between
        // "no allocations exist" and "first allocation lands" cannot
        // sneak under us.
        enum Outcome {
            Updated,
            UserMissing,
            Locked,
        }

        let outcome: Result<Outcome, rusqlite::Error> = self
            .conn
            .call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                let user_exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM users WHERE id = ?1)",
                    [&user_id],
                    |r| r.get(0),
                )?;
                if !user_exists {
                    tx.commit()?;
                    return Ok::<_, BoxErr>(Ok(Outcome::UserMissing));
                }

                let any_alloc: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM task_key_allocations WHERE user_id = ?1)",
                    [&user_id],
                    |r| r.get(0),
                )?;
                if any_alloc {
                    tx.commit()?;
                    return Ok::<_, BoxErr>(Ok(Outcome::Locked));
                }

                // S1 Task Scope lock-extension: once an active Personal Task
                // Scope exists, `users.prefix` and `task_scopes.key_prefix`
                // must remain identical. A later rebrand needs an explicit
                // migration command, not `set-prefix`.
                let any_active_scope: bool = tx.query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM task_scopes
                        WHERE owner_runtime_user_id = ?1
                          AND kind = 'personal'
                          AND status != 'deleted'
                    )",
                    [&user_id],
                    |r| r.get(0),
                )?;
                if any_active_scope {
                    tx.commit()?;
                    return Ok::<_, BoxErr>(Ok(Outcome::Locked));
                }

                // Phase 4 lock-extension (server#130): an empty-account
                // backfill marks `task_keys_migrated_at` without
                // creating allocation rows. The contract in
                // `task-write-contract.md` § Set-prefix immutability
                // and `migrations/026 § Immutability rule` both call
                // out `task_keys_migrated_at IS NOT NULL` as a lock
                // trigger; without this branch a fresh user could be
                // re-prefixed after first access.
                let migrated_at: Option<Option<String>> = tx
                    .query_row(
                        "SELECT task_keys_migrated_at FROM users WHERE id = ?1",
                        [&user_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if matches!(migrated_at, Some(Some(_))) {
                    tx.commit()?;
                    return Ok::<_, BoxErr>(Ok(Outcome::Locked));
                }

                // Inner result so the UNIQUE constraint failure propagates
                // through the outer call's anyhow chain to
                // store_err_from_anyhow.
                let update_result = tx.execute(
                    "UPDATE users SET prefix = ?2 WHERE id = ?1",
                    rusqlite::params![user_id, prefix],
                );
                match update_result {
                    Ok(_) => {
                        tx.commit()?;
                        Ok::<_, BoxErr>(Ok(Outcome::Updated))
                    }
                    Err(e) => {
                        // Roll back implicitly when tx drops; surface the
                        // error so unique_resource_from_chain can extract
                        // the resource label.
                        Ok::<_, BoxErr>(Err(e))
                    }
                }
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;

        match outcome {
            Ok(Outcome::Updated) => Ok(()),
            Ok(Outcome::UserMissing) => Err(StoreError::Other(anyhow::anyhow!(
                "user {user_id_owned} does not exist"
            ))),
            Ok(Outcome::Locked) => Err(StoreError::PrefixLocked),
            Err(rusqlite_err) => {
                // Re-route through the standard mapping so a UNIQUE
                // collision becomes Constraint(Unique{USERS_PREFIX}).
                let anyhow_err = anyhow::anyhow!(rusqlite_err);
                Err(store_err_from_anyhow(anyhow_err))
            }
        }
    }

    pub(super) async fn users_without_prefix_impl(
        &self,
    ) -> Result<Vec<UserWithoutPrefix>, StoreError> {
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, username FROM users
                     WHERE prefix IS NULL
                     ORDER BY created_at, id",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok(UserWithoutPrefix {
                            id: row.get(0)?,
                            username: row.get(1)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(result)
    }

    pub(super) async fn backfill_task_key_allocation_task_scope_ids_impl(
        &self,
    ) -> Result<usize, StoreError> {
        let rows = self
            .conn
            .call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let has_missing: bool = tx.query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM task_key_allocations
                        WHERE task_scope_id IS NULL
                        LIMIT 1
                     )",
                    [],
                    |row| row.get(0),
                )?;
                if !has_missing {
                    tx.commit()?;
                    return Ok::<_, BoxErr>(0);
                }

                let n = tx.execute(
                    "UPDATE task_key_allocations
                     SET task_scope_id = (
                         SELECT task_scopes.id
                         FROM task_scopes
                         WHERE task_scopes.kind = 'personal'
                           AND task_scopes.status = 'active'
                           AND task_scopes.owner_runtime_user_id = task_key_allocations.user_id
                           AND task_scopes.key_prefix = task_key_allocations.prefix
                     )
                     WHERE task_scope_id IS NULL
                       AND EXISTS (
                         SELECT 1 FROM task_scopes
                         WHERE task_scopes.kind = 'personal'
                           AND task_scopes.status = 'active'
                           AND task_scopes.owner_runtime_user_id = task_key_allocations.user_id
                           AND task_scopes.key_prefix = task_key_allocations.prefix
                       )",
                    [],
                )?;
                tx.commit()?;
                Ok::<_, BoxErr>(n)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(rows)
    }

    pub(super) async fn count_task_key_allocations_missing_task_scope_id_impl(
        &self,
    ) -> Result<usize, StoreError> {
        let count = self
            .conn
            .call(move |conn| {
                let n: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM task_key_allocations WHERE task_scope_id IS NULL",
                    [],
                    |row| row.get(0),
                )?;
                Ok::<_, BoxErr>(n as usize)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(count)
    }

    async fn count_task_key_allocations_missing_task_scope_id_for_user_impl(
        &self,
        user_id: &str,
    ) -> Result<usize, StoreError> {
        let user_id = user_id.to_string();
        let count = self
            .conn
            .call(move |conn| {
                let n: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM task_key_allocations
                     WHERE user_id = ?1 AND task_scope_id IS NULL",
                    [&user_id],
                    |row| row.get(0),
                )?;
                Ok::<_, BoxErr>(n as usize)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(count)
    }

    pub(super) async fn lookup_task_uuid_by_task_scope_key_impl(
        &self,
        task_scope_id: &str,
        n: i64,
    ) -> Result<Option<String>, StoreError> {
        let task_scope_id = task_scope_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row: Option<String> = conn
                    .query_row(
                        "SELECT task_uuid FROM task_key_allocations
                         WHERE task_scope_id = ?1 AND n = ?2
                               AND state = 'committed'",
                        rusqlite::params![task_scope_id, n],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(result)
    }

    pub(super) async fn lookup_task_uuid_by_key_impl(
        &self,
        user_id: &str,
        prefix: &str,
        n: i64,
    ) -> Result<Option<String>, StoreError> {
        let missing = self
            .count_task_key_allocations_missing_task_scope_id_for_user_impl(user_id)
            .await?;
        if missing > 0 {
            return Err(StoreError::MissingTaskScopeId {
                user_id: user_id.to_string(),
                count: missing,
            });
        }

        let Some(scope) = self
            .lookup_task_scope_by_prefix_for_user_impl(user_id, prefix)
            .await?
        else {
            return Ok(None);
        };

        self.lookup_task_uuid_by_task_scope_key_impl(&scope.id, n)
            .await
    }

    pub(super) async fn lookup_task_key_by_uuid_impl(
        &self,
        user_id: &str,
        task_uuid: &str,
    ) -> Result<Option<(String, KeyState)>, StoreError> {
        let user_id = user_id.to_string();
        let task_uuid = task_uuid.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row: Option<(String, i64, String)> = conn
                    .query_row(
                        "SELECT prefix, n, state FROM task_key_allocations
                         WHERE user_id = ?1 AND task_uuid = ?2
                               AND state IN ('pending', 'committed')",
                        rusqlite::params![user_id, task_uuid],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;

        Ok(result.and_then(|(prefix, n, state_str)| {
            KeyState::from_db_str(&state_str).map(|state| (format!("{prefix}-{n}"), state))
        }))
    }

    pub(super) async fn get_user_task_keys_migrated_at_impl(
        &self,
        user_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row: Option<Option<String>> = conn
                    .query_row(
                        "SELECT task_keys_migrated_at FROM users WHERE id = ?1",
                        [&user_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row.flatten())
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(result)
    }

    pub(super) async fn mark_user_task_keys_migrated_impl(
        &self,
        user_id: &str,
    ) -> Result<(), StoreError> {
        let user_id = user_id.to_string();
        let rows: usize = self
            .conn
            .call(move |conn| {
                let n = conn.execute(
                    "UPDATE users
                        SET task_keys_migrated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                      WHERE id = ?1",
                    [&user_id],
                )?;
                Ok::<_, BoxErr>(n)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        if rows == 1 {
            Ok(())
        } else {
            // The empty-candidate branch in the backfill flow calls this
            // primitive directly, so the same delete_user-during-backfill
            // race that `commit_backfill_allocations_for_user` defends
            // against could leak through here without the rows-affected
            // check. Rejecting with `BackfillUserMissing` keeps the two
            // commit shapes symmetric.
            Err(StoreError::BackfillUserMissing)
        }
    }

    pub(super) async fn max_n_for_user_prefix_impl(
        &self,
        user_id: &str,
        prefix: &str,
    ) -> Result<i64, StoreError> {
        let user_id = user_id.to_string();
        let prefix = prefix.to_string();
        let result: i64 = self
            .conn
            .call(move |conn| {
                let max_n: i64 = conn.query_row(
                    "SELECT COALESCE(MAX(n), 0) FROM task_key_allocations
                     WHERE user_id = ?1 AND prefix = ?2",
                    rusqlite::params![user_id, prefix],
                    |row| row.get(0),
                )?;
                Ok::<_, BoxErr>(max_n)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(result)
    }

    pub(super) async fn commit_backfill_allocations_for_user_impl(
        &self,
        user_id: &str,
        prefix: &str,
        expected_max_n: i64,
        task_uuids_in_order: &[String],
    ) -> Result<Vec<(String, i64)>, StoreError> {
        let user_id = user_id.to_string();
        let prefix = prefix.to_string();
        let prefix_for_err = prefix.clone();
        let task_uuids: Vec<String> = task_uuids_in_order.to_vec();

        // The closure can return three flavours of error: rusqlite errors
        // (transport / constraint), a "user missing" sentinel, and a
        // "max changed" sentinel with the observed value. We classify
        // post-call so the right `StoreError` variant surfaces to the
        // caller without losing the original error chain on transport
        // failures.
        enum BackfillCommitErr {
            UserMissing,
            MaxChanged { actual: i64 },
            PrefixChanged { actual: Option<String> },
        }

        let outcome: Result<Vec<(String, i64)>, BackfillCommitErr> = self
            .conn
            .call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                let user_row: Option<Option<String>> = tx
                    .query_row("SELECT prefix FROM users WHERE id = ?1", [&user_id], |r| {
                        r.get(0)
                    })
                    .optional()?;
                let current_prefix: Option<String> = match user_row {
                    None => {
                        // Implicit rollback on tx drop.
                        return Ok::<_, BoxErr>(Err(BackfillCommitErr::UserMissing));
                    }
                    Some(p) => p,
                };
                if current_prefix.as_deref() != Some(prefix.as_str()) {
                    return Ok::<_, BoxErr>(Err(BackfillCommitErr::PrefixChanged {
                        actual: current_prefix,
                    }));
                }

                let task_scope_id = active_personal_task_scope_id(&tx, &user_id, &prefix)?
                    .ok_or_else(|| missing_task_scope_err(&user_id, &prefix))?;

                let observed_max_n: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(n), 0) FROM task_key_allocations
                     WHERE user_id = ?1 AND prefix = ?2",
                    rusqlite::params![user_id, prefix],
                    |row| row.get(0),
                )?;
                if observed_max_n != expected_max_n {
                    return Ok::<_, BoxErr>(Err(BackfillCommitErr::MaxChanged {
                        actual: observed_max_n,
                    }));
                }

                let mut pairs: Vec<(String, i64)> = Vec::with_capacity(task_uuids.len());
                for (i, task_uuid) in task_uuids.iter().enumerate() {
                    let n = expected_max_n + (i as i64) + 1;
                    let attempt_id = Uuid::new_v4().to_string();
                    tx.execute(
                        "INSERT INTO task_key_allocations
                            (user_id, prefix, n, task_scope_id, task_uuid, state, attempt_id, committed_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'committed', ?6,
                                 strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                        rusqlite::params![user_id, prefix, n, task_scope_id, task_uuid, attempt_id],
                    )?;
                    pairs.push((task_uuid.clone(), n));
                }

                let users_updated = tx.execute(
                    "UPDATE users
                        SET task_keys_migrated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                      WHERE id = ?1",
                    [&user_id],
                )?;
                if users_updated != 1 {
                    // Defence-in-depth: the EXISTS check above should
                    // have caught a missing user; if a foreign-key
                    // cascade or other oddity shows zero rows here we
                    // still bail rather than orphan the allocations.
                    return Ok::<_, BoxErr>(Err(BackfillCommitErr::UserMissing));
                }

                tx.commit()?;
                Ok::<_, BoxErr>(Ok(pairs))
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;

        match outcome {
            Ok(pairs) => Ok(pairs),
            Err(BackfillCommitErr::UserMissing) => Err(StoreError::BackfillUserMissing),
            Err(BackfillCommitErr::MaxChanged { actual }) => Err(StoreError::BackfillMaxChanged {
                expected: expected_max_n,
                actual,
            }),
            Err(BackfillCommitErr::PrefixChanged { actual }) => {
                Err(StoreError::BackfillPrefixChanged {
                    expected: prefix_for_err,
                    actual,
                })
            }
        }
    }

    pub(super) async fn list_pending_attached_task_keys_for_user_impl(
        &self,
        user_id: &str,
    ) -> Result<Vec<crate::store::models::PendingAttachedKey>, StoreError> {
        let user_id = user_id.to_string();
        let result: Vec<crate::store::models::PendingAttachedKey> = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT prefix, n, attempt_id, task_uuid
                     FROM task_key_allocations
                     WHERE user_id = ?1
                           AND state = 'pending'
                           AND task_uuid IS NOT NULL
                     ORDER BY prefix, n",
                )?;
                let rows = stmt
                    .query_map([&user_id], |row| {
                        Ok(crate::store::models::PendingAttachedKey {
                            prefix: row.get(0)?,
                            n: row.get(1)?,
                            attempt_id: row.get(2)?,
                            task_uuid: row.get(3)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(result)
    }

    pub(super) async fn lookup_task_keys_by_uuids_impl(
        &self,
        user_id: &str,
        task_uuids: &[String],
    ) -> Result<HashMap<String, String>, StoreError> {
        if task_uuids.is_empty() {
            return Ok(HashMap::new());
        }

        let user_id = user_id.to_string();
        let chunks: Vec<Vec<String>> = task_uuids
            .chunks(UUID_CHUNK_SIZE)
            .map(|c| c.to_vec())
            .collect();

        let mut out: HashMap<String, String> = HashMap::with_capacity(task_uuids.len());

        for chunk in chunks {
            let user_id = user_id.clone();
            let chunk_for_query = chunk.clone();
            let rows: Vec<(String, String, i64)> = self
                .conn
                .call(move |conn| {
                    // Build the placeholders dynamically — the chunk size
                    // is bounded by UUID_CHUNK_SIZE so allocation cost is
                    // a one-shot per-chunk format.
                    let placeholders: Vec<String> = (1..=chunk_for_query.len())
                        .map(|i| format!("?{}", i + 1))
                        .collect();
                    let sql = format!(
                        "SELECT task_uuid, prefix, n FROM task_key_allocations
                         WHERE user_id = ?1 AND state = 'committed'
                               AND task_uuid IN ({})",
                        placeholders.join(", ")
                    );

                    let mut params: Vec<&dyn rusqlite::ToSql> =
                        Vec::with_capacity(chunk_for_query.len() + 1);
                    params.push(&user_id);
                    for u in &chunk_for_query {
                        params.push(u);
                    }

                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt
                        .query_map(params.as_slice(), |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, i64>(2)?,
                            ))
                        })?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok::<_, BoxErr>(rows)
                })
                .await
                .map_err(map_err)
                .map_err(store_err_from_anyhow)?;

            for (uuid, prefix, n) in rows {
                out.insert(uuid, format!("{prefix}-{n}"));
            }
        }

        Ok(out)
    }

    pub(super) async fn lookup_task_keys_for_projection_by_task_scope_impl(
        &self,
        task_scope_id: &str,
        task_uuids: &[String],
        now_unix_seconds: i64,
        pending_timeout_seconds: u32,
    ) -> Result<HashMap<String, String>, StoreError> {
        if task_uuids.is_empty() {
            return Ok(HashMap::new());
        }

        let task_scope_id = task_scope_id.to_string();
        let chunks: Vec<Vec<String>> = task_uuids
            .chunks(UUID_CHUNK_SIZE)
            .map(|c| c.to_vec())
            .collect();

        let mut out: HashMap<String, String> = HashMap::with_capacity(task_uuids.len());

        for chunk in chunks {
            let task_scope_id = task_scope_id.clone();
            let chunk_for_query = chunk.clone();
            let pending_timeout_i64 = i64::from(pending_timeout_seconds);
            // Lookup-time expiry rule: include `committed` rows always,
            // and `pending` rows whose `created_at + pending_timeout >
            // now_unix_seconds`. Same expiry comparison shape as the
            // reaper's `select_stale_pending_task_keys` (inverted
            // predicate: reaper selects expired rows; projection
            // selects non-expired rows).
            let rows: Vec<(String, String, i64)> = self
                .conn
                .call(move |conn| {
                    let placeholders: Vec<String> = (1..=chunk_for_query.len())
                        .map(|i| format!("?{}", i + 3))
                        .collect();
                    let sql = format!(
                        "SELECT task_uuid, prefix, n FROM task_key_allocations
                         WHERE task_scope_id = ?3
                               AND task_uuid IN ({})
                               AND (
                                   state = 'committed'
                                   OR (
                                       state = 'pending'
                                       AND CAST(strftime('%s', created_at) AS INTEGER)
                                           >= (?1 - ?2)
                                   )
                               )",
                        placeholders.join(", ")
                    );

                    let mut params: Vec<&dyn rusqlite::ToSql> =
                        Vec::with_capacity(chunk_for_query.len() + 3);
                    params.push(&now_unix_seconds);
                    params.push(&pending_timeout_i64);
                    params.push(&task_scope_id);
                    for u in &chunk_for_query {
                        params.push(u);
                    }

                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt
                        .query_map(params.as_slice(), |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, i64>(2)?,
                            ))
                        })?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok::<_, BoxErr>(rows)
                })
                .await
                .map_err(map_err)
                .map_err(store_err_from_anyhow)?;

            for (uuid, prefix, n) in rows {
                out.insert(uuid, format!("{prefix}-{n}"));
            }
        }

        Ok(out)
    }

    pub(super) async fn lookup_task_keys_for_projection_impl(
        &self,
        user_id: &str,
        task_uuids: &[String],
        now_unix_seconds: i64,
        pending_timeout_seconds: u32,
    ) -> Result<HashMap<String, String>, StoreError> {
        if task_uuids.is_empty() {
            return Ok(HashMap::new());
        }

        let missing = self
            .count_task_key_allocations_missing_task_scope_id_for_user_impl(user_id)
            .await?;
        if missing > 0 {
            return Err(StoreError::MissingTaskScopeId {
                user_id: user_id.to_string(),
                count: missing,
            });
        }

        let Some(prefix) = self.get_user_prefix_impl(user_id).await? else {
            return Ok(HashMap::new());
        };
        let Some(scope) = self
            .lookup_task_scope_by_prefix_for_user_impl(user_id, &prefix)
            .await?
        else {
            return Ok(HashMap::new());
        };

        self.lookup_task_keys_for_projection_by_task_scope_impl(
            &scope.id,
            task_uuids,
            now_unix_seconds,
            pending_timeout_seconds,
        )
        .await
    }

    pub(super) async fn lookup_task_keys_for_drift_impl(
        &self,
        user_id: &str,
        task_uuids: &[String],
    ) -> Result<Vec<DriftAllocationRow>, StoreError> {
        if task_uuids.is_empty() {
            return Ok(Vec::new());
        }

        let user_id = user_id.to_string();
        let chunks: Vec<Vec<String>> = task_uuids
            .chunks(UUID_CHUNK_SIZE)
            .map(|c| c.to_vec())
            .collect();

        let mut out: Vec<DriftAllocationRow> = Vec::with_capacity(task_uuids.len());

        for chunk in chunks {
            let user_id = user_id.clone();
            let chunk_for_query = chunk.clone();
            let rows: Vec<(String, String, i64, String, String, String)> = self
                .conn
                .call(move |conn| {
                    let placeholders: Vec<String> = (1..=chunk_for_query.len())
                        .map(|i| format!("?{}", i + 1))
                        .collect();
                    let sql = format!(
                        "SELECT task_uuid, prefix, n, state, attempt_id, created_at
                           FROM task_key_allocations
                          WHERE user_id = ?1
                            AND state IN ('pending', 'committed')
                            AND task_uuid IN ({})",
                        placeholders.join(", ")
                    );

                    let mut params: Vec<&dyn rusqlite::ToSql> =
                        Vec::with_capacity(chunk_for_query.len() + 1);
                    params.push(&user_id);
                    for u in &chunk_for_query {
                        params.push(u);
                    }

                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt
                        .query_map(params.as_slice(), |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, String>(5)?,
                            ))
                        })?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok::<_, BoxErr>(rows)
                })
                .await
                .map_err(map_err)
                .map_err(store_err_from_anyhow)?;

            for (task_uuid, prefix, n, state_str, attempt_id, created_at) in rows {
                let Some(state) = KeyState::from_db_str(&state_str) else {
                    // Defence-in-depth: schema CHECK already prevents this
                    // and the SQL filter excludes 'burned'. An unexpected
                    // value here would mean DB corruption — skip the row
                    // rather than panic.
                    continue;
                };
                out.push(DriftAllocationRow {
                    task_uuid,
                    prefix: prefix.clone(),
                    key: format!("{prefix}-{n}"),
                    state,
                    attempt_id,
                    created_at,
                });
            }
        }

        Ok(out)
    }
}
