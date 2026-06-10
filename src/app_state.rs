use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use crate::auth::middleware::AuthCache;
use crate::circuit_breaker::CircuitBreaker;
use crate::config::ServerConfig;
use crate::health::handlers::HealthCache;
use crate::merged_sync_gateway::storage::MergedSyncStorageManager;
use crate::replica::ReplicaManager;
use crate::runtime_recovery::RuntimeRecoveryCoordinator;
use crate::runtime_sync::RuntimeSyncCoordinator;
use crate::store::{ConfigStore, OperatorMaintenanceBackend};
use crate::tc_sync::runtime::SyncStorageManager;
use crate::webhooks::delivery::{ReqwestWebhookTransport, WebhookTransport};
use crate::webhooks::security::{SystemWebhookDnsResolver, WebhookDnsResolver};

#[derive(Clone)]
pub struct AppState {
    /// Config store (trait object — currently SQLite, swappable to Postgres)
    pub store: Arc<dyn ConfigStore>,
    /// Backend-specific operator maintenance handle (checkpoint / backup /
    /// restore). Distinct from `store`: lives outside `ConfigStore` because
    /// these ops cannot be honestly implemented by every backend (e.g.
    /// Postgres). See `src/store/maintenance.rs`.
    pub maintenance: Arc<dyn OperatorMaintenanceBackend>,
    /// Base data directory
    pub data_dir: PathBuf,
    /// Server configuration (Arc-wrapped to avoid cloning on every request)
    pub config: Arc<ServerConfig>,
    /// Operator token can change after a restore that includes secrets.
    pub operator_token: Arc<std::sync::RwLock<Option<String>>>,
    /// Cached health stats (pending task count, refreshed every 30s)
    pub health_cache: HealthCache,
    /// Per-user replica connection manager (caches open connections)
    pub replica_manager: ReplicaManager,
    /// Auth token cache (reduces config DB load)
    pub auth_cache: AuthCache,
    /// LLM circuit breaker (auto-fallback when API is degraded)
    pub llm_circuit_breaker: Arc<CircuitBreaker>,
    /// Server start time (for uptime reporting)
    pub started_at: Instant,
    /// Per-device sync storage connection pool (DashMap with TTL eviction)
    pub sync_storage_manager: Arc<SyncStorageManager>,
    /// Merged gateway sync storage connection pool, separate from the old per-user sync chain.
    pub merged_sync_storage_manager: Arc<MergedSyncStorageManager>,
    /// Runtime recovery/offline coordination.
    pub recovery_runtime: Arc<RuntimeRecoveryCoordinator>,
    /// Runtime sync coordination: gateway freshness tracking.
    pub runtime_sync: Arc<RuntimeSyncCoordinator>,
    /// Serialises backup and restore operations.
    pub admin_operation_lock: Arc<Mutex<()>>,
    /// Outbound webhook transport.
    pub webhook_transport: Arc<dyn WebhookTransport>,
    /// DNS resolver used for webhook SSRF validation and address pinning.
    pub webhook_dns_resolver: Arc<dyn WebhookDnsResolver>,
    /// Delay schedule between failed webhook delivery attempts.
    pub webhook_retry_delays: Arc<[std::time::Duration]>,
    /// Tracks webhook dispatches spawned off the synchronous mutation response
    /// path (#149) — drives the `webhook_dispatch_in_flight` gauge and the
    /// test quiescence signal.
    pub webhook_dispatch: Arc<crate::webhooks::dispatch::WebhookDispatchTracker>,
}

impl AppState {
    pub fn new(
        store: Arc<dyn ConfigStore>,
        maintenance: Arc<dyn OperatorMaintenanceBackend>,
        config: &ServerConfig,
    ) -> Self {
        Self::with_webhook_transport(
            store,
            maintenance,
            config,
            Arc::new(ReqwestWebhookTransport),
        )
    }

    pub fn with_webhook_transport(
        store: Arc<dyn ConfigStore>,
        maintenance: Arc<dyn OperatorMaintenanceBackend>,
        config: &ServerConfig,
        webhook_transport: Arc<dyn WebhookTransport>,
    ) -> Self {
        Self::with_webhook_transport_and_retry_delays(
            store,
            maintenance,
            config,
            webhook_transport,
            Arc::new(SystemWebhookDnsResolver),
            vec![
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(10),
                std::time::Duration::from_secs(60),
            ],
        )
    }

