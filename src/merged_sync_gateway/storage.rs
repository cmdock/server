//! Merged-chain sync storage cache.
//!
//! This is deliberately distinct from the legacy per-user sync storage manager:
//! the merged chain lives under `data/users/{user_id}/merged/sync.sqlite` and is
//! the durable TW-visible projection chain owned by the gateway.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;

use crate::replica;
use crate::tc_sync::storage::SyncStorage;

const MERGED_SYNC_STORAGE_TTL: Duration = Duration::from_secs(300);
const MERGED_SYNC_STORAGE_REAP_INTERVAL: Duration = Duration::from_secs(60);

struct CachedMergedStorage {
    storage: Arc<std::sync::Mutex<SyncStorage>>,
    last_accessed: AtomicU64,
}

impl CachedMergedStorage {
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
        EPOCH.get_or_init(Instant::now).elapsed().as_secs()
    }
}

/// Per-user merged-chain `SyncStorage` connection cache.
pub struct MergedSyncStorageManager {
    connections: dashmap::DashMap<String, CachedMergedStorage>,
    data_dir: PathBuf,
}

impl MergedSyncStorageManager {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            connections: dashmap::DashMap::new(),
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// Open the user's merged protocol chain at
    /// `data/users/{user_id}/merged/sync.sqlite`.
    pub(crate) fn get_or_open(
        &self,
        user_id: &str,
    ) -> Result<Arc<std::sync::Mutex<SyncStorage>>, StatusCode> {
        if let Some(entry) = self.connections.get(user_id) {
            entry.touch();
            return Ok(Arc::clone(&entry.storage));
        }

        let user_dir = self.data_dir.join("users").join(user_id);
        let merged_replica_dir = user_dir.join("merged").join("replica");
        std::fs::create_dir_all(&merged_replica_dir).map_err(|err| {
            tracing::error!(
                "Failed to create merged replica dir for {user_id} at {}: {err}",
                merged_replica_dir.display()
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        let storage = SyncStorage::open_merged(&user_dir).map_err(|err| {
            if replica::is_corruption_in_chain(&err) {
                tracing::error!("Merged sync storage corruption on open for {user_id}: {err}");
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                tracing::error!("Failed to open merged sync storage for {user_id}: {err}");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        })?;

        let cached = CachedMergedStorage::new(storage);
        let arc = Arc::clone(&cached.storage);
        self.connections.insert(user_id.to_string(), cached);
        Ok(arc)
    }

    pub fn evict_user(&self, user_id: &str) -> bool {
        self.connections.remove(user_id).is_some()
    }

    pub fn cached_count(&self) -> usize {
        self.connections.len()
    }

    pub fn start_reaper(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(MERGED_SYNC_STORAGE_REAP_INTERVAL).await;
                let ttl_secs = MERGED_SYNC_STORAGE_TTL.as_secs();
                let mut evicted = 0usize;
                manager.connections.retain(|_, entry| {
                    if entry.age_secs() > ttl_secs {
                        evicted += 1;
                        false
                    } else {
                        true
                    }
                });
                if evicted > 0 {
                    tracing::debug!(
                        "Merged sync storage reaper evicted {evicted} idle connections"
                    );
                }
            }
        });
    }
}

pub fn ensure_merged_sync_storage(data_dir: &Path, user_id: &str) -> anyhow::Result<()> {
    let user_dir = data_dir.join("users").join(user_id);
    let merged_replica_dir = user_dir.join("merged").join("replica");
    std::fs::create_dir_all(&merged_replica_dir)?;
    SyncStorage::open_merged(&user_dir)?;
    Ok(())
}

pub fn open_merged_sync_storage(
    state: &crate::app_state::AppState,
    user_id: &str,
) -> Result<Arc<std::sync::Mutex<SyncStorage>>, StatusCode> {
    if user_id.contains('/') || user_id.contains('\\') || user_id.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }
    crate::user_runtime::block_quarantined_user(state, user_id, "merged_sync")?;
    state
        .merged_sync_storage_manager
        .get_or_open(user_id)
        .map_err(|status| {
            crate::user_runtime::handle_sync_open_status(
                state,
                user_id,
                status,
                "open",
                "merged_sync",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn opens_merged_sync_storage_under_separate_layout() {
        let temp = TempDir::new().unwrap();
        let manager = MergedSyncStorageManager::new(temp.path());
        let storage = manager.get_or_open("user-1").unwrap();
        drop(storage.lock().unwrap());

        assert!(temp.path().join("users/user-1/merged/replica").is_dir());
        assert!(temp.path().join("users/user-1/merged/sync.sqlite").exists());
        assert_eq!(manager.cached_count(), 1);
        assert!(manager.evict_user("user-1"));
        assert_eq!(manager.cached_count(), 0);
    }
}
