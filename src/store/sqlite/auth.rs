use base64::Engine;
use rusqlite::OptionalExtension;
use uuid::Uuid;

use crate::store::models::{
    ApiTokenRecord, IssuedApiToken, LabeledTokenCorrelation, NewUser, TokenUseRecord, UserRecord,
};

use super::{delete_user_owned_rows, hash_token, map_err, BoxErr, SqliteConfigStore};

fn user_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserRecord> {
    Ok(UserRecord {
        id: row.get(0)?,
        username: row.get(1)?,
        password_hash: row.get(2)?,
        created_at: row.get(3)?,
    })
}

fn api_token_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiTokenRecord> {
    Ok(ApiTokenRecord {
        token_hash: row.get(0)?,
        user_id: row.get(1)?,
        label: row.get(2)?,
        token_id: row.get(3)?,
        expires_at: row.get(4)?,
        created_at: row.get(5)?,
        first_used_at: row.get(6)?,
        last_used_at: row.get(7)?,
        last_used_ip: row.get(8)?,
    })
}

impl SqliteConfigStore {
    pub(super) async fn get_user_by_token_impl(
        &self,
        token: &str,
    ) -> anyhow::Result<Option<UserRecord>> {
        let token_hash = hash_token(token);
        let result = self
            .conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT u.id, u.username, u.password_hash, u.created_at
                         FROM api_tokens t
                         JOIN users u ON t.user_id = u.id
                         WHERE t.token_hash = ?1
                           AND (t.expires_at IS NULL OR t.expires_at > datetime('now'))",
                        [&token_hash],
                        user_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn get_user_by_id_impl(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Option<UserRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT id, username, password_hash, created_at FROM users WHERE id = ?1",
                        [&user_id],
                        user_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn get_user_by_username_impl(
        &self,
        username: &str,
    ) -> anyhow::Result<Option<UserRecord>> {
        let username = username.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT id, username, password_hash, created_at
                         FROM users WHERE username = ?1",
                        [&username],
                        user_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn list_users_impl(&self) -> anyhow::Result<Vec<UserRecord>> {
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, username, password_hash, created_at FROM users ORDER BY created_at",
                )?;
                let rows = stmt
                    .query_map([], user_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn create_user_impl(&self, user: &NewUser) -> anyhow::Result<UserRecord> {
        let id = Uuid::new_v4().to_string();
        let username = user.username.clone();
        let password_hash = user.password_hash.clone();

        let result = self
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO users (id, username, password_hash) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, username, password_hash],
                )?;
                let user = conn.query_row(
                    "SELECT id, username, password_hash, created_at FROM users WHERE id = ?1",
                    [&id],
                    user_from_row,
                )?;
                Ok::<_, BoxErr>(user)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn create_api_token_impl(
        &self,
        user_id: &str,
        label: Option<&str>,
    ) -> anyhow::Result<String> {
        self.create_api_token_with_expiry_impl(user_id, label, None, 32)
            .await
    }

    pub(super) async fn create_api_token_with_expiry_impl(
        &self,
        user_id: &str,
        label: Option<&str>,
        expires_at: Option<&str>,
        token_bytes: usize,
    ) -> anyhow::Result<String> {
        if token_bytes == 0 {
            anyhow::bail!("token_bytes must be greater than zero");
        }

        use rand::RngCore;
        let mut bytes = vec![0u8; token_bytes];
        rand::rng().fill_bytes(&mut bytes);
        let token = if token_bytes == 32 && expires_at.is_none() {
            hex::encode(&bytes)
        } else {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
        };
        let user_id = user_id.to_string();
        let label = label.map(|s| s.to_string());
        let expires_at = expires_at.map(|s| s.to_string());
        let token_hash = hash_token(&token);

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO api_tokens (token_hash, user_id, label, token_id, expires_at)
                     VALUES (?1, ?2, ?3, NULL, ?4)",
                    rusqlite::params![token_hash, user_id, label, expires_at],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;

        Ok(token)
    }

    pub(super) async fn create_labeled_api_token_impl(
        &self,
        user_id: &str,
        label: &str,
        token_id: &str,
        expires_at: &str,
        token_bytes: usize,
    ) -> anyhow::Result<IssuedApiToken> {
        if token_bytes == 0 {
            anyhow::bail!("token_bytes must be greater than zero");
        }

        use rand::RngCore;
        let mut bytes = vec![0u8; token_bytes];
        rand::rng().fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
        let token_hash = hash_token(&token);
        let credential_hash_prefix = token_hash[..8.min(token_hash.len())].to_string();
        let user_id = user_id.to_string();
        let label = label.to_string();
        let token_id_owned = token_id.to_string();
        let expires_at_owned = expires_at.to_string();

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO api_tokens (token_hash, user_id, label, token_id, expires_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![token_hash, user_id, label, token_id_owned, expires_at_owned],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;

        Ok(IssuedApiToken {
            token,
            token_id: token_id.to_string(),
            credential_hash_prefix,
            expires_at: expires_at.to_string(),
        })
    }

    pub(super) async fn lookup_token_correlation_impl(
        &self,
        token: &str,
        expected_label: &str,
    ) -> anyhow::Result<Option<LabeledTokenCorrelation>> {
        let token_hash = hash_token(token);
        let expected_label = expected_label.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT user_id, token_id, token_hash, expires_at,
                                CASE
                                    WHEN expires_at IS NOT NULL AND expires_at <= datetime('now') THEN 1
                                    ELSE 0
                                END AS is_expired
                         FROM api_tokens
                         WHERE token_hash = ?1 AND label = ?2",
                        rusqlite::params![token_hash, expected_label],
                        |row| {
                            Ok(LabeledTokenCorrelation {
                                user_id: row.get(0)?,
                                token_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                                credential_hash_prefix: row
                                    .get::<_, String>(2)?
                                    .chars()
                                    .take(8)
                                    .collect(),
                                expires_at: row.get(3)?,
                                is_expired: row.get::<_, i64>(4)? != 0,
                            })
                        },
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn delete_user_impl(&self, user_id: &str) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                for table in &[
                    "devices",
                    "api_tokens",
                    "user_runtime_policies",
                    "merged_sync_journal",
                    "webhooks",
                    "views",
                    "contexts",
                    "presets",
                    "stores",
                    "replicas",
                    "sync_clients",
                    "shopping_config",
                    "config",
                ] {
                    delete_user_owned_rows(&tx, table, &user_id).map_err(BoxErr::from)?;
                }
                // Idempotency dedup records can store response bodies
                // containing task data; delete-user must remove them
                // immediately rather than waiting for the 24h retention
                // pruner. (#114 codex iter2 finding.) Tolerate "no such
                // table" for unit-test setups using `run_migrations_inline`
                // that don't include this table.
                match tx.execute(
                    "DELETE FROM idempotency_records WHERE user_id = ?1",
                    [&user_id],
                ) {
                    Ok(_) => {}
                    Err(rusqlite::Error::SqliteFailure(_, Some(ref msg)))
                        if msg.contains("no such table") => {}
                    Err(e) => return Err(BoxErr::from(e)),
                }
                // Task-key allocations have no ON DELETE CASCADE — delete
                // explicitly so deleted users don't leave orphaned rows
                // that would block prefix reuse if the same user_id were
                // ever reissued (#130). Tolerate missing table for legacy
                // backups (DR scenario tests).
                match tx.execute(
                    "DELETE FROM task_key_allocations WHERE user_id = ?1",
                    [&user_id],
                ) {
                    Ok(_) => {}
                    Err(rusqlite::Error::SqliteFailure(_, Some(ref msg)))
                        if msg.contains("no such table") => {}
                    Err(e) => return Err(BoxErr::from(e)),
                }
                // Task Scope rows are referenced by S2 allocation rows, so
                // delete them after task_key_allocations. The users FK also
                // cascades, but explicit deletion keeps legacy/inline schemas
                // deterministic.
                delete_user_owned_rows(&tx, "task_scopes", &user_id).map_err(BoxErr::from)?;
                let count = tx.execute("DELETE FROM users WHERE id = ?1", [&user_id])?;
                tx.commit()?;
                Ok::<_, BoxErr>(count > 0)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn list_api_tokens_impl(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Vec<ApiTokenRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT token_hash, user_id, label, token_id, expires_at, created_at,
                            first_used_at, last_used_at, last_used_ip
                     FROM api_tokens WHERE user_id = ?1 ORDER BY created_at",
                )?;
                let rows = stmt
                    .query_map([&user_id], api_token_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn mark_token_used_impl(
        &self,
        token: &str,
        client_ip: &str,
        expected_label: &str,
    ) -> anyhow::Result<Option<TokenUseRecord>> {
        let token_hash = hash_token(token);
        let client_ip = client_ip.to_string();
        let expected_label = expected_label.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT label, first_used_at, user_id, token_id, token_hash, expires_at
                         FROM api_tokens
                         WHERE token_hash = ?1",
                        [&token_hash],
                        |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, Option<String>>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, Option<String>>(5)?,
                            ))
                        },
                    )
                    .optional()?;

                let Some((label, first_used_at, user_id, token_id, stored_hash, expires_at)) = row
                else {
                    return Ok::<_, BoxErr>(None);
                };

                // Preserve pre-refactor behaviour: only the canonical
                // label triggers the side-effect UPDATE. Regular API
                // tokens are read-only on this path, matching the
                // pre-refactor `record_connect_config_token_use_impl`
                // early-return.
                if label.as_deref() != Some(expected_label.as_str()) {
                    return Ok::<_, BoxErr>(None);
                }

                conn.execute(
                    "UPDATE api_tokens
                     SET first_used_at = COALESCE(first_used_at, datetime('now')),
                         last_used_at = datetime('now'),
                         last_used_ip = ?2
                     WHERE token_hash = ?1",
                    rusqlite::params![token_hash, client_ip],
                )?;

                Ok::<_, BoxErr>(Some(TokenUseRecord {
                    user_id,
                    token_id: token_id.unwrap_or_default(),
                    label,
                    credential_hash_prefix: stored_hash.chars().take(8).collect(),
                    expires_at,
                    was_first_use: first_used_at.is_none(),
                }))
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn revoke_api_token_impl(&self, token_hash: &str) -> anyhow::Result<bool> {
        let token_hash = token_hash.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let count = conn.execute(
                    "DELETE FROM api_tokens WHERE token_hash = ?1",
                    [&token_hash],
                )?;
                Ok::<_, BoxErr>(count > 0)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }
}
