use rusqlite::OptionalExtension;

use crate::store::models::{
    AdminWebhookRecord, NewAdminWebhookRecord, NewWebhookRecord, UpdateAdminWebhookRecord,
    UpdateWebhookRecord, WebhookDeliveryRecord, WebhookFailureState, WebhookRecord,
};

use super::{map_err, BoxErr, SqliteConfigStore};

fn webhook_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WebhookRecord> {
    let events_json: String = row.get(3)?;
    let modified_fields_json: Option<String> = row.get(4)?;

    Ok(WebhookRecord {
        id: row.get(0)?,
        user_id: row.get(1)?,
        url: row.get(2)?,
        events: serde_json::from_str(&events_json).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(err))
        })?,
        modified_fields: modified_fields_json
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
        name: row.get(5)?,
        enabled: row.get::<_, i64>(6)? != 0,
        consecutive_failures: row.get::<_, i64>(7)? as u32,
        secret_enc: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn admin_webhook_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AdminWebhookRecord> {
    let events_json: String = row.get(2)?;
    let modified_fields_json: Option<String> = row.get(3)?;

    Ok(AdminWebhookRecord {
        id: row.get(0)?,
        url: row.get(1)?,
        events: serde_json::from_str(&events_json).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(err))
        })?,
        modified_fields: modified_fields_json
            .map(|json| {
                serde_json::from_str(&json).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })
            })
            .transpose()?,
        name: row.get(4)?,
        enabled: row.get::<_, i64>(5)? != 0,
        consecutive_failures: row.get::<_, i64>(6)? as u32,
        secret_enc: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn webhook_delivery_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WebhookDeliveryRecord> {
    Ok(WebhookDeliveryRecord {
        delivery_id: row.get(0)?,
        webhook_id: row.get(1)?,
        event_id: row.get(2)?,
        event: row.get(3)?,
        timestamp: row.get(4)?,
        status: row.get(5)?,
        response_status: row.get::<_, Option<i64>>(6)?.map(|value| value as u16),
        attempt: row.get::<_, i64>(7)? as u32,
        failure_reason: row.get(8)?,
    })
}

