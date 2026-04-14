use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio_rusqlite::Connection;

use super::models::*;
use super::ConfigStore;

#[path = "sqlite/auth.rs"]
mod auth;
#[path = "sqlite/config.rs"]
mod config;
#[path = "sqlite/devices.rs"]
mod devices;
#[path = "sqlite/maintenance.rs"]
mod maintenance;
#[path = "sqlite/runtime.rs"]
mod runtime;
#[path = "sqlite/webhooks.rs"]
mod webhooks;

/// Hash a bearer token for storage/lookup (tokens are never stored in plaintext)
pub(super) fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub(super) fn delete_user_owned_rows(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    user_id: &str,
) -> Result<(), rusqlite::Error> {
    let result = match table {
        "devices" => tx.execute("DELETE FROM devices WHERE user_id = ?1", [&user_id]),
        "api_tokens" => tx.execute("DELETE FROM api_tokens WHERE user_id = ?1", [&user_id]),
        "user_runtime_policies" => tx.execute(
            "DELETE FROM user_runtime_policies WHERE user_id = ?1",
            [&user_id],
        ),
        "views" => tx.execute("DELETE FROM views WHERE user_id = ?1", [&user_id]),
        "contexts" => tx.execute("DELETE FROM contexts WHERE user_id = ?1", [&user_id]),
        "presets" => tx.execute("DELETE FROM presets WHERE user_id = ?1", [&user_id]),
        "stores" => tx.execute("DELETE FROM stores WHERE user_id = ?1", [&user_id]),
        "replicas" => tx.execute("DELETE FROM replicas WHERE user_id = ?1", [&user_id]),
        "sync_clients" => tx.execute("DELETE FROM sync_clients WHERE user_id = ?1", [&user_id]),
        "shopping_config" => {
            tx.execute("DELETE FROM shopping_config WHERE user_id = ?1", [&user_id])
        }
        "config" => tx.execute("DELETE FROM config WHERE user_id = ?1", [&user_id]),
        "webhooks" => tx.execute("DELETE FROM webhooks WHERE user_id = ?1", [&user_id]),
        _ => unreachable!("delete_user_owned_rows only accepts internal allowlisted tables"),
    };

    match result {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(_, Some(ref msg))) if msg.contains("no such table") => {
            Ok(())
        }
        Err(err) => Err(err),
    }
}

pub(super) type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// Convert tokio_rusqlite::Error<E> to anyhow::Error
pub(super) fn map_err<E: std::fmt::Display + Send + Sync + 'static>(
    e: tokio_rusqlite::Error<E>,
) -> anyhow::Error {
    match e {
        tokio_rusqlite::Error::ConnectionClosed => anyhow::anyhow!("Connection closed"),
        tokio_rusqlite::Error::Close((_, e)) => anyhow::anyhow!("Close error: {e}"),
        tokio_rusqlite::Error::Error(e) => anyhow::anyhow!("{e}"),
        _ => anyhow::anyhow!("Unknown tokio-rusqlite error"),
    }
}

/// SQLite-backed implementation of ConfigStore.
///
/// Uses tokio-rusqlite for async access. Designed to be swappable
/// with a Postgres implementation when scaling commercially.
#[derive(Clone)]
pub struct SqliteConfigStore {
    conn: Connection,
}

impl SqliteConfigStore {
    pub async fn new(db_path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(db_path).await?;

        conn.call(|conn| {
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
            Ok::<_, BoxErr>(())
        })
        .await
        .map_err(map_err)?;

        Ok(Self { conn })
    }
}