    pub fn with_webhook_transport_and_retry_delays(
        store: Arc<dyn ConfigStore>,
        maintenance: Arc<dyn OperatorMaintenanceBackend>,
        config: &ServerConfig,
        webhook_transport: Arc<dyn WebhookTransport>,
        webhook_dns_resolver: Arc<dyn WebhookDnsResolver>,
        webhook_retry_delays: Vec<std::time::Duration>,
    ) -> Self {
        crate::metrics::configure_disk_metrics_paths(
            &config.server.data_dir,
            config.backup_dir.as_deref(),
        );
        let replica_manager = ReplicaManager::new(&config.server.data_dir);
        let sync_storage_manager = Arc::new(SyncStorageManager::new(&config.server.data_dir));
        let merged_sync_storage_manager =
            Arc::new(MergedSyncStorageManager::new(&config.server.data_dir));
        let runtime_sync = Arc::new(RuntimeSyncCoordinator::new());
        let recovery_runtime = Arc::new(RuntimeRecoveryCoordinator::new(
            &config.server.data_dir,
            replica_manager.clone(),
            sync_storage_manager.clone(),
            merged_sync_storage_manager.clone(),
            runtime_sync.freshness_tracker(),
        ));
        let state = Self {
            store,
            maintenance,
            data_dir: config.server.data_dir.clone(),
            config: Arc::new(config.clone()),
            operator_token: Arc::new(std::sync::RwLock::new(config.admin.http_token.clone())),
            health_cache: HealthCache::new(),
            replica_manager,
            auth_cache: AuthCache::new(),
            llm_circuit_breaker: Arc::new(CircuitBreaker::new()),
            started_at: Instant::now(),
            sync_storage_manager,
            merged_sync_storage_manager,
            recovery_runtime,
            runtime_sync,
            admin_operation_lock: Arc::new(Mutex::new(())),
            webhook_transport,
            webhook_dns_resolver,
            webhook_retry_delays: webhook_retry_delays.into(),
            webhook_dispatch: Arc::new(crate::webhooks::dispatch::WebhookDispatchTracker::new(
                crate::webhooks::dispatch::capacity_from_env(),
            )),
        };
        state.recovery_runtime.start();
        crate::webhooks::scheduler::start(&state);
        crate::idempotency::start(&state);
        state
    }

    /// Get the TaskChampion replica directory for a user.
    pub fn user_replica_dir(&self, user_id: &str) -> PathBuf {
        self.data_dir.join("users").join(user_id)
    }

    /// Marker file used to keep a user offline across restarts and to let the
    /// local admin CLI coordinate with a running server.
    pub fn user_offline_marker(&self, user_id: &str) -> PathBuf {
        self.recovery_runtime.user_offline_marker(user_id)
    }

    /// Return true when a user is currently offline/quarantined.
    pub fn is_user_quarantined(&self, user_id: &str) -> bool {
        self.recovery_runtime.is_user_quarantined(user_id)
    }

    /// Persistently mark a user offline and evict any open runtime state.
    pub fn mark_user_offline(&self, user_id: &str) {
        self.recovery_runtime.mark_user_offline(user_id);
    }

    /// Bring a user back online and evict stale runtime state so it reopens
    /// from disk on the next request.
    pub fn clear_user_quarantine(&self, user_id: &str) -> bool {
        self.recovery_runtime.clear_user_quarantine(user_id)
    }

    /// Synchronously refresh the in-memory offline/quarantine set from the
    /// persisted marker files on disk.
    pub fn sync_offline_markers_now(&self) {
        self.recovery_runtime.sync_offline_markers_now();
    }

    /// Quarantine a user and evict all cached connections.
    /// Called on corruption detection — ensures no stale connections remain.
    pub fn quarantine_user(&self, user_id: &str) {
        self.recovery_runtime.quarantine_user(user_id);
    }

    pub fn operator_token(&self) -> Option<String> {
        self.operator_token
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    pub fn set_operator_token(&self, token: Option<String>) {
        if let Ok(mut guard) = self.operator_token.write() {
            *guard = token;
        }
    }
}