impl SqliteConfigStore {
    pub(super) async fn list_webhooks_impl(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Vec<WebhookRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, user_id, url, events_json, modified_fields_json, name,
                            enabled, consecutive_failures, secret_enc, created_at, updated_at
                     FROM webhooks
                     WHERE user_id = ?1
                     ORDER BY created_at, id",
                )?;
                let rows = stmt
                    .query_map([&user_id], webhook_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn get_webhook_impl(
        &self,
        user_id: &str,
        webhook_id: &str,
    ) -> anyhow::Result<Option<WebhookRecord>> {
        let user_id = user_id.to_string();
        let webhook_id = webhook_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT id, user_id, url, events_json, modified_fields_json, name,
                                enabled, consecutive_failures, secret_enc, created_at, updated_at
                         FROM webhooks
                         WHERE user_id = ?1 AND id = ?2",
                        rusqlite::params![user_id, webhook_id],
                        webhook_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn create_webhook_impl(
        &self,
        webhook: &NewWebhookRecord,
    ) -> anyhow::Result<WebhookRecord> {
        let webhook = webhook.clone();
        let result = self
            .conn
            .call(move |conn| {
                let events_json = serde_json::to_string(&webhook.events)?;
                let modified_fields_json = webhook
                    .modified_fields
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?;

                conn.execute(
                    "INSERT INTO webhooks (
                        id, user_id, url, events_json, modified_fields_json, name,
                        enabled, consecutive_failures, secret_enc
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)",
                    rusqlite::params![
                        webhook.id,
                        webhook.user_id,
                        webhook.url,
                        events_json,
                        modified_fields_json,
                        webhook.name,
                        if webhook.enabled { 1 } else { 0 },
                        webhook.secret_enc,
                    ],
                )?;

                let row = conn.query_row(
                    "SELECT id, user_id, url, events_json, modified_fields_json, name,
                            enabled, consecutive_failures, secret_enc, created_at, updated_at
                     FROM webhooks
                     WHERE id = ?1",
                    [&webhook.id],
                    webhook_from_row,
                )?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn update_webhook_impl(
        &self,
        webhook: &UpdateWebhookRecord,
    ) -> anyhow::Result<Option<WebhookRecord>> {
        let webhook = webhook.clone();
        let result = self
            .conn
            .call(move |conn| {
                let existing_secret: Option<String> = conn
                    .query_row(
                        "SELECT secret_enc
                         FROM webhooks
                         WHERE user_id = ?1 AND id = ?2",
                        rusqlite::params![webhook.user_id, webhook.id],
                        |row| row.get(0),
                    )
                    .optional()?;

                let Some(existing_secret) = existing_secret else {
                    return Ok::<_, BoxErr>(None);
                };

                let events_json = serde_json::to_string(&webhook.events)?;
                let modified_fields_json = webhook
                    .modified_fields
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?;
                let secret_enc = webhook.secret_enc.unwrap_or(existing_secret);

                conn.execute(
                    "UPDATE webhooks
                     SET url = ?1,
                         events_json = ?2,
                         modified_fields_json = ?3,
                         name = ?4,
                         enabled = ?5,
                         consecutive_failures = CASE WHEN ?5 = 1 THEN 0 ELSE consecutive_failures END,
                         secret_enc = ?6,
                         updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                     WHERE user_id = ?7 AND id = ?8",
                    rusqlite::params![
                        webhook.url,
                        events_json,
                        modified_fields_json,
                        webhook.name,
                        if webhook.enabled { 1 } else { 0 },
                        secret_enc,
                        webhook.user_id,
                        webhook.id,
                    ],
                )?;

                let row = conn
                    .query_row(
                        "SELECT id, user_id, url, events_json, modified_fields_json, name,
                                enabled, consecutive_failures, secret_enc, created_at, updated_at
                         FROM webhooks
                         WHERE user_id = ?1 AND id = ?2",
                        rusqlite::params![webhook.user_id, webhook.id],
                        webhook_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn delete_webhook_impl(
        &self,
        user_id: &str,
        webhook_id: &str,
    ) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let webhook_id = webhook_id.to_string();
        let rows = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "DELETE FROM webhooks WHERE user_id = ?1 AND id = ?2",
                    rusqlite::params![user_id, webhook_id],
                )?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(rows > 0)
    }

    pub(super) async fn list_admin_webhooks_impl(&self) -> anyhow::Result<Vec<AdminWebhookRecord>> {
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, url, events_json, modified_fields_json, name,
                            enabled, consecutive_failures, secret_enc, created_at, updated_at
                     FROM admin_webhooks
                     ORDER BY created_at, id",
                )?;
                let rows = stmt
                    .query_map([], admin_webhook_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn get_admin_webhook_impl(
        &self,
        webhook_id: &str,
    ) -> anyhow::Result<Option<AdminWebhookRecord>> {
        let webhook_id = webhook_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT id, url, events_json, modified_fields_json, name,
                                enabled, consecutive_failures, secret_enc, created_at, updated_at
                         FROM admin_webhooks
                         WHERE id = ?1",
                        [&webhook_id],
                        admin_webhook_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn create_admin_webhook_impl(
        &self,
        webhook: &NewAdminWebhookRecord,
    ) -> anyhow::Result<AdminWebhookRecord> {
        let webhook = webhook.clone();
        let result = self
            .conn
            .call(move |conn| {
                let events_json = serde_json::to_string(&webhook.events)?;
                let modified_fields_json = webhook
                    .modified_fields
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?;

                conn.execute(
                    "INSERT INTO admin_webhooks (
                        id, url, events_json, modified_fields_json, name,
                        enabled, consecutive_failures, secret_enc
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
                    rusqlite::params![
                        webhook.id,
                        webhook.url,
                        events_json,
                        modified_fields_json,
                        webhook.name,
                        if webhook.enabled { 1 } else { 0 },
                        webhook.secret_enc,
                    ],
                )?;

                let row = conn.query_row(
                    "SELECT id, url, events_json, modified_fields_json, name,
                            enabled, consecutive_failures, secret_enc, created_at, updated_at
                     FROM admin_webhooks
                     WHERE id = ?1",
                    [&webhook.id],
                    admin_webhook_from_row,
                )?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn update_admin_webhook_impl(
        &self,
        webhook: &UpdateAdminWebhookRecord,
    ) -> anyhow::Result<Option<AdminWebhookRecord>> {
        let webhook = webhook.clone();
        let result = self
            .conn
            .call(move |conn| {
                let updated = conn.execute(
                    "UPDATE admin_webhooks
                     SET enabled = ?1,
                         consecutive_failures = CASE WHEN ?1 = 1 THEN 0 ELSE consecutive_failures END,
                         updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                     WHERE id = ?2",
                    rusqlite::params![if webhook.enabled { 1 } else { 0 }, webhook.id],
                )?;
                if updated == 0 {
                    return Ok::<_, BoxErr>(None);
                }

                let row = conn
                    .query_row(
                        "SELECT id, url, events_json, modified_fields_json, name,
                                enabled, consecutive_failures, secret_enc, created_at, updated_at
                         FROM admin_webhooks
                         WHERE id = ?1",
                        [&webhook.id],
                        admin_webhook_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn delete_admin_webhook_impl(&self, webhook_id: &str) -> anyhow::Result<bool> {
        let webhook_id = webhook_id.to_string();
        let rows = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "DELETE FROM admin_webhooks WHERE id = ?1",
                    rusqlite::params![webhook_id],
                )?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(rows > 0)
    }

    pub(super) async fn list_webhook_deliveries_impl(
        &self,
        user_id: &str,
        webhook_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<WebhookDeliveryRecord>> {
        let user_id = user_id.to_string();
        let webhook_id = webhook_id.to_string();
        let limit = limit as i64;
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT d.delivery_id, d.webhook_id, d.event_id, d.event, d.timestamp,
                            d.status, d.response_status, d.attempt, d.failure_reason
                     FROM webhook_deliveries d
                     JOIN webhooks w ON w.id = d.webhook_id
                     WHERE w.user_id = ?1 AND w.id = ?2
                     ORDER BY d.timestamp DESC, d.delivery_id DESC
                     LIMIT ?3",
                )?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![user_id, webhook_id, limit],
                        webhook_delivery_from_row,
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn list_admin_webhook_deliveries_impl(
        &self,
        webhook_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<WebhookDeliveryRecord>> {
        let webhook_id = webhook_id.to_string();
        let limit = limit as i64;
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT delivery_id, webhook_id, event_id, event, timestamp,
                            status, response_status, attempt, failure_reason
                     FROM admin_webhook_deliveries
                     WHERE webhook_id = ?1
                     ORDER BY timestamp DESC, delivery_id DESC
                     LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![webhook_id, limit],
                        webhook_delivery_from_row,
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn record_webhook_delivery_impl(
        &self,
        delivery: &WebhookDeliveryRecord,
    ) -> anyhow::Result<()> {
        let delivery = delivery.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO webhook_deliveries (
                        delivery_id, webhook_id, event_id, event, timestamp,
                        status, response_status, attempt, failure_reason
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        delivery.delivery_id,
                        delivery.webhook_id,
                        delivery.event_id,
                        delivery.event,
                        delivery.timestamp,
                        delivery.status,
                        delivery.response_status.map(i64::from),
                        i64::from(delivery.attempt),
                        delivery.failure_reason,
                    ],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn record_admin_webhook_delivery_impl(
        &self,
        delivery: &WebhookDeliveryRecord,
    ) -> anyhow::Result<()> {
        let delivery = delivery.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO admin_webhook_deliveries (
                        delivery_id, webhook_id, event_id, event, timestamp,
                        status, response_status, attempt, failure_reason
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        delivery.delivery_id,
                        delivery.webhook_id,
                        delivery.event_id,
                        delivery.event,
                        delivery.timestamp,
                        delivery.status,
                        delivery.response_status.map(i64::from),
                        i64::from(delivery.attempt),
                        delivery.failure_reason,
                    ],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn purge_webhook_deliveries_older_than_impl(
        &self,
        retention_days: u32,
    ) -> anyhow::Result<usize> {
        let cutoff = format!("-{retention_days} days");
        let deleted = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "DELETE FROM webhook_deliveries
                     WHERE datetime(timestamp) < datetime('now', ?1)",
                    [cutoff],
                )?;
                let admin_rows = conn.execute(
                    "DELETE FROM admin_webhook_deliveries
                     WHERE datetime(timestamp) < datetime('now', ?1)",
                    [format!("-{retention_days} days")],
                )?;
                Ok::<_, BoxErr>(rows + admin_rows)
            })
            .await
            .map_err(map_err)?;
        Ok(deleted)
    }

    pub(super) async fn mark_webhook_delivery_succeeded_impl(
        &self,
        webhook_id: &str,
    ) -> anyhow::Result<()> {
        let webhook_id = webhook_id.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE webhooks
                     SET consecutive_failures = 0,
                         updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                     WHERE id = ?1",
                    [&webhook_id],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn mark_admin_webhook_delivery_succeeded_impl(
        &self,
        webhook_id: &str,
    ) -> anyhow::Result<()> {
        let webhook_id = webhook_id.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE admin_webhooks
                     SET consecutive_failures = 0,
                         updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                     WHERE id = ?1",
                    [&webhook_id],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn mark_webhook_delivery_failed_impl(
        &self,
        webhook_id: &str,
        disable_after: u32,
    ) -> anyhow::Result<Option<WebhookFailureState>> {
        let webhook_id = webhook_id.to_string();
        let disable_after = i64::from(disable_after);
        let result = self
            .conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                let updated = tx.execute(
                    "UPDATE webhooks
                     SET consecutive_failures = consecutive_failures + 1,
                         enabled = CASE
                             WHEN consecutive_failures + 1 >= ?2 THEN 0
                             ELSE enabled
                         END,
                         updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                     WHERE id = ?1",
                    rusqlite::params![webhook_id, disable_after],
                )?;
                if updated == 0 {
                    tx.commit()?;
                    return Ok::<_, BoxErr>(None);
                }

                let state = tx.query_row(
                    "SELECT consecutive_failures, enabled
                     FROM webhooks
                     WHERE id = ?1",
                    [&webhook_id],
                    |row| {
                        Ok(WebhookFailureState {
                            consecutive_failures: row.get::<_, i64>(0)? as u32,
                            enabled: row.get::<_, i64>(1)? != 0,
                        })
                    },
                )?;
                tx.commit()?;
                Ok::<_, BoxErr>(Some(state))
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn mark_admin_webhook_delivery_failed_impl(
        &self,
        webhook_id: &str,
        disable_after: u32,
    ) -> anyhow::Result<Option<WebhookFailureState>> {
        let webhook_id = webhook_id.to_string();
        let disable_after = i64::from(disable_after);
        let result = self
            .conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                let updated = tx.execute(
                    "UPDATE admin_webhooks
                     SET consecutive_failures = consecutive_failures + 1,
                         enabled = CASE
                             WHEN consecutive_failures + 1 >= ?2 THEN 0
                             ELSE enabled
                         END,
                         updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                     WHERE id = ?1",
                    rusqlite::params![webhook_id, disable_after],
                )?;
                if updated == 0 {
                    tx.commit()?;
                    return Ok::<_, BoxErr>(None);
                }

                let state = tx.query_row(
                    "SELECT consecutive_failures, enabled
                     FROM admin_webhooks
                     WHERE id = ?1",
                    [&webhook_id],
                    |row| {
                        Ok(WebhookFailureState {
                            consecutive_failures: row.get::<_, i64>(0)? as u32,
                            enabled: row.get::<_, i64>(1)? != 0,
                        })
                    },
                )?;
                tx.commit()?;
                Ok::<_, BoxErr>(Some(state))
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn record_webhook_event_history_impl(
        &self,
        user_id: &str,
        task_uuid: &str,
        event_type: &str,
        due_at: &str,
    ) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let task_uuid = task_uuid.to_string();
        let event_type = event_type.to_string();
        let due_at = due_at.to_string();
        let inserted = self
            .conn
            .call(move |conn| {
                let rows = conn.execute(
                    "INSERT OR IGNORE INTO webhook_event_history (
                        user_id, task_uuid, event_type, due_at
                     ) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![user_id, task_uuid, event_type, due_at],
                )?;
                Ok::<_, BoxErr>(rows > 0)
            })
            .await
            .map_err(map_err)?;
        Ok(inserted)
    }

    pub(super) async fn clear_webhook_event_history_impl(
        &self,
        user_id: &str,
        task_uuid: &str,
    ) -> anyhow::Result<()> {
        let user_id = user_id.to_string();
        let task_uuid = task_uuid.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM webhook_event_history
                     WHERE user_id = ?1 AND task_uuid = ?2",
                    rusqlite::params![user_id, task_uuid],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }
}
