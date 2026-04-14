//! Sync bridge: wires the canonical REST replica to per-device TC sync chains.
//!
//! Called by the background bridge scheduler after REST mutations/reads and by
//! TC handlers to reconcile the requesting device chain with the canonical
//! plaintext replica.
//!
//! If `master_key` is not configured, sync is silently skipped (server doesn't
//! support the bridge without key escrow).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use dashmap::DashMap;
use taskchampion::{Replica, SqliteStorage};
use tokio::sync::Mutex;

use crate::app_state::AppState;
use crate::app_state::SyncInFlight;
use crate::config::ServerConfig;
use crate::metrics as m;
use crate::replica;
use crate::runtime_recovery::RuntimeRecoveryCoordinator;
use crate::runtime_sync::RuntimeSyncCoordinator;
use crate::store::models::DeviceRecord;
use crate::store::ConfigStore;
use crate::tc_sync::bridge::SyncBridgeServer;
use crate::tc_sync::runtime::SyncStorageManager;
use crate::tc_sync::storage::SyncStorage;

/// Timeout for each sync operation. If sync takes longer than this,
/// we log a warning and return an error (eventual consistency — caller
/// continues without sync).
///
/// Override with `CMDOCK_SYNC_TIMEOUT` env var (value in seconds, default 5).
fn sync_timeout() -> Duration {
    static TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
        let secs = std::env::var("CMDOCK_SYNC_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(5)
            .max(1); // Floor at 1s to prevent hot-loop on 0
        Duration::from_secs(secs)
    });
    *TIMEOUT
}

/// Debounce window for queued bridge sync jobs.
fn scheduler_debounce() -> Duration {
    static DEBOUNCE: LazyLock<Duration> = LazyLock::new(|| {
        let millis = std::env::var("CMDOCK_SYNC_DEBOUNCE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(250);
        Duration::from_millis(millis.max(1))
    });
    *DEBOUNCE
}

#[derive(Clone, Copy)]
pub enum SyncPriority {
    Low = 1,
    Normal = 2,
    High = 3,
}

#[derive(Clone)]
pub struct BridgeFreshnessTracker {
    users: Arc<DashMap<String, Arc<UserFreshnessState>>>,
}

struct UserFreshnessState {
    canonical_generation: AtomicU64,
    device_generations: DashMap<String, u64>,
}

impl UserFreshnessState {
    fn new() -> Self {
        Self {
            // Start at 1 so newly registered devices are treated as stale
            // until they complete an initial bridge sync from canonical.
            canonical_generation: AtomicU64::new(1),
            device_generations: DashMap::new(),
        }
    }
}

impl BridgeFreshnessTracker {
    pub fn new() -> Self {
        Self {
            users: Arc::new(DashMap::new()),
        }
    }

    fn user_state(&self, user_id: &str) -> Arc<UserFreshnessState> {
        self.users
            .entry(user_id.to_string())
            .or_insert_with(|| Arc::new(UserFreshnessState::new()))
            .clone()
    }

    pub fn mark_canonical_changed(&self, user_id: &str) -> u64 {
        self.user_state(user_id)
            .canonical_generation
            .fetch_add(1, Ordering::AcqRel)
            + 1
    }

    pub fn mark_device_synced_to_current(&self, user_id: &str, client_id: &str) -> u64 {
        let state = self.user_state(user_id);
        let generation = state.canonical_generation.load(Ordering::Acquire);
        state
            .device_generations
            .insert(client_id.to_string(), generation);
        generation
    }

    pub fn mark_devices_synced_to_current<'a, I>(&self, user_id: &str, client_ids: I) -> u64
    where
        I: IntoIterator<Item = &'a str>,
    {
        let state = self.user_state(user_id);
        let generation = state.canonical_generation.load(Ordering::Acquire);
        for client_id in client_ids {
            state
                .device_generations
                .insert(client_id.to_string(), generation);
        }
        generation
    }

    pub fn mark_canonical_changed_and_device_synced(&self, user_id: &str, client_id: &str) -> u64 {
        let state = self.user_state(user_id);
        let generation = state.canonical_generation.fetch_add(1, Ordering::AcqRel) + 1;
        state
            .device_generations
            .insert(client_id.to_string(), generation);
        generation
    }

    pub fn device_needs_sync(&self, user_id: &str, client_id: &str) -> bool {
        let state = self.user_state(user_id);
        let current = state.canonical_generation.load(Ordering::Acquire);
        let device_generation = state
            .device_generations
            .get(client_id)
            .map(|entry| *entry.value())
            .unwrap_or(0);
        device_generation < current
    }

    pub fn remove_device(&self, user_id: &str, client_id: &str) {
        if let Some(state) = self.users.get(user_id) {
            state.device_generations.remove(client_id);
        }
    }

    pub fn clear_user(&self, user_id: &str) {
        self.users.remove(user_id);
    }
}

impl Default for BridgeFreshnessTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncPriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            3 => Self::High,
            2 => Self::Normal,
            _ => Self::Low,
        }
    }
}