#[cfg(test)]
impl SqliteConfigStore {
    /// Create all tables inline (for unit tests that don't have the migrations directory).
    async fn run_migrations_inline(&self) -> anyhow::Result<()> {
        self.conn
            .call(|conn| {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS users (
                        id TEXT PRIMARY KEY,
                        username TEXT UNIQUE NOT NULL,
                        password_hash TEXT NOT NULL,
                        created_at TEXT NOT NULL DEFAULT (datetime('now'))
                    );
                    CREATE TABLE IF NOT EXISTS api_tokens (
                        token_hash TEXT PRIMARY KEY,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        label TEXT,
                        token_id TEXT UNIQUE,
                        expires_at TEXT,
                        created_at TEXT NOT NULL DEFAULT (datetime('now')),
                        first_used_at TEXT,
                        last_used_at TEXT,
                        last_used_ip TEXT
                    );
                    CREATE TABLE IF NOT EXISTS user_runtime_policies (
                        user_id TEXT PRIMARY KEY REFERENCES users(id),
                        desired_version TEXT NOT NULL,
                        desired_policy_json TEXT NOT NULL,
                        applied_version TEXT,
                        applied_policy_json TEXT,
                        applied_at TEXT,
                        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                    );
                    CREATE TABLE IF NOT EXISTS views (
                        id TEXT NOT NULL,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        label TEXT NOT NULL,
                        icon TEXT NOT NULL DEFAULT '',
                        filter TEXT NOT NULL DEFAULT '',
                        group_by TEXT,
                        context_filtered INTEGER NOT NULL DEFAULT 0,
                        display_mode TEXT NOT NULL DEFAULT 'list',
                        sort_order INTEGER NOT NULL DEFAULT 0,
                        origin TEXT NOT NULL DEFAULT 'user' CHECK(origin IN ('builtin', 'user')),
                        user_modified INTEGER NOT NULL DEFAULT 0,
                        hidden INTEGER NOT NULL DEFAULT 0,
                        template_version INTEGER NOT NULL DEFAULT 0,
                        PRIMARY KEY (user_id, id)
                    );
                    CREATE TABLE IF NOT EXISTS contexts (
                        id TEXT NOT NULL,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        label TEXT NOT NULL,
                        project_prefixes TEXT NOT NULL DEFAULT '[]',
                        color TEXT,
                        icon TEXT,
                        sort_order INTEGER NOT NULL DEFAULT 0,
                        PRIMARY KEY (user_id, id)
                    );
                    CREATE TABLE IF NOT EXISTS presets (
                        id TEXT NOT NULL,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        label TEXT NOT NULL,
                        raw_suffix TEXT NOT NULL DEFAULT '',
                        sort_order INTEGER NOT NULL DEFAULT 0,
                        PRIMARY KEY (user_id, id)
                    );
                    CREATE TABLE IF NOT EXISTS stores (
                        id TEXT NOT NULL,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        label TEXT NOT NULL,
                        tag TEXT NOT NULL DEFAULT '',
                        sort_order INTEGER NOT NULL DEFAULT 0,
                        PRIMARY KEY (user_id, id)
                    );
                    CREATE TABLE IF NOT EXISTS shopping_config (
                        user_id TEXT PRIMARY KEY REFERENCES users(id),
                        project TEXT NOT NULL DEFAULT '',
                        default_tags TEXT NOT NULL DEFAULT '[]'
                    );
                    CREATE TABLE IF NOT EXISTS geofences (
                        id TEXT NOT NULL,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        label TEXT NOT NULL,
                        latitude REAL NOT NULL,
                        longitude REAL NOT NULL,
                        radius REAL NOT NULL DEFAULT 200,
                        type TEXT NOT NULL DEFAULT 'home',
                        context_id TEXT,
                        view_id TEXT,
                        store_tag TEXT,
                        PRIMARY KEY (user_id, id)
                    );
                    CREATE TABLE IF NOT EXISTS config (
                        config_type TEXT NOT NULL,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        version TEXT,
                        items TEXT NOT NULL DEFAULT '[]',
                        PRIMARY KEY (user_id, config_type)
                    );
                    CREATE TABLE IF NOT EXISTS replicas (
                        id TEXT PRIMARY KEY,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        encryption_secret_enc TEXT NOT NULL,
                        label TEXT NOT NULL DEFAULT 'Personal',
                        created_at TEXT NOT NULL DEFAULT (datetime('now')),
                        UNIQUE(user_id)
                    );
                    CREATE TABLE IF NOT EXISTS devices (
                        client_id TEXT PRIMARY KEY,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        name TEXT NOT NULL,
                        encryption_secret_enc TEXT,
                        registered_at TEXT NOT NULL DEFAULT (datetime('now')),
                        last_sync_at TEXT,
                        last_sync_ip TEXT,
                        status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'revoked')),
                        bootstrap_request_id TEXT UNIQUE,
                        bootstrap_status TEXT,
                        bootstrap_requested_username TEXT,
                        bootstrap_create_user_if_missing INTEGER,
                        bootstrap_expires_at TEXT
                    );
                    CREATE TABLE IF NOT EXISTS webhooks (
                        id TEXT PRIMARY KEY,
                        user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                        url TEXT NOT NULL,
                        events_json TEXT NOT NULL,
                        modified_fields_json TEXT,
                        name TEXT,
                        enabled INTEGER NOT NULL DEFAULT 1,
                        consecutive_failures INTEGER NOT NULL DEFAULT 0,
                        secret_enc TEXT NOT NULL,
                        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                        updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                        UNIQUE(user_id, url)
                    );
                    CREATE INDEX IF NOT EXISTS idx_webhooks_user_id
                    ON webhooks(user_id);
                    CREATE TABLE IF NOT EXISTS webhook_deliveries (
                        delivery_id TEXT PRIMARY KEY,
                        webhook_id TEXT NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
                        event_id TEXT NOT NULL,
                        event TEXT NOT NULL,
                        timestamp TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                        status TEXT NOT NULL,
                        response_status INTEGER,
                        attempt INTEGER NOT NULL,
                        failure_reason TEXT
                    );
                    CREATE TABLE IF NOT EXISTS admin_webhooks (
                        id TEXT PRIMARY KEY,
                        url TEXT NOT NULL UNIQUE,
                        events_json TEXT NOT NULL,
                        modified_fields_json TEXT,
                        name TEXT,
                        enabled INTEGER NOT NULL DEFAULT 1,
                        consecutive_failures INTEGER NOT NULL DEFAULT 0,
                        secret_enc TEXT NOT NULL,
                        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                        updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
                    );
                    CREATE TABLE IF NOT EXISTS admin_webhook_deliveries (
                        delivery_id TEXT PRIMARY KEY,
                        webhook_id TEXT NOT NULL REFERENCES admin_webhooks(id) ON DELETE CASCADE,
                        event_id TEXT NOT NULL,
                        event TEXT NOT NULL,
                        timestamp TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                        status TEXT NOT NULL,
                        response_status INTEGER,
                        attempt INTEGER NOT NULL,
                        failure_reason TEXT
                    );
                    CREATE TABLE IF NOT EXISTS webhook_event_history (
                        user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                        task_uuid TEXT NOT NULL,
                        event_type TEXT NOT NULL,
                        due_at TEXT NOT NULL,
                        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                        PRIMARY KEY (user_id, task_uuid, event_type, due_at)
                    );
                    CREATE INDEX IF NOT EXISTS idx_webhook_event_history_task
                    ON webhook_event_history(user_id, task_uuid);
                    CREATE INDEX IF NOT EXISTS idx_webhook_event_history_created_at
                    ON webhook_event_history(created_at);",
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

#[async_trait]
impl ConfigStore for SqliteConfigStore {
    // --- Users & Auth ---

    async fn get_user_by_token(&self, token: &str) -> anyhow::Result<Option<UserRecord>> {
        self.get_user_by_token_impl(token).await
    }

    async fn get_user_by_id(&self, user_id: &str) -> anyhow::Result<Option<UserRecord>> {
        self.get_user_by_id_impl(user_id).await
    }

    async fn get_user_by_username(&self, username: &str) -> anyhow::Result<Option<UserRecord>> {
        self.get_user_by_username_impl(username).await
    }

    async fn list_users(&self) -> anyhow::Result<Vec<UserRecord>> {
        self.list_users_impl().await
    }

    async fn create_user(&self, user: &NewUser) -> anyhow::Result<UserRecord> {
        self.create_user_impl(user).await
    }

    async fn create_api_token(&self, user_id: &str, label: Option<&str>) -> anyhow::Result<String> {
        self.create_api_token_impl(user_id, label).await
    }

    async fn create_api_token_with_expiry(
        &self,
        user_id: &str,
        label: Option<&str>,
        expires_at: Option<&str>,
        token_bytes: usize,
    ) -> anyhow::Result<String> {
        self.create_api_token_with_expiry_impl(user_id, label, expires_at, token_bytes)
            .await
    }

    async fn create_connect_config_token(
        &self,
        user_id: &str,
        expires_at: &str,
        token_bytes: usize,
    ) -> anyhow::Result<ConnectConfigIssuedToken> {
        self.create_connect_config_token_impl(user_id, expires_at, token_bytes)
            .await
    }

    async fn lookup_connect_config_token(
        &self,
        token: &str,
    ) -> anyhow::Result<Option<ConnectConfigTokenCorrelation>> {
        self.lookup_connect_config_token_impl(token).await
    }

    async fn delete_user(&self, user_id: &str) -> anyhow::Result<bool> {
        self.delete_user_impl(user_id).await
    }

    async fn get_runtime_policy(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Option<RuntimePolicyRecord>> {
        self.get_runtime_policy_impl(user_id).await
    }

    async fn upsert_runtime_policy(
        &self,
        user_id: &str,
        desired_version: &str,
        desired_policy: &crate::runtime_policy::RuntimePolicy,
        applied_version: Option<&str>,
        applied_policy: Option<&crate::runtime_policy::RuntimePolicy>,
        applied_at: Option<&str>,
    ) -> anyhow::Result<RuntimePolicyRecord> {
        self.upsert_runtime_policy_impl(
            user_id,
            desired_version,
            desired_policy,
            applied_version,
            applied_policy,
            applied_at,
        )
        .await
    }

    async fn list_api_tokens(&self, user_id: &str) -> anyhow::Result<Vec<ApiTokenRecord>> {
        self.list_api_tokens_impl(user_id).await
    }

    async fn record_connect_config_token_use(
        &self,
        token: &str,
        client_ip: &str,
    ) -> anyhow::Result<ConnectConfigTokenUse> {
        self.record_connect_config_token_use_impl(token, client_ip)
            .await
    }

    async fn revoke_api_token(&self, token_hash: &str) -> anyhow::Result<bool> {
        self.revoke_api_token_impl(token_hash).await
    }

    // --- Views ---

    async fn list_views(&self, user_id: &str) -> anyhow::Result<Vec<ViewRecord>> {
        self.list_views_impl(user_id).await
    }

    async fn list_views_all(&self, user_id: &str) -> anyhow::Result<Vec<ViewRecord>> {
        self.list_views_all_impl(user_id).await
    }

    async fn upsert_view(&self, user_id: &str, view: &ViewRecord) -> anyhow::Result<()> {
        self.upsert_view_impl(user_id, view).await
    }

    async fn delete_view(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        self.delete_view_impl(user_id, id).await
    }

    // --- Contexts ---

    async fn list_contexts(&self, user_id: &str) -> anyhow::Result<Vec<ContextRecord>> {
        self.list_contexts_impl(user_id).await
    }

    async fn upsert_context(&self, user_id: &str, ctx: &ContextRecord) -> anyhow::Result<()> {
        self.upsert_context_impl(user_id, ctx).await
    }

    async fn delete_context(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        self.delete_context_impl(user_id, id).await
    }

    // --- Presets ---

    async fn list_presets(&self, user_id: &str) -> anyhow::Result<Vec<PresetRecord>> {
        self.list_presets_impl(user_id).await
    }

    async fn upsert_preset(&self, user_id: &str, preset: &PresetRecord) -> anyhow::Result<()> {
        self.upsert_preset_impl(user_id, preset).await
    }

    async fn delete_preset(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        self.delete_preset_impl(user_id, id).await
    }

    // --- Stores ---

    async fn list_stores(&self, user_id: &str) -> anyhow::Result<Vec<StoreRecord>> {
        self.list_stores_impl(user_id).await
    }

    async fn upsert_store(&self, user_id: &str, store: &StoreRecord) -> anyhow::Result<()> {
        self.upsert_store_impl(user_id, store).await
    }

    async fn delete_store(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        self.delete_store_impl(user_id, id).await
    }

    // --- Shopping Config ---

    async fn get_shopping_config(&self, user_id: &str) -> anyhow::Result<Option<ShoppingRecord>> {
        self.get_shopping_config_impl(user_id).await
    }

    async fn upsert_shopping_config(
        &self,
        user_id: &str,
        config: &ShoppingRecord,
    ) -> anyhow::Result<()> {
        self.upsert_shopping_config_impl(user_id, config).await
    }

    async fn delete_shopping_config(&self, user_id: &str) -> anyhow::Result<bool> {
        self.delete_shopping_config_impl(user_id).await
    }

    // --- Geofences ---

    async fn list_geofences(&self, user_id: &str) -> anyhow::Result<Vec<GeofenceRecord>> {
        self.list_geofences_impl(user_id).await
    }

    async fn upsert_geofence(
        &self,
        user_id: &str,
        geofence: &GeofenceRecord,
    ) -> anyhow::Result<()> {
        self.upsert_geofence_impl(user_id, geofence).await
    }

    async fn delete_geofence(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        self.delete_geofence_impl(user_id, id).await
    }

    // --- Generic Config ---

    async fn get_config(
        &self,
        user_id: &str,
        config_type: &str,
    ) -> anyhow::Result<Option<GenericConfigRecord>> {
        self.get_config_impl(user_id, config_type).await
    }

    async fn upsert_config(
        &self,
        user_id: &str,
        config_type: &str,
        record: &GenericConfigRecord,
    ) -> anyhow::Result<()> {
        self.upsert_config_impl(user_id, config_type, record).await
    }

    async fn delete_config_item(
        &self,
        user_id: &str,
        config_type: &str,
        item_id: &str,
    ) -> anyhow::Result<bool> {
        self.delete_config_item_impl(user_id, config_type, item_id)
            .await
    }

    // --- Replicas (ADR-0001) ---

    async fn create_replica(
        &self,
        user_id: &str,
        client_id: &str,
        encryption_secret_enc: &str,
    ) -> anyhow::Result<()> {
        self.create_replica_impl(user_id, client_id, encryption_secret_enc)
            .await
    }

    async fn get_replica_by_user(&self, user_id: &str) -> anyhow::Result<Option<ReplicaRecord>> {
        self.get_replica_by_user_impl(user_id).await
    }

    async fn get_replica_by_client_id(
        &self,
        client_id: &str,
    ) -> anyhow::Result<Option<ReplicaRecord>> {
        self.get_replica_by_client_id_impl(client_id).await
    }

    async fn get_user_by_client_id(&self, client_id: &str) -> anyhow::Result<Option<UserRecord>> {
        self.get_user_by_client_id_impl(client_id).await
    }

    async fn delete_replica(&self, user_id: &str) -> anyhow::Result<bool> {
        self.delete_replica_impl(user_id).await
    }

    // --- Devices ---

    async fn list_devices(&self, user_id: &str) -> anyhow::Result<Vec<DeviceRecord>> {
        self.list_devices_impl(user_id).await
    }

    async fn get_device(&self, client_id: &str) -> anyhow::Result<Option<DeviceRecord>> {
        self.get_device_impl(client_id).await
    }

    async fn get_device_by_bootstrap_request(
        &self,
        bootstrap_request_id: &str,
    ) -> anyhow::Result<Option<DeviceRecord>> {
        self.get_device_by_bootstrap_request_impl(bootstrap_request_id)
            .await
    }

    async fn create_device(
        &self,
        user_id: &str,
        client_id: &str,
        name: &str,
        encryption_secret_enc: Option<&str>,
    ) -> anyhow::Result<()> {
        self.create_device_impl(user_id, client_id, name, encryption_secret_enc)
            .await
    }

    async fn create_bootstrap_device(
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
        self.create_bootstrap_device_impl(
            user_id,
            client_id,
            name,
            encryption_secret_enc,
            bootstrap_request_id,
            bootstrap_requested_username,
            bootstrap_create_user_if_missing,
            bootstrap_expires_at,
        )
        .await
    }

    async fn update_device_name(
        &self,
        user_id: &str,
        client_id: &str,
        name: &str,
    ) -> anyhow::Result<bool> {
        self.update_device_name_impl(user_id, client_id, name).await
    }

    async fn revoke_device(&self, user_id: &str, client_id: &str) -> anyhow::Result<bool> {
        self.revoke_device_impl(user_id, client_id).await
    }

    async fn unrevoke_device(&self, user_id: &str, client_id: &str) -> anyhow::Result<bool> {
        self.unrevoke_device_impl(user_id, client_id).await
    }

    async fn delete_device(&self, user_id: &str, client_id: &str) -> anyhow::Result<bool> {
        self.delete_device_impl(user_id, client_id).await
    }

    async fn acknowledge_bootstrap_device(
        &self,
        bootstrap_request_id: &str,
    ) -> anyhow::Result<bool> {
        self.acknowledge_bootstrap_device_impl(bootstrap_request_id)
            .await
    }

    async fn touch_device(&self, client_id: &str, ip: &str) -> anyhow::Result<()> {
        self.touch_device_impl(client_id, ip).await
    }

    // --- Webhooks ---

    async fn list_webhooks(&self, user_id: &str) -> anyhow::Result<Vec<WebhookRecord>> {
        self.list_webhooks_impl(user_id).await
    }

    async fn get_webhook(
        &self,
        user_id: &str,
        webhook_id: &str,
    ) -> anyhow::Result<Option<WebhookRecord>> {
        self.get_webhook_impl(user_id, webhook_id).await
    }

    async fn create_webhook(&self, webhook: &NewWebhookRecord) -> anyhow::Result<WebhookRecord> {
        self.create_webhook_impl(webhook).await
    }

    async fn update_webhook(
        &self,
        webhook: &UpdateWebhookRecord,
    ) -> anyhow::Result<Option<WebhookRecord>> {
        self.update_webhook_impl(webhook).await
    }

    async fn delete_webhook(&self, user_id: &str, webhook_id: &str) -> anyhow::Result<bool> {
        self.delete_webhook_impl(user_id, webhook_id).await
    }

    async fn list_admin_webhooks(&self) -> anyhow::Result<Vec<AdminWebhookRecord>> {
        self.list_admin_webhooks_impl().await
    }

    async fn get_admin_webhook(
        &self,
        webhook_id: &str,
    ) -> anyhow::Result<Option<AdminWebhookRecord>> {
        self.get_admin_webhook_impl(webhook_id).await
    }

    async fn create_admin_webhook(
        &self,
        webhook: &NewAdminWebhookRecord,
    ) -> anyhow::Result<AdminWebhookRecord> {
        self.create_admin_webhook_impl(webhook).await
    }

    async fn update_admin_webhook(
        &self,
        webhook: &UpdateAdminWebhookRecord,
    ) -> anyhow::Result<Option<AdminWebhookRecord>> {
        self.update_admin_webhook_impl(webhook).await
    }

    async fn delete_admin_webhook(&self, webhook_id: &str) -> anyhow::Result<bool> {
        self.delete_admin_webhook_impl(webhook_id).await
    }

    async fn list_webhook_deliveries(
        &self,
        user_id: &str,
        webhook_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<WebhookDeliveryRecord>> {
        self.list_webhook_deliveries_impl(user_id, webhook_id, limit)
            .await
    }

    async fn list_admin_webhook_deliveries(
        &self,
        webhook_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<WebhookDeliveryRecord>> {
        self.list_admin_webhook_deliveries_impl(webhook_id, limit)
            .await
    }

    async fn record_webhook_delivery(
        &self,
        delivery: &WebhookDeliveryRecord,
    ) -> anyhow::Result<()> {
        self.record_webhook_delivery_impl(delivery).await
    }

    async fn record_admin_webhook_delivery(
        &self,
        delivery: &WebhookDeliveryRecord,
    ) -> anyhow::Result<()> {
        self.record_admin_webhook_delivery_impl(delivery).await
    }

    async fn purge_webhook_deliveries_older_than(
        &self,
        retention_days: u32,
    ) -> anyhow::Result<usize> {
        self.purge_webhook_deliveries_older_than_impl(retention_days)
            .await
    }

    async fn mark_webhook_delivery_succeeded(&self, webhook_id: &str) -> anyhow::Result<()> {
        self.mark_webhook_delivery_succeeded_impl(webhook_id).await
    }

    async fn mark_admin_webhook_delivery_succeeded(&self, webhook_id: &str) -> anyhow::Result<()> {
        self.mark_admin_webhook_delivery_succeeded_impl(webhook_id)
            .await
    }

    async fn mark_webhook_delivery_failed(
        &self,
        webhook_id: &str,
        disable_after: u32,
    ) -> anyhow::Result<Option<WebhookFailureState>> {
        self.mark_webhook_delivery_failed_impl(webhook_id, disable_after)
            .await
    }

    async fn mark_admin_webhook_delivery_failed(
        &self,
        webhook_id: &str,
        disable_after: u32,
    ) -> anyhow::Result<Option<WebhookFailureState>> {
        self.mark_admin_webhook_delivery_failed_impl(webhook_id, disable_after)
            .await
    }

    async fn record_webhook_event_history(
        &self,
        user_id: &str,
        task_uuid: &str,
        event_type: &str,
        due_at: &str,
    ) -> anyhow::Result<bool> {
        self.record_webhook_event_history_impl(user_id, task_uuid, event_type, due_at)
            .await
    }

    async fn clear_webhook_event_history(
        &self,
        user_id: &str,
        task_uuid: &str,
    ) -> anyhow::Result<()> {
        self.clear_webhook_event_history_impl(user_id, task_uuid)
            .await
    }

    // --- Migrations ---

    async fn checkpoint_database(&self) -> anyhow::Result<()> {
        self.checkpoint_database_impl().await
    }

    async fn backup_to_path(&self, dst: &Path) -> anyhow::Result<()> {
        self.backup_to_path_impl(dst).await
    }

    async fn restore_from_path(&self, src: &Path) -> anyhow::Result<()> {
        self.restore_from_path_impl(src).await
    }

    async fn run_migrations(&self) -> anyhow::Result<()> {
        self.run_migrations_impl().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// Create an in-memory SqliteConfigStore with all tables.
    async fn test_store() -> SqliteConfigStore {
        let store = SqliteConfigStore::new(":memory:").await.unwrap();
        store.run_migrations_inline().await.unwrap();
        store
    }

    /// Create a test user and return (user_id, raw_token).
    async fn create_test_user(store: &SqliteConfigStore) -> (String, String) {
        let user = store
            .create_user(&NewUser {
                username: format!("test_{}", Uuid::new_v4()),
                password_hash: "hash".to_string(),
            })
            .await
            .unwrap();
        let token = store
            .create_api_token(&user.id, Some("test"))
            .await
            .unwrap();
        (user.id, token)
    }

    #[tokio::test]
    async fn test_expired_token_not_returned() {
        let store = test_store().await;
        let user = store
            .create_user(&NewUser {
                username: "expiry_test".to_string(),
                password_hash: "hash".to_string(),
            })
            .await
            .unwrap();

        // Insert a token with past expiry directly via SQL
        let token = "expired-token-value";
        let token_hash = hash_token(token);
        let user_id = user.id.clone();
        store
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO api_tokens (token_hash, user_id, label, expires_at)
                     VALUES (?1, ?2, 'expired', datetime('now', '-1 hour'))",
                    rusqlite::params![token_hash, user_id],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .unwrap();

        // Lookup should return None because the token is expired
        let result = store.get_user_by_token(token).await.unwrap();
        assert!(result.is_none(), "expired token should not return a user");
    }

    #[tokio::test]
    async fn test_revoke_token() {
        let store = test_store().await;
        let (user_id, token) = create_test_user(&store).await;

        // Token should work before revocation
        let before = store.get_user_by_token(&token).await.unwrap();
        assert!(before.is_some(), "token should be valid before revocation");
        assert_eq!(before.unwrap().id, user_id);

        // Revoke by hash
        let token_hash = hash_token(&token);
        let revoked = store.revoke_api_token(&token_hash).await.unwrap();
        assert!(revoked, "revoke should return true");

        // Token should no longer work
        let after = store.get_user_by_token(&token).await.unwrap();
        assert!(after.is_none(), "revoked token should not return a user");
    }

    #[tokio::test]
    async fn test_create_api_token_with_expiry_supports_compact_tokens() {
        let store = test_store().await;
        let user = store
            .create_user(&NewUser {
                username: "compact-token-test".to_string(),
                password_hash: "hash".to_string(),
            })
            .await
            .unwrap();

        let issued = store
            .create_connect_config_token(&user.id, "2099-01-01 00:00:00", 18)
            .await
            .unwrap();
        let token = issued.token.clone();

        assert!(token.len() <= 24, "compact token should fit QR budget");
        assert!(
            !token.contains('='),
            "compact token should be URL-safe without padding"
        );

        let looked_up = store.get_user_by_token(&token).await.unwrap();
        assert_eq!(looked_up.unwrap().id, user.id);

        let first = store
            .record_connect_config_token_use(&token, "203.0.113.10")
            .await
            .unwrap();
        match first {
            ConnectConfigTokenUse::FirstUse(correlation) => {
                assert_eq!(correlation.user_id, user.id);
                assert_eq!(correlation.token_id, issued.token_id);
                assert_eq!(
                    correlation.credential_hash_prefix,
                    issued.credential_hash_prefix
                );
            }
            other => panic!("expected FirstUse, got {other:?}"),
        }

        let second = store
            .record_connect_config_token_use(&token, "203.0.113.11")
            .await
            .unwrap();
        match second {
            ConnectConfigTokenUse::RepeatUse(correlation) => {
                assert_eq!(correlation.token_id, issued.token_id);
            }
            other => panic!("expected RepeatUse, got {other:?}"),
        }

        let token_row = store.list_api_tokens(&user.id).await.unwrap();
        let token_row = token_row
            .into_iter()
            .find(|row| row.label.as_deref() == Some("connect-config"))
            .unwrap();
        assert_eq!(
            token_row.token_id.as_deref(),
            Some(issued.token_id.as_str())
        );
        assert!(token_row.first_used_at.is_some());
        assert!(token_row.last_used_at.is_some());
        assert_eq!(token_row.last_used_ip.as_deref(), Some("203.0.113.11"));
    }

    #[tokio::test]
    async fn test_delete_user_removes_tokens() {
        let store = test_store().await;
        let (user_id, token) = create_test_user(&store).await;

        // Token works before deletion
        assert!(store.get_user_by_token(&token).await.unwrap().is_some());

        // Delete user
        let deleted = store.delete_user(&user_id).await.unwrap();
        assert!(deleted, "delete_user should return true");

        // Token lookup should fail (user and tokens deleted)
        let after = store.get_user_by_token(&token).await.unwrap();
        assert!(after.is_none(), "token should be gone after user deletion");

        // User should be gone too
        let user = store.get_user_by_id(&user_id).await.unwrap();
        assert!(user.is_none(), "user should be gone after deletion");
    }

    #[tokio::test]
    async fn test_create_and_query_replicas() {
        let store = test_store().await;
        let (user_id, _token) = create_test_user(&store).await;

        let client_id = Uuid::new_v4().to_string();
        let enc_secret = "encrypted-secret-base64";

        // Create replica
        store
            .create_replica(&user_id, &client_id, enc_secret)
            .await
            .unwrap();

        // get_replica_by_user should return it
        let replica = store.get_replica_by_user(&user_id).await.unwrap();
        assert!(replica.is_some());
        let replica = replica.unwrap();
        assert_eq!(replica.id, client_id);
        assert_eq!(replica.user_id, user_id);
        assert_eq!(replica.encryption_secret_enc, enc_secret);
        assert_eq!(replica.label, "Personal");

        // get_replica_by_client_id should return it
        let replica = store.get_replica_by_client_id(&client_id).await.unwrap();
        assert!(replica.is_some());
        assert_eq!(replica.unwrap().user_id, user_id);

        // get_user_by_client_id should resolve to the user
        let user = store.get_user_by_client_id(&client_id).await.unwrap();
        assert!(user.is_some());
        assert_eq!(user.unwrap().id, user_id);

        // Delete replica
        let deleted = store.delete_replica(&user_id).await.unwrap();
        assert!(deleted);

        // Should be gone now
        let replica = store.get_replica_by_user(&user_id).await.unwrap();
        assert!(
            replica.is_none(),
            "replica should be removed after deletion"
        );

        // Lookup by client_id should return None
        let user = store.get_user_by_client_id(&client_id).await.unwrap();
        assert!(user.is_none());
    }

    #[tokio::test]
    async fn test_one_replica_per_user() {
        let store = test_store().await;
        let (user_id, _token) = create_test_user(&store).await;

        let client_id1 = Uuid::new_v4().to_string();
        let client_id2 = Uuid::new_v4().to_string();

        store
            .create_replica(&user_id, &client_id1, "enc1")
            .await
            .unwrap();

        // Second create for same user should fail (UNIQUE constraint on user_id)
        let result = store.create_replica(&user_id, &client_id2, "enc2").await;
        assert!(result.is_err(), "second replica for same user should fail");
    }

    #[tokio::test]
    async fn test_revoke_nonexistent_token_returns_false() {
        let store = test_store().await;
        let result = store.revoke_api_token("nonexistent-hash").await.unwrap();
        assert!(!result, "revoking nonexistent token should return false");
    }

    #[tokio::test]
    async fn test_delete_nonexistent_user_returns_false() {
        let store = test_store().await;
        let result = store.delete_user("nonexistent-id").await.unwrap();
        assert!(!result, "deleting nonexistent user should return false");
    }

    #[tokio::test]
    async fn test_delete_user_cascades_tokens_and_replicas() {
        let store = test_store().await;
        let (user_id, token) = create_test_user(&store).await;

        // Create a replica for this user
        let client_id = Uuid::new_v4().to_string();
        store
            .create_replica(&user_id, &client_id, "enc-secret")
            .await
            .unwrap();

        // Verify token and replica exist
        assert!(
            store.get_user_by_token(&token).await.unwrap().is_some(),
            "token should exist before deletion"
        );
        assert!(
            store
                .get_user_by_client_id(&client_id)
                .await
                .unwrap()
                .is_some(),
            "replica should exist before deletion"
        );
        assert!(store.get_replica_by_user(&user_id).await.unwrap().is_some());

        // Delete user — should cascade to tokens and replicas
        let deleted = store.delete_user(&user_id).await.unwrap();
        assert!(deleted, "delete_user should return true");

        // Verify everything is gone
        assert!(
            store.get_user_by_token(&token).await.unwrap().is_none(),
            "token should be gone after user deletion"
        );
        assert!(
            store
                .get_user_by_client_id(&client_id)
                .await
                .unwrap()
                .is_none(),
            "replica should be gone after user deletion"
        );
        assert!(
            store.list_api_tokens(&user_id).await.unwrap().is_empty(),
            "api_tokens should be empty after user deletion"
        );
        assert!(
            store.get_replica_by_user(&user_id).await.unwrap().is_none(),
            "replicas should be empty after user deletion"
        );
        assert!(
            store.get_user_by_id(&user_id).await.unwrap().is_none(),
            "user should be gone after deletion"
        );
    }

    /// DR scenario: delete_user on a restored backup from an older schema version
    /// where replicas table doesn't exist yet. The "no such table" branch in
    /// delete_user must handle this gracefully without crashing.
    #[tokio::test]
    async fn test_delete_user_missing_replicas_table() {
        // Create a store with only users + api_tokens (simulating old schema)
        let store = SqliteConfigStore::new(":memory:").await.unwrap();
        store
            .conn
            .call(|conn| {
                conn.execute_batch(
                    "CREATE TABLE users (
                    id TEXT PRIMARY KEY,
                    username TEXT UNIQUE NOT NULL,
                    password_hash TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE api_tokens (
                    token_hash TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL REFERENCES users(id),
                    label TEXT,
                    expires_at TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            })
            .await
            .unwrap();

        // Create a user directly
        store.conn.call(|conn| {
            conn.execute(
                "INSERT INTO users (id, username, password_hash) VALUES ('dr-user', 'drtest', 'hash')",
                [],
            )?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }).await.unwrap();

        // delete_user should NOT crash even though replicas table is missing
        let result = store.delete_user("dr-user").await;
        assert!(
            result.is_ok(),
            "delete_user should handle missing replicas table: {:?}",
            result.err()
        );
        assert!(result.unwrap(), "user should have been deleted");

        // Verify user is actually gone
        let user = store
            .conn
            .call(|conn| {
                let exists: bool = conn.query_row(
                    "SELECT COUNT(*) > 0 FROM users WHERE id = 'dr-user'",
                    [],
                    |row| row.get(0),
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(exists)
            })
            .await
            .unwrap();
        assert!(!user, "user should be removed from DB");
    }

    /// DR scenario: delete_user on a backup missing views/contexts/presets/stores tables.
    /// Simulates a very old backup where only users + api_tokens exist.
    #[tokio::test]
    async fn test_delete_user_missing_all_config_tables() {
        let store = SqliteConfigStore::new(":memory:").await.unwrap();
        store
            .conn
            .call(|conn| {
                conn.execute_batch(
                    "CREATE TABLE users (
                    id TEXT PRIMARY KEY,
                    username TEXT UNIQUE NOT NULL,
                    password_hash TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE api_tokens (
                    token_hash TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL REFERENCES users(id),
                    label TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            })
            .await
            .unwrap();

        store.conn.call(|conn| {
            conn.execute(
                "INSERT INTO users (id, username, password_hash) VALUES ('old-user', 'oldtest', 'hash')",
                [],
            )?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }).await.unwrap();

        // Should not crash even though views, contexts, presets, stores, replicas, sync_clients are all missing
        let result = store.delete_user("old-user").await;
        assert!(
            result.is_ok(),
            "delete_user should handle missing config tables: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_device_revoke_unrevoke_delete_lifecycle() {
        let store = test_store().await;
        let (user_id, _token) = create_test_user(&store).await;
        let client_id = Uuid::new_v4().to_string();

        store
            .create_device(
                &user_id,
                &client_id,
                "Test device",
                Some("enc-device-secret"),
            )
            .await
            .unwrap();

        let device = store.get_device(&client_id).await.unwrap().unwrap();
        assert_eq!(device.status, "active");
        assert_eq!(device.name, "Test device");

        let revoked = store.revoke_device(&user_id, &client_id).await.unwrap();
        assert!(revoked, "revoke_device should return true");
        let device = store.get_device(&client_id).await.unwrap().unwrap();
        assert_eq!(device.status, "revoked");

        let unrevoked = store.unrevoke_device(&user_id, &client_id).await.unwrap();
        assert!(unrevoked, "unrevoke_device should return true");
        let device = store.get_device(&client_id).await.unwrap().unwrap();
        assert_eq!(device.status, "active");

        let deleted = store.delete_device(&user_id, &client_id).await.unwrap();
        assert!(deleted, "delete_device should return true");
        assert!(
            store.get_device(&client_id).await.unwrap().is_none(),
            "device should be gone after delete"
        );
    }
}
