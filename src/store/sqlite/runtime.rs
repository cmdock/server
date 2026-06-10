use rusqlite::OptionalExtension;

use crate::{
    runtime_policy::RuntimePolicy,
    store::models::{ReplicaRecord, RuntimePolicyRecord, UserRecord},
};

use super::{map_err, BoxErr, SqliteConfigStore};

fn runtime_policy_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RuntimePolicyRecord> {
    let desired_policy_json: String = row.get(2)?;
    let applied_policy_json: Option<String> = row.get(4)?;

    Ok(RuntimePolicyRecord {
        user_id: row.get(0)?,
        desired_version: row.get(1)?,
        desired_policy: serde_json::from_str(&desired_policy_json).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(err))
        })?,
        applied_version: row.get(3)?,
        applied_policy: applied_policy_json
            .map(|json| {
                serde_json::from_str(&json).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })
            })
            .transpose()?,
        applied_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn replica_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReplicaRecord> {
    Ok(ReplicaRecord {
        id: row.get(0)?,
        user_id: row.get(1)?,
        encryption_secret_enc: row.get(2)?,
        label: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn user_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserRecord> {
    Ok(UserRecord {
        id: row.get(0)?,
        username: row.get(1)?,
        password_hash: row.get(2)?,
        created_at: row.get(3)?,
    })
}

impl SqliteConfigStore {
    pub(super) async fn get_runtime_policy_impl(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Option<RuntimePolicyRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT user_id, desired_version, desired_policy_json,
                                applied_version, applied_policy_json, applied_at, updated_at
                         FROM user_runtime_policies
                         WHERE user_id = ?1",
                        [&user_id],
                        runtime_policy_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn upsert_runtime_policy_impl(
        &self,
        user_id: &str,
        desired_version: &str,
        desired_policy: &RuntimePolicy,
        applied_version: Option<&str>,
        applied_policy: Option<&RuntimePolicy>,
        applied_at: Option<&str>,
    ) -> anyhow::Result<RuntimePolicyRecord> {
        let user_id = user_id.to_string();
        let desired_version = desired_version.to_string();
        let desired_policy_json =
            serde_json::to_string(desired_policy).map_err(|err| anyhow::anyhow!(err))?;
        let applied_version = applied_version.map(|value| value.to_string());
        let applied_policy_json = applied_policy
            .map(serde_json::to_string)
            .transpose()
            .map_err(|err| anyhow::anyhow!(err))?;
        let applied_at = applied_at.map(|value| value.to_string());

        let result = self
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO user_runtime_policies (
                        user_id, desired_version, desired_policy_json,
                        applied_version, applied_policy_json, applied_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))
                     ON CONFLICT (user_id) DO UPDATE SET
                        desired_version = excluded.desired_version,
                        desired_policy_json = excluded.desired_policy_json,
                        applied_version = excluded.applied_version,
                        applied_policy_json = excluded.applied_policy_json,
                        applied_at = excluded.applied_at,
                        updated_at = datetime('now')",
                    rusqlite::params![
                        user_id,
                        desired_version,
                        desired_policy_json,
                        applied_version,
                        applied_policy_json,
                        applied_at,
                    ],
                )?;

                let row = conn.query_row(
                    "SELECT user_id, desired_version, desired_policy_json,
                            applied_version, applied_policy_json, applied_at, updated_at
                     FROM user_runtime_policies
                     WHERE user_id = ?1",
                    rusqlite::params![user_id],
                    runtime_policy_from_row,
                )?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn create_replica_impl(
        &self,
        user_id: &str,
        client_id: &str,
        encryption_secret_enc: &str,
    ) -> anyhow::Result<()> {
        let client_id = client_id.to_string();
        let user_id = user_id.to_string();
        let enc = encryption_secret_enc.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO replicas (id, user_id, encryption_secret_enc) VALUES (?1, ?2, ?3)",
                    rusqlite::params![client_id, user_id, enc],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn get_replica_by_user_impl(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Option<ReplicaRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let result = conn.query_row(
                    "SELECT id, user_id, encryption_secret_enc, label, created_at
                     FROM replicas WHERE user_id = ?1",
                    [&user_id],
                    replica_from_row,
                );
                match result {
                    Ok(r) => Ok::<_, BoxErr>(Some(r)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn get_replica_by_client_id_impl(
        &self,
        client_id: &str,
    ) -> anyhow::Result<Option<ReplicaRecord>> {
        let client_id = client_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let result = conn.query_row(
                    "SELECT id, user_id, encryption_secret_enc, label, created_at
                     FROM replicas WHERE id = ?1",
                    [&client_id],
                    replica_from_row,
                );
                match result {
                    Ok(r) => Ok::<_, BoxErr>(Some(r)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn get_user_by_client_id_impl(
        &self,
        client_id: &str,
    ) -> anyhow::Result<Option<UserRecord>> {
        let client_id = client_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let result = conn.query_row(
                    "SELECT u.id, u.username, u.password_hash, u.created_at
                     FROM replicas r JOIN users u ON r.user_id = u.id
                     WHERE r.id = ?1",
                    [&client_id],
                    user_from_row,
                );
                match result {
                    Ok(user) => Ok::<_, BoxErr>(Some(user)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn delete_replica_impl(&self, user_id: &str) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let count = conn.execute("DELETE FROM replicas WHERE user_id = ?1", [&user_id])?;
                Ok::<_, BoxErr>(count > 0)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }
}
