pub mod models;
pub mod sqlite;

use std::path::Path;

use async_trait::async_trait;
use models::*;

/// Trait abstracting config database access.
///
/// Handlers depend on this trait, not a specific database implementation.
/// Currently backed by SQLite (`SqliteConfigStore`), designed to be
/// swappable to Postgres when scaling for commercial use.
#[async_trait]
pub trait ConfigStore: Send + Sync + 'static {
    // --- Users & Auth ---
    async fn get_user_by_token(&self, token: &str) -> anyhow::Result<Option<UserRecord>>;
    async fn get_user_by_id(&self, user_id: &str) -> anyhow::Result<Option<UserRecord>>;
    async fn get_user_by_username(&self, username: &str) -> anyhow::Result<Option<UserRecord>>;
    async fn list_users(&self) -> anyhow::Result<Vec<UserRecord>>;
    async fn create_user(&self, user: &NewUser) -> anyhow::Result<UserRecord>;
    async fn delete_user(&self, user_id: &str) -> anyhow::Result<bool>;
    async fn get_runtime_policy(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Option<RuntimePolicyRecord>>;
    async fn upsert_runtime_policy(
        &self,
        user_id: &str,
        desired_version: &str,
        desired_policy: &crate::runtime_policy::RuntimePolicy,
        applied_version: Option<&str>,
        applied_policy: Option<&crate::runtime_policy::RuntimePolicy>,
        applied_at: Option<&str>,
    ) -> anyhow::Result<RuntimePolicyRecord>;
    async fn create_api_token(&self, user_id: &str, label: Option<&str>) -> anyhow::Result<String>;
    async fn create_api_token_with_expiry(
        &self,
        user_id: &str,
        label: Option<&str>,
        expires_at: Option<&str>,
        token_bytes: usize,
    ) -> anyhow::Result<String>;
    async fn create_connect_config_token(
        &self,
        user_id: &str,
        expires_at: &str,
        token_bytes: usize,
    ) -> anyhow::Result<ConnectConfigIssuedToken>;
    async fn lookup_connect_config_token(
        &self,
        token: &str,
    ) -> anyhow::Result<Option<ConnectConfigTokenCorrelation>>;
    async fn record_connect_config_token_use(
        &self,
        token: &str,
        client_ip: &str,
    ) -> anyhow::Result<ConnectConfigTokenUse>;
    async fn list_api_tokens(&self, user_id: &str) -> anyhow::Result<Vec<ApiTokenRecord>>;
    async fn revoke_api_token(&self, token_hash: &str) -> anyhow::Result<bool>;

    // --- Views ---
    /// List visible views for a user (excludes hidden/tombstoned views).
    async fn list_views(&self, user_id: &str) -> anyhow::Result<Vec<ViewRecord>>;
    /// List ALL views including hidden tombstones (for reconciliation).
    async fn list_views_all(&self, user_id: &str) -> anyhow::Result<Vec<ViewRecord>>;
    async fn upsert_view(&self, user_id: &str, view: &ViewRecord) -> anyhow::Result<()>;
    async fn delete_view(&self, user_id: &str, id: &str) -> anyhow::Result<bool>;

    // --- Contexts ---
    async fn list_contexts(&self, user_id: &str) -> anyhow::Result<Vec<ContextRecord>>;
    async fn upsert_context(&self, user_id: &str, ctx: &ContextRecord) -> anyhow::Result<()>;
    async fn delete_context(&self, user_id: &str, id: &str) -> anyhow::Result<bool>;

    // --- Presets ---
    async fn list_presets(&self, user_id: &str) -> anyhow::Result<Vec<PresetRecord>>;
    async fn upsert_preset(&self, user_id: &str, preset: &PresetRecord) -> anyhow::Result<()>;
    async fn delete_preset(&self, user_id: &str, id: &str) -> anyhow::Result<bool>;

    // --- Stores ---
    async fn list_stores(&self, user_id: &str) -> anyhow::Result<Vec<StoreRecord>>;
    async fn upsert_store(&self, user_id: &str, store: &StoreRecord) -> anyhow::Result<()>;
    async fn delete_store(&self, user_id: &str, id: &str) -> anyhow::Result<bool>;

    // --- Shopping Config ---
    async fn get_shopping_config(&self, user_id: &str) -> anyhow::Result<Option<ShoppingRecord>>;
    async fn upsert_shopping_config(
        &self,
        user_id: &str,
        config: &ShoppingRecord,
    ) -> anyhow::Result<()>;
    async fn delete_shopping_config(&self, user_id: &str) -> anyhow::Result<bool>;

    // --- Geofences ---
    async fn list_geofences(&self, user_id: &str) -> anyhow::Result<Vec<GeofenceRecord>>;
    async fn upsert_geofence(&self, user_id: &str, geofence: &GeofenceRecord)
        -> anyhow::Result<()>;
    async fn delete_geofence(&self, user_id: &str, id: &str) -> anyhow::Result<bool>;

    // --- Generic Config (backwards compat) ---
    async fn get_config(
        &self,
        user_id: &str,
        config_type: &str,
    ) -> anyhow::Result<Option<GenericConfigRecord>>;
    async fn upsert_config(
        &self,
        user_id: &str,
        config_type: &str,
        record: &GenericConfigRecord,
    ) -> anyhow::Result<()>;
    async fn delete_config_item(
        &self,
        user_id: &str,
        config_type: &str,
        item_id: &str,
    ) -> anyhow::Result<bool>;

    // --- Replicas (ADR-0001: per-user sync identity + key escrow) ---
    async fn create_replica(
        &self,
        user_id: &str,
        client_id: &str,
        encryption_secret_enc: &str,
    ) -> anyhow::Result<()>;
    async fn get_replica_by_user(&self, user_id: &str) -> anyhow::Result<Option<ReplicaRecord>>;
    async fn get_replica_by_client_id(
        &self,
        client_id: &str,
    ) -> anyhow::Result<Option<ReplicaRecord>>;
    /// Look up a user by their replica's client_id (used by TC sync auth).
    async fn get_user_by_client_id(&self, client_id: &str) -> anyhow::Result<Option<UserRecord>>;
    async fn delete_replica(&self, user_id: &str) -> anyhow::Result<bool>;

    // --- Devices (per-user client_id registry) ---
    async fn list_devices(&self, user_id: &str) -> anyhow::Result<Vec<DeviceRecord>>;
    async fn get_device(&self, client_id: &str) -> anyhow::Result<Option<DeviceRecord>>;
    async fn get_device_by_bootstrap_request(
        &self,
        bootstrap_request_id: &str,
    ) -> anyhow::Result<Option<DeviceRecord>>;
    async fn create_device(
        &self,
        user_id: &str,
        client_id: &str,
        name: &str,
        encryption_secret_enc: Option<&str>,
    ) -> anyhow::Result<()>;
    // TODO: collapse bootstrap params into a struct once the onboarding flow stabilises.
    #[allow(clippy::too_many_arguments)]
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
    ) -> anyhow::Result<()>;
    async fn update_device_name(
        &self,
        user_id: &str,
        client_id: &str,
        name: &str,
    ) -> anyhow::Result<bool>;
    async fn revoke_device(&self, user_id: &str, client_id: &str) -> anyhow::Result<bool>;
    async fn unrevoke_device(&self, user_id: &str, client_id: &str) -> anyhow::Result<bool>;
    async fn delete_device(&self, user_id: &str, client_id: &str) -> anyhow::Result<bool>;
    async fn acknowledge_bootstrap_device(
        &self,
        bootstrap_request_id: &str,
    ) -> anyhow::Result<bool>;
    /// Update last_sync_at and last_sync_ip for a device (called on every successful sync).
    async fn touch_device(&self, client_id: &str, ip: &str) -> anyhow::Result<()>;

    // --- Webhooks ---
    async fn list_webhooks(&self, user_id: &str) -> anyhow::Result<Vec<WebhookRecord>>;
    async fn get_webhook(
        &self,
        user_id: &str,
        webhook_id: &str,
    ) -> anyhow::Result<Option<WebhookRecord>>;
    async fn create_webhook(&self, webhook: &NewWebhookRecord) -> anyhow::Result<WebhookRecord>;
    async fn update_webhook(
        &self,
        webhook: &UpdateWebhookRecord,
    ) -> anyhow::Result<Option<WebhookRecord>>;
    async fn delete_webhook(&self, user_id: &str, webhook_id: &str) -> anyhow::Result<bool>;
    async fn list_admin_webhooks(&self) -> anyhow::Result<Vec<AdminWebhookRecord>>;
    async fn get_admin_webhook(
        &self,
        webhook_id: &str,
    ) -> anyhow::Result<Option<AdminWebhookRecord>>;
    async fn create_admin_webhook(
        &self,
        webhook: &NewAdminWebhookRecord,
    ) -> anyhow::Result<AdminWebhookRecord>;
    async fn update_admin_webhook(
        &self,
        webhook: &UpdateAdminWebhookRecord,
    ) -> anyhow::Result<Option<AdminWebhookRecord>>;
    async fn delete_admin_webhook(&self, webhook_id: &str) -> anyhow::Result<bool>;
    async fn list_webhook_deliveries(
        &self,
        user_id: &str,
        webhook_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<WebhookDeliveryRecord>>;
    async fn list_admin_webhook_deliveries(
        &self,
        webhook_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<WebhookDeliveryRecord>>;
    async fn record_webhook_delivery(&self, delivery: &WebhookDeliveryRecord)
        -> anyhow::Result<()>;
    async fn record_admin_webhook_delivery(
        &self,
        delivery: &WebhookDeliveryRecord,
    ) -> anyhow::Result<()>;
    async fn purge_webhook_deliveries_older_than(
        &self,
        retention_days: u32,
    ) -> anyhow::Result<usize>;
    async fn mark_webhook_delivery_succeeded(&self, webhook_id: &str) -> anyhow::Result<()>;
    async fn mark_admin_webhook_delivery_succeeded(&self, webhook_id: &str) -> anyhow::Result<()>;
    async fn mark_webhook_delivery_failed(
        &self,
        webhook_id: &str,
        disable_after: u32,
    ) -> anyhow::Result<Option<WebhookFailureState>>;
    async fn mark_admin_webhook_delivery_failed(
        &self,
        webhook_id: &str,
        disable_after: u32,
    ) -> anyhow::Result<Option<WebhookFailureState>>;
    async fn record_webhook_event_history(
        &self,
        user_id: &str,
        task_uuid: &str,
        event_type: &str,
        due_at: &str,
    ) -> anyhow::Result<bool>;
    async fn clear_webhook_event_history(
        &self,
        user_id: &str,
        task_uuid: &str,
    ) -> anyhow::Result<()>;

    // --- Migrations ---
    async fn checkpoint_database(&self) -> anyhow::Result<()>;
    async fn backup_to_path(&self, dst: &Path) -> anyhow::Result<()>;
    async fn restore_from_path(&self, src: &Path) -> anyhow::Result<()>;
    async fn run_migrations(&self) -> anyhow::Result<()>;
}
