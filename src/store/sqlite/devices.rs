use rusqlite::OptionalExtension;

use crate::store::models::DeviceRecord;

use super::{map_err, BoxErr, SqliteConfigStore};

fn device_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRecord> {
    Ok(DeviceRecord {
        client_id: row.get(0)?,
        user_id: row.get(1)?,
        name: row.get(2)?,
        encryption_secret_enc: row.get(3)?,
        registered_at: row.get(4)?,
        last_sync_at: row.get(5)?,
        last_sync_ip: row.get(6)?,
        status: row.get(7)?,
        bootstrap_request_id: row.get(8)?,
        bootstrap_status: row.get(9)?,
        bootstrap_requested_username: row.get(10)?,
        bootstrap_create_user_if_missing: row.get::<_, Option<i64>>(11)?.map(|v| v != 0),
        bootstrap_expires_at: row.get(12)?,
    })
}

impl SqliteConfigStore {
    pub(super) async fn list_devices_impl(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Vec<DeviceRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT client_id, user_id, name, encryption_secret_enc,
                            registered_at, last_sync_at, last_sync_ip, status,
                            bootstrap_request_id, bootstrap_status,
                            bootstrap_requested_username, bootstrap_create_user_if_missing,
                            bootstrap_expires_at
                     FROM devices WHERE user_id = ?1 ORDER BY registered_at",
                )?;
                let rows = stmt
                    .query_map([&user_id], device_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn get_device_impl(
        &self,
        client_id: &str,
    ) -> anyhow::Result<Option<DeviceRecord>> {
        let client_id = client_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT client_id, user_id, name, encryption_secret_enc,
                                registered_at, last_sync_at, last_sync_ip, status,
                                bootstrap_request_id, bootstrap_status,
                                bootstrap_requested_username, bootstrap_create_user_if_missing,
                                bootstrap_expires_at
                         FROM devices WHERE client_id = ?1",
                        [&client_id],
                        device_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn get_device_by_bootstrap_request_impl(
        &self,
        bootstrap_request_id: &str,
    ) -> anyhow::Result<Option<DeviceRecord>> {
        let bootstrap_request_id = bootstrap_request_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT client_id, user_id, name, encryption_secret_enc,
                                registered_at, last_sync_at, last_sync_ip, status,
                                bootstrap_request_id, bootstrap_status,
                                bootstrap_requested_username, bootstrap_create_user_if_missing,
                                bootstrap_expires_at
                         FROM devices WHERE bootstrap_request_id = ?1",
                        [&bootstrap_request_id],
                        device_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn create_device_impl(
        &self,
        user_id: &str,
        client_id: &str,
        name: &str,
        encryption_secret_enc: Option<&str>,
    ) -> anyhow::Result<()> {
        let user_id = user_id.to_string();
        let client_id = client_id.to_string();
        let name = name.to_string();
        let enc = encryption_secret_enc.map(|s| s.to_string());
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO devices (client_id, user_id, name, encryption_secret_enc)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![client_id, user_id, name, enc],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    // Mirrors the trait signature; collapse into a struct when the trait does.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn create_bootstrap_device_impl(
        &self,
        user_id: &str,
        client_id: &str,
        name: &str,
        encryption_secret_enc: &str,
        bootstrap_request_id: &str,
        bootstrap_requested_username: Option<&str>,
        bootstrap_create_user_if_missing: bool,
        bootstrap_expires_at: &str,
    ) -> anyhow::Result<()> {
        let user_id = user_id.to_string();
        let client_id = client_id.to_string();
        let name = name.to_string();
        let encryption_secret_enc = encryption_secret_enc.to_string();
        let bootstrap_request_id = bootstrap_request_id.to_string();
        let bootstrap_requested_username = bootstrap_requested_username.map(|s| s.to_string());
        let bootstrap_expires_at = bootstrap_expires_at.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO devices (
                        client_id, user_id, name, encryption_secret_enc,
                        bootstrap_request_id, bootstrap_status,
                        bootstrap_requested_username, bootstrap_create_user_if_missing,
                        bootstrap_expires_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending_delivery', ?6, ?7, ?8)",
                    rusqlite::params![
                        client_id,
                        user_id,
                        name,
                        encryption_secret_enc,
                        bootstrap_request_id,
                        bootstrap_requested_username,
                        if bootstrap_create_user_if_missing {
                            1
                        } else {
                            0
                        },
                        bootstrap_expires_at,
                    ],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn update_device_name_impl(
        &self,
        user_id: &str,
        client_id: &str,
        name: &str,
    ) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let client_id = client_id.to_string();
        let name = name.to_string();
        let rows = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "UPDATE devices SET name = ?1 WHERE client_id = ?2 AND user_id = ?3",
                    rusqlite::params![name, client_id, user_id],
                )?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(rows > 0)
    }

    pub(super) async fn revoke_device_impl(
        &self,
        user_id: &str,
        client_id: &str,
    ) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let client_id = client_id.to_string();
        let rows = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "UPDATE devices SET status = 'revoked' WHERE client_id = ?1 AND user_id = ?2",
                    rusqlite::params![client_id, user_id],
                )?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(rows > 0)
    }

    pub(super) async fn unrevoke_device_impl(
        &self,
        user_id: &str,
        client_id: &str,
    ) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let client_id = client_id.to_string();
        let rows = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "UPDATE devices SET status = 'active' WHERE client_id = ?1 AND user_id = ?2",
                    rusqlite::params![client_id, user_id],
                )?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(rows > 0)
    }

    pub(super) async fn delete_device_impl(
        &self,
        user_id: &str,
        client_id: &str,
    ) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let client_id = client_id.to_string();
        let rows = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "DELETE FROM devices WHERE client_id = ?1 AND user_id = ?2",
                    rusqlite::params![client_id, user_id],
                )?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(rows > 0)
    }

    pub(super) async fn acknowledge_bootstrap_device_impl(
        &self,
        bootstrap_request_id: &str,
    ) -> anyhow::Result<bool> {
        let bootstrap_request_id = bootstrap_request_id.to_string();
        let rows = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "UPDATE devices
                     SET bootstrap_status = 'acknowledged'
                     WHERE bootstrap_request_id = ?1
                       AND status = 'active'
                       AND (
                           bootstrap_status IS NULL
                           OR bootstrap_status != 'pending_delivery'
                           OR bootstrap_expires_at IS NULL
                           OR bootstrap_expires_at > datetime('now')
                       )
                       AND COALESCE(bootstrap_status, '') != 'abandoned'",
                    rusqlite::params![bootstrap_request_id],
                )?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(rows > 0)
    }

    pub(super) async fn touch_device_impl(&self, client_id: &str, ip: &str) -> anyhow::Result<()> {
        let client_id = client_id.to_string();
        let ip = ip.to_string();
        self.conn
            .call(move |conn| {
                let rows = conn.execute(
                    "UPDATE devices
                     SET last_sync_at = datetime('now'),
                         last_sync_ip = ?1,
                         bootstrap_status = CASE
                             WHEN bootstrap_status = 'pending_delivery' THEN 'acknowledged'
                             ELSE bootstrap_status
                         END
                     WHERE client_id = ?2
                       AND status = 'active'
                       AND COALESCE(bootstrap_status, '') != 'abandoned'",
                    rusqlite::params![ip, client_id],
                )?;
                if rows == 0 {
                    return Err("device touch matched no active device rows".into());
                }
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }
}
