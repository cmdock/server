use base64::Engine;
use rusqlite::OptionalExtension;
use uuid::Uuid;

use crate::store::models::{
    ApiTokenRecord, ConnectConfigIssuedToken, ConnectConfigTokenCorrelation, ConnectConfigTokenUse,
    NewUser, UserRecord,
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

    pub(super) async fn create_connect_config_token_impl(
        &self,
        user_id: &str,
        expires_at: &str,
        token_bytes: usize,
    ) -> anyhow::Result<ConnectConfigIssuedToken> {
        if token_bytes == 0 {
            anyhow::bail!("token_bytes must be greater than zero");
        }

        use rand::RngCore;
        let mut bytes = vec![0u8; token_bytes];
        rand::rng().fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
        let token_hash = hash_token(&token);
        let mut token_id_bytes = [0u8; 8];
        rand::rng().fill_bytes(&mut token_id_bytes);
        let token_id = format!("cc_{}", hex::encode(token_id_bytes));
        let credential_hash_prefix = token_hash[..8.min(token_hash.len())].to_string();
        let user_id = user_id.to_string();
        let expires_at_owned = expires_at.to_string();
        let insert_token_id = token_id.clone();

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO api_tokens (token_hash, user_id, label, token_id, expires_at)
                     VALUES (?1, ?2, 'connect-config', ?3, ?4)",
                    rusqlite::params![token_hash, user_id, insert_token_id, expires_at_owned],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;

        Ok(ConnectConfigIssuedToken {
            token,
            token_id,
            credential_hash_prefix,
            expires_at: expires_at.to_string(),
        })
    }

    pub(super) async fn lookup_connect_config_token_impl(
        &self,
        token: &str,
    ) -> anyhow::Result<Option<ConnectConfigTokenCorrelation>> {
        let token_hash = hash_token(token);
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
                         WHERE token_hash = ?1 AND label = 'connect-config'",
                        [&token_hash],
                        |row| {
                            Ok(ConnectConfigTokenCorrelation {
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

    pub(super) async fn record_connect_config_token_use_impl(
        &self,
        token: &str,
        client_ip: &str,
    ) -> anyhow::Result<ConnectConfigTokenUse> {
        let token_hash = hash_token(token);
        let client_ip = client_ip.to_string();
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
                    return Ok::<_, BoxErr>(ConnectConfigTokenUse::NotConnectConfig);
                };

                if label.as_deref() != Some("connect-config") {
                    return Ok::<_, BoxErr>(ConnectConfigTokenUse::NotConnectConfig);
                }

                conn.execute(
                    "UPDATE api_tokens
                     SET first_used_at = COALESCE(first_used_at, datetime('now')),
                         last_used_at = datetime('now'),
                         last_used_ip = ?2
                     WHERE token_hash = ?1",
                    rusqlite::params![token_hash, client_ip],
                )?;

                let correlation = ConnectConfigTokenCorrelation {
                    user_id,
                    token_id: token_id.unwrap_or_default(),
                    credential_hash_prefix: stored_hash.chars().take(8).collect(),
                    expires_at,
                    is_expired: false,
                };

                Ok::<_, BoxErr>(if first_used_at.is_none() {
                    ConnectConfigTokenUse::FirstUse(correlation)
                } else {
                    ConnectConfigTokenUse::RepeatUse(correlation)
                })
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