#[derive(Clone)]
pub struct BridgeScheduler {
    inner: Arc<BridgeSchedulerInner>,
}

struct BridgeSchedulerInner {
    context: std::sync::OnceLock<BridgeSyncContext>,
    lanes: DashMap<String, Arc<UserSyncLane>>,
}

struct UserSyncLane {
    pending_priority: AtomicU8,
    running: AtomicBool,
}

impl UserSyncLane {
    fn new() -> Self {
        Self {
            pending_priority: AtomicU8::new(0),
            running: AtomicBool::new(false),
        }
    }
}

impl BridgeScheduler {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(BridgeSchedulerInner {
                context: std::sync::OnceLock::new(),
                lanes: DashMap::new(),
            }),
        }
    }

    pub fn start(&self, state: &AppState) {
        let _ = self.inner.context.set(BridgeSyncContext::from_state(state));
    }

    pub fn schedule(&self, user_id: &str, priority: SyncPriority, source: &'static str) {
        let Some(context) = self.inner.context.get() else {
            return;
        };
        if context.config.master_key.is_none() {
            return;
        }

        let lane = self
            .inner
            .lanes
            .entry(user_id.to_string())
            .or_insert_with(|| Arc::new(UserSyncLane::new()))
            .clone();

        let previous = bump_pending_priority(&lane.pending_priority, priority as u8);
        m::record_bridge_sync_enqueue(source, priority.as_str());
        if previous >= priority as u8 {
            m::record_bridge_sync_coalesced(source, priority.as_str());
        }

        if !lane.running.swap(true, Ordering::AcqRel) {
            let context = context.clone();
            let lanes = self.inner.lanes.clone();
            let user_id = user_id.to_string();
            tokio::spawn(async move {
                run_scheduled_sync(context, user_id, lane, lanes).await;
            });
        }
    }
}

impl Default for BridgeScheduler {
    fn default() -> Self {
        Self::new()
    }
}

fn bump_pending_priority(target: &AtomicU8, next: u8) -> u8 {
    let mut current = target.load(Ordering::Acquire);
    loop {
        let desired = current.max(next);
        match target.compare_exchange(current, desired, Ordering::AcqRel, Ordering::Acquire) {
            Ok(previous) => return previous,
            Err(observed) => current = observed,
        }
    }
}

#[derive(Clone)]
struct BridgeSyncContext {
    store: Arc<dyn ConfigStore>,
    data_dir: PathBuf,
    config: Arc<ServerConfig>,
    replica_manager: crate::replica::ReplicaManager,
    sync_storage_manager: Arc<SyncStorageManager>,
    recovery_runtime: Arc<RuntimeRecoveryCoordinator>,
    sync_in_flight: Arc<SyncInFlight>,
    runtime_sync: Arc<RuntimeSyncCoordinator>,
}

