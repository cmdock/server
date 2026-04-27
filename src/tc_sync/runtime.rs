use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;

use crate::app_state::AppState;
use crate::metrics as m;
use crate::replica;
use crate::sync_bridge::{self, SyncPriority};
use crate::user_runtime::{block_quarantined_user, handle_sync_open_status};

use super::storage::SyncStorage;

pub use crate::user_runtime::handle_sync_error;

/// TTL for cached sync storage connections (5 minutes).
const SYNC_STORAGE_TTL: Duration = Duration::from_secs(300);

/// Interval between reaper sweeps (60 seconds).
const SYNC_STORAGE_REAP_INTERVAL: Duration = Duration::from_secs(60);

/// A cached SyncStorage connection with last-access tracking.
struct CachedStorage {
    storage: Arc<std::sync::Mutex<SyncStorage>>,
    last_accessed: AtomicU64,
}

impl CachedStorage {
    fn new(storage: SyncStorage) -> Self {
        Self {
            storage: Arc::new(std::sync::Mutex::new(storage)),
            last_accessed: AtomicU64::new(Self::now_secs()),
        }
    }

    fn touch(&self) {
        self.last_accessed
            .store(Self::now_secs(), Ordering::Relaxed);
    }

    fn age_secs(&self) -> u64 {
        Self::now_secs().saturating_sub(self.last_accessed.load(Ordering::Relaxed))
    }

    fn now_secs() -> u64 {
        static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let epoch = EPOCH.get_or_init(Instant::now);
        epoch.elapsed().as_secs()
    }
}

/// Per-user SyncStorage connection pool backed by DashMap.
///
/// `get_or_open` is synchronous — it is called from within handlers that
/// run the actual storage operation inside `spawn_blocking`. The DashMap
/// lookup is fast; opening a new SQLite connection is the slow path.
pub struct SyncStorageManager {
    connections: dashmap::DashMap<String, CachedStorage>,
    data_dir: PathBuf,
}

impl SyncStorageManager {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            connections: dashmap::DashMap::new(),
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// Note: callers should go through `open_sync_storage()` which enforces
    /// quarantine checks. Direct use of this method bypasses quarantine.
    pub(crate) fn get_or_open(
        &self,
        user_id: &str,
    ) -> Result<Arc<std::sync::Mutex<SyncStorage>>, StatusCode> {
        if let Some(entry) = self.connections.get(user_id) {
            entry.touch();
            return Ok(Arc::clone(&entry.storage));
        }

        let user_dir = self.data_dir.join("users").join(user_id);
        std::fs::create_dir_all(&user_dir).map_err(|e| {
            tracing::error!("Failed to create user dir for {user_id}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        let storage = SyncStorage::open(&user_dir).map_err(|e| {
            if replica::is_corruption_in_chain(&e) {
                tracing::error!("Sync storage corruption on open for {user_id}: {e}");
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                tracing::error!("Failed to open sync storage for {user_id}: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        })?;

        let cached = CachedStorage::new(storage);
        let arc = Arc::clone(&cached.storage);
        self.connections.insert(user_id.to_string(), cached);
        m::set_sync_storage_cached_count(self.connections.len());
        Ok(arc)
    }

    pub fn evict_user(&self, user_id: &str) -> bool {
        let removed = self.connections.remove(user_id).is_some();
        if removed {
            m::set_sync_storage_cached_count(self.connections.len());
        }
        removed
    }

    pub fn start_reaper(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SYNC_STORAGE_REAP_INTERVAL).await;
                let ttl_secs = SYNC_STORAGE_TTL.as_secs();
                let mut evicted = 0usize;
                manager.connections.retain(|_client_id, entry| {
                    if entry.age_secs() > ttl_secs {
                        evicted += 1;
                        false
                    } else {
                        true
                    }
                });
                if evicted > 0 {
                    tracing::debug!("Sync storage reaper evicted {evicted} idle connections");
                }
                m::set_sync_storage_cached_count(manager.connections.len());
            }
        });
    }
}

/// RAII guard that increments sync_storage_in_flight on creation
/// and decrements on drop.
pub struct InFlightGuard {
    _private: (),
}

#[allow(clippy::new_without_default)]
impl InFlightGuard {
    pub fn new() -> Self {
        m::sync_storage_in_flight_inc();
        Self { _private: () }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        m::sync_storage_in_flight_dec();
    }
}

pub fn open_sync_storage(
    state: &AppState,
    device: &crate::store::models::DeviceRecord,
) -> Result<Arc<std::sync::Mutex<SyncStorage>>, StatusCode> {
    let user_id = &device.user_id;
    let client_id = &device.client_id;
    if user_id.contains('/') || user_id.contains('\\') || user_id.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }
    if client_id.contains('/') || client_id.contains('\\') || client_id.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }
    block_quarantined_user(state, user_id, "sync")?;
    state
        .sync_storage_manager
        .get_or_open(user_id)
        .map_err(|status| handle_sync_open_status(state, user_id, status, "open", "sync"))
}

fn handle_bridge_write_error(err: &anyhow::Error) -> StatusCode {
    if replica::is_corruption_in_chain(err) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

pub async fn reconcile_after_tc_write(
    state: &AppState,
    device: &crate::store::models::DeviceRecord,
    source: &'static str,
) -> Result<(), StatusCode> {
    match sync_bridge::sync_user_replica(state, &device.user_id).await {
        Ok(()) => {
            state
                .runtime_sync
                .mark_canonical_changed_and_device_synced(&device.user_id, &device.client_id);
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                "Bridge sync after TC write failed for user {} device {}: {e}",
                device.user_id,
                device.client_id
            );
            let status = handle_bridge_write_error(&e);
            if status != StatusCode::OK {
                return Err(status);
            }

            state
                .runtime_sync
                .schedule(&device.user_id, SyncPriority::High, source);
            Ok(())
        }
    }
}

pub async fn sync_device_if_stale(state: &AppState, device: &crate::store::models::DeviceRecord) {
    if !state
        .runtime_sync
        .device_needs_sync(&device.user_id, &device.client_id)
    {
        return;
    }

    if let Err(e) = sync_bridge::sync_user_replica(state, &device.user_id).await {
        tracing::warn!(
            "Bridge sync before TC read failed for user {} device {}: {e}",
            device.user_id,
            device.client_id
        );
        state
            .runtime_sync
            .schedule(&device.user_id, SyncPriority::High, "tc_read_fallback");
    }
}