impl BridgeSyncContext {
    fn from_state(state: &AppState) -> Self {
        Self {
            store: state.store.clone(),
            data_dir: state.data_dir.clone(),
            config: state.config.clone(),
            replica_manager: state.replica_manager.clone(),
            sync_storage_manager: state.sync_storage_manager.clone(),
            recovery_runtime: state.recovery_runtime.clone(),
            sync_in_flight: state.sync_in_flight.clone(),
            runtime_sync: state.runtime_sync.clone(),
        }
    }

    fn user_replica_dir(&self, user_id: &str) -> PathBuf {
        self.data_dir.join("users").join(user_id)
    }

    fn quarantine_user(&self, user_id: &str) {
        self.recovery_runtime.quarantine_user(user_id);
    }
}

// Cryptor caching moved to tc_sync::cryptor_cache (shared with TC handlers).

async fn run_scheduled_sync(
    context: BridgeSyncContext,
    user_id: String,
    lane: Arc<UserSyncLane>,
    lanes: DashMap<String, Arc<UserSyncLane>>,
) {
    loop {
        tokio::time::sleep(scheduler_debounce()).await;

        let pending = lane.pending_priority.swap(0, Ordering::AcqRel);
        if pending == 0 {
            lane.running.store(false, Ordering::Release);
            if lane.pending_priority.load(Ordering::Acquire) == 0 {
                lanes.remove(&user_id);
                return;
            }
            if lane.running.swap(true, Ordering::AcqRel) {
                return;
            }
            continue;
        }

        let priority = SyncPriority::from_u8(pending);
        let start = std::time::Instant::now();
        let result = sync_user_replica_inner(&context, &user_id).await;
        match &result {
            Ok(_) => m::record_bridge_sync_run(
                "scheduled",
                priority.as_str(),
                "ok",
                start.elapsed().as_secs_f64(),
            ),
            Err(_) => m::record_bridge_sync_run(
                "scheduled",
                priority.as_str(),
                "error",
                start.elapsed().as_secs_f64(),
            ),
        }
        if let Err(e) = result {
            tracing::warn!("Scheduled sync bridge run failed for user {user_id}: {e}");
        }
    }
}

/// Sync the user's canonical Replica with the shared user sync chain.
///
/// Called after REST mutations (push local changes) and before reads
/// (pull remote changes).
///
/// # Send / !Send handling
///
/// `Replica::sync()` takes `&mut Box<dyn Server>` where `Server` is
/// `async_trait(?Send)`, producing a `!Send` future. Axum handlers must
/// return `Send` futures. We solve this by running the sync on a
/// dedicated OS thread with its own single-threaded tokio runtime, which
/// can safely `.await` `!Send` futures.
///
/// # Timeout
///
/// Both lock acquisition AND sync execution are independently capped
/// at [`sync_timeout()`]. If either exceeds this, the caller continues
/// without sync (eventual consistency).
///
/// # Corruption handling
///
/// If the sync fails with a SQLite corruption error, the user is
/// quarantined (all cached connections evicted, further requests
/// return 503).
///
/// Errors are logged but do not fail the caller (eventual consistency).
pub async fn sync_user_replica(state: &AppState, user_id: &str) -> Result<(), anyhow::Error> {
    let context = BridgeSyncContext::from_state(state);
    sync_user_replica_inner(&context, user_id).await
}

async fn sync_user_replica_inner(
    context: &BridgeSyncContext,
    user_id: &str,
) -> Result<(), anyhow::Error> {
    // If master_key is None, sync not configured — return early silently
    let master_key = match context.config.master_key {
        Some(key) => key,
        None => return Ok(()),
    };

    // Acquire per-user sync lock with timeout. If another sync is already
    // in progress, we wait up to the sync timeout for it to finish. If the
    // lock isn't acquired in time, we skip (eventual consistency — the
    // running sync will handle it). At most ONE OS thread per user.
    let lock = context.sync_in_flight.get_or_insert(user_id);
    let timeout = sync_timeout();
    let guard = match tokio::time::timeout(timeout, lock.lock_owned()).await {
        Ok(guard) => guard,
        Err(_) => {
            tracing::debug!(
                "sync lock acquisition timed out after {}s for {user_id}, skipping",
                timeout.as_secs()
            );
            return Ok(()); // Eventual consistency — prior sync covers this user
        }
    };

    let has_active_device = context
        .store
        .list_devices(user_id)
        .await?
        .into_iter()
        .any(|device| device.status == "active");
    if !has_active_device {
        return Ok(());
    }

    // The guard is passed into do_sync and held for the entire OS thread
    // lifetime (including timeout). This guarantees no overlapping threads.
    do_sync_for_user(context, user_id, master_key, guard).await
}

/// Sync the user's canonical Replica for a request routed via a specific device.
///
/// Devices still authenticate independently, but they all speak to the same
/// shared user sync chain. This helper preserves the old call sites.
pub async fn sync_device_replica(
    state: &AppState,
    device: &DeviceRecord,
) -> Result<(), anyhow::Error> {
    sync_user_replica(state, &device.user_id).await
}

/// Inner sync logic. The `_guard` is held for the entire duration (including
/// the OS thread) to prevent overlapping syncs for the same user.
async fn do_sync_for_user(
    context: &BridgeSyncContext,
    user_id: &str,
    master_key: [u8; 32],
    _guard: tokio::sync::OwnedMutexGuard<()>,
) -> Result<(), anyhow::Error> {
    let replica_arc = context.replica_manager.get_replica(user_id).await?;
    let user_dir = context.user_replica_dir(user_id);
    let sync_storage = SyncStorage::open(&user_dir).map_err(|e| {
        if replica::is_corruption_in_chain(&e) {
            tracing::error!("Sync bridge storage corruption on open for {user_id}: {e}");
            context.quarantine_user(user_id);
        }
        e
    })?;
    let replica_record = context
        .store
        .get_replica_by_user(user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("user {user_id} has no canonical sync identity"))?;
    let cryptor = crate::tc_sync::cryptor_cache::get_or_create_canonical(
        user_id,
        &replica_record,
        &master_key,
    )?;

    // Run sync on a dedicated OS thread with its own current-thread runtime.
    //
    // This is necessary because Replica::sync() produces a !Send future
    // (Server trait uses async_trait(?Send)), but Axum handlers must be Send.
    // We can't nest runtimes in spawn_blocking (which has a runtime context),
    // so we spawn a plain OS thread instead.
    //
    // Corruption is checked inside the OS thread so that even if the caller
    // times out and drops the receiver, corruption is still detected and the
    // user is quarantined. The guard is passed through to the OS thread
    // so it's held until the thread completes (not until the caller times out).
    do_sync(
        replica_arc,
        sync_storage,
        cryptor,
        context.clone(),
        user_id.to_string(),
        _guard,
    )
    .await?;

    let synced_client_ids = context
        .store
        .list_devices(user_id)
        .await?
        .into_iter()
        .filter(|device| device.status == "active")
        .map(|device| device.client_id)
        .collect::<Vec<_>>();
    context
        .runtime_sync
        .mark_devices_synced_to_current(user_id, synced_client_ids.iter().map(String::as_str));
    Ok(())
}

/// Evict a user's cached canonical SyncCryptor.
///
/// Called when a user's sync identity changes (e.g. key rotation).
pub fn evict_cryptor(user_id: &str) {
    crate::tc_sync::cryptor_cache::evict_canonical(user_id);
}

/// Execute the sync on a dedicated OS thread with a current-thread runtime.
///
/// The `!Send` types (`Box<dyn Server>`, `MutexGuard`) only exist inside
/// the thread, keeping the outer future `Send`.
///
/// Applies a timeout from [`sync_timeout()`] — if the OS thread doesn't
/// complete within that window, we return a timeout error. Corruption is
/// checked inside the thread so quarantine still fires even if the caller
/// has timed out.
async fn do_sync(
    replica_arc: Arc<Mutex<Replica<SqliteStorage>>>,
    sync_storage: SyncStorage,
    cryptor: Arc<crate::tc_sync::crypto::SyncCryptor>,
    context: BridgeSyncContext,
    user_id: String,
    _in_flight_guard: tokio::sync::OwnedMutexGuard<()>,
) -> Result<(), anyhow::Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();

    // The _in_flight_guard is captured by the thread closure — it's held
    // until the thread finishes (not until the caller times out). This
    // prevents overlapping OS threads for the same user.
    std::thread::spawn(move || {
        let _guard = _in_flight_guard; // explicitly capture into thread
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(anyhow::anyhow!("Failed to build sync runtime: {e}")));
                return;
            }
        };

        let sync_user_id = user_id.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(async move {
                let mut replica = replica_arc.lock().await;
                let bridge = SyncBridgeServer::new_with_arc(sync_storage, cryptor);
                let mut server: Box<dyn taskchampion::Server> = Box::new(bridge);
                replica.sync(&mut server, false).await.map_err(|e| {
                    anyhow::anyhow!("bridge sync failed for user {sync_user_id}: {e}")
                })?;
                Ok::<_, anyhow::Error>(())
            })
        }))
        .unwrap_or_else(|panic| {
            let message = if let Some(msg) = panic.downcast_ref::<&str>() {
                (*msg).to_string()
            } else if let Some(msg) = panic.downcast_ref::<String>() {
                msg.clone()
            } else {
                "unknown panic".to_string()
            };
            Err(anyhow::anyhow!("sync bridge panicked: {message}"))
        });

        // Handle corruption INSIDE the thread — even if the caller timed out
        // and dropped the receiver, corruption is still detected and the user
        // is quarantined.
        if let Err(ref e) = result {
            if replica::is_corruption_in_chain(e) {
                tracing::error!("Sync bridge corruption detected for {user_id}: {e}");
                context.quarantine_user(&user_id);
            }
        }

        // Ignore send error — caller may have timed out / been cancelled
        let _ = tx.send(result);
    });

    // Apply timeout on the oneshot receive
    let timeout = sync_timeout();
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(anyhow::anyhow!(
            "sync thread dropped without sending result"
        )),
        Err(_) => {
            tracing::warn!("sync bridge timed out after {}s", timeout.as_secs());
            Err(anyhow::anyhow!(
                "sync timed out after {}s",
                timeout.as_secs()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    use ring::rand::{SecureRandom, SystemRandom};

    use crate::app_state::AppState;
    use crate::config::{ServerConfig, ServerSection};
    use crate::store::models::NewUser;
    use crate::store::sqlite::SqliteConfigStore;
    use crate::store::ConfigStore;

    use super::{scheduler_debounce, BridgeFreshnessTracker, SyncPriority};

    async fn scheduler_test_state() -> (tempfile::TempDir, AppState, String) {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let db_path = data_dir.join("config.sqlite");
        let store: Arc<dyn ConfigStore> = Arc::new(
            SqliteConfigStore::new(&db_path.to_string_lossy())
                .await
                .unwrap(),
        );
        store.run_migrations().await.unwrap();

        let user = store
            .create_user(&NewUser {
                username: "scheduler_user".to_string(),
                password_hash: "not-real".to_string(),
            })
            .await
            .unwrap();

        let mut master_key = [0u8; 32];
        SystemRandom::new().fill(&mut master_key).unwrap();

        let mut secret_bytes = [0u8; 32];
        SystemRandom::new().fill(&mut secret_bytes).unwrap();
        let encrypted = crate::crypto::encrypt_secret(&secret_bytes, &master_key).unwrap();
        let encrypted_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &encrypted);

        let client_id = uuid::Uuid::new_v4().to_string();
        store
            .create_replica(&user.id, &client_id, &encrypted_b64)
            .await
            .unwrap();
        let device_secret_raw =
            crate::crypto::derive_device_secret(&secret_bytes, client_id.as_bytes()).unwrap();
        let device_secret_enc =
            crate::crypto::encrypt_secret(&device_secret_raw, &master_key).unwrap();
        let device_secret_enc_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &device_secret_enc,
        );
        store
            .create_device(
                &user.id,
                &client_id,
                "Scheduler test device",
                Some(&device_secret_enc_b64),
            )
            .await
            .unwrap();

        std::fs::create_dir_all(data_dir.join("users").join(&user.id)).unwrap();

        let config = ServerConfig {
            server: ServerSection {
                host: "127.0.0.1".to_string(),
                port: 0,
                data_dir: data_dir.clone(),
                public_base_url: Some("https://test.invalid".to_string()),
                trust_forwarded_headers: false,
            },
            admin: Default::default(),
            backup_dir: Some(data_dir.join("backups")),
            backup_retention_count: 7,
            llm: None,
            audit: Default::default(),
            master_key: Some(master_key),
        };

        let state = AppState::new(store, &config);
        (tmp, state, user.id)
    }

    #[test]
    fn new_devices_start_stale_until_initial_sync() {
        let tracker = BridgeFreshnessTracker::new();

        assert!(tracker.device_needs_sync("user-a", "device-a"));

        tracker.mark_device_synced_to_current("user-a", "device-a");
        assert!(!tracker.device_needs_sync("user-a", "device-a"));
    }

    #[test]
    fn canonical_changes_stale_other_devices_but_not_synced_one() {
        let tracker = BridgeFreshnessTracker::new();

        tracker.mark_device_synced_to_current("user-a", "device-a");
        tracker.mark_device_synced_to_current("user-a", "device-b");
        tracker.mark_canonical_changed_and_device_synced("user-a", "device-a");

        assert!(!tracker.device_needs_sync("user-a", "device-a"));
        assert!(tracker.device_needs_sync("user-a", "device-b"));

        tracker.mark_device_synced_to_current("user-a", "device-b");
        assert!(!tracker.device_needs_sync("user-a", "device-b"));
    }

    #[tokio::test]
    async fn scheduler_coalesces_per_user_and_keeps_highest_priority() {
        let (_tmp, state, user_id) = scheduler_test_state().await;
        let scheduler = state.runtime_sync.scheduler();

        scheduler.schedule(&user_id, SyncPriority::Low, "test_low");
        scheduler.schedule(&user_id, SyncPriority::High, "test_high");

        let lane = scheduler
            .inner
            .lanes
            .get(&user_id)
            .expect("expected one user lane after scheduling");
        assert_eq!(
            lane.pending_priority.load(Ordering::Acquire),
            SyncPriority::High as u8
        );
        assert!(
            lane.running.load(Ordering::Acquire),
            "lane should be marked running after first schedule"
        );
        drop(lane);
        assert_eq!(
            scheduler.inner.lanes.len(),
            1,
            "repeated schedule calls for the same user should coalesce into one lane"
        );

        let mut drained = false;
        for _ in 0..30 {
            tokio::time::sleep(scheduler_debounce() + Duration::from_millis(25)).await;
            let lane = scheduler.inner.lanes.get(&user_id);
            let no_pending = lane
                .as_ref()
                .map(|lane| lane.pending_priority.load(Ordering::Acquire) == 0)
                .unwrap_or(true);
            let not_running = lane
                .as_ref()
                .map(|lane| !lane.running.load(Ordering::Acquire))
                .unwrap_or(true);
            if no_pending && not_running {
                drained = true;
                break;
            }
        }

        assert!(
            drained,
            "lane should eventually drain its pending work after the queued sync completes"
        );
    }
}
