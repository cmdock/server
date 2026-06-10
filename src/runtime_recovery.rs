use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock;

use dashmap::{DashMap, DashSet};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use utoipa::ToSchema;

use crate::merged_sync_gateway::storage::MergedSyncStorageManager;
use crate::metrics;
use crate::recovery::StartupRecoverySummary;
use crate::replica::ReplicaManager;
use crate::runtime_sync::BridgeFreshnessTracker;
use crate::tc_sync::runtime::SyncStorageManager;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "totalUsers": 12,
    "healthyUsers": 10,
    "rebuildableUsers": 1,
    "needsOperatorAttentionUsers": 1,
    "alreadyOfflineUsers": 0,
    "newlyOfflinedUsers": ["86a9cca3-5689-41e4-8361-8075c9c49b38"],
    "orphanUserDirs": []
}))]
pub struct StartupRecoverySnapshot {
    pub total_users: usize,
    pub healthy_users: usize,
    pub rebuildable_users: usize,
    pub needs_operator_attention_users: usize,
    pub already_offline_users: usize,
    pub newly_offlined_users: Vec<String>,
    pub orphan_user_dirs: Vec<String>,
}

impl From<StartupRecoverySummary> for StartupRecoverySnapshot {
    fn from(value: StartupRecoverySummary) -> Self {
        Self {
            total_users: value.total_users,
            healthy_users: value.healthy_users,
            rebuildable_users: value.rebuildable_users,
            needs_operator_attention_users: value.needs_operator_attention_users,
            already_offline_users: value.already_offline_users,
            newly_offlined_users: value.newly_offlined_users,
            orphan_user_dirs: value.orphan_user_dirs,
        }
    }
}

/// Outcome of `RuntimeRecoveryCoordinator::evict_user` — reports whether
/// each cache had an entry to evict, for operator-facing diagnostic
/// messages.
#[derive(Debug, Clone, Copy)]
pub struct EvictUserOutcome {
    pub replica_evicted: bool,
    pub sync_evicted: bool,
    pub merged_sync_evicted: bool,
}

#[derive(Clone)]
pub struct RuntimeRecoveryCoordinator {
    data_dir: PathBuf,
    quarantined_users: Arc<DashSet<String>>,
    startup_recovery_snapshot: Arc<RwLock<Option<StartupRecoverySnapshot>>>,
    replica_manager: ReplicaManager,
    sync_storage_manager: Arc<SyncStorageManager>,
    merged_sync_storage_manager: Arc<MergedSyncStorageManager>,
    bridge_freshness: BridgeFreshnessTracker,
    /// Per-user mutation lock for task-key allocation (server#130). Held
    /// across `service::add_task` from `reserve_task_key_pending` through
    /// `commit_task_key`. Reused by Phase 4 backfill and the Phase 1
    /// reaper coordinator (see `src/task_keys/reaper.rs`).
    ///
    /// Single-server-process assumption — DashMap is in-memory and not
    /// shared across processes. Cache invalidation on `evict_user` keeps
    /// it bounded under churn.
    task_mutation_locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
    /// Phase 4 backfill fast-path cache (server#130). `true` means the
    /// `users.task_keys_migrated_at` column is populated for this user
    /// — backfill has already run and `ensure_user_migrated` can return
    /// without acquiring the per-user mutation lock.
    ///
    /// The DB column is the source of truth. The cache is a hot-path
    /// optimisation; first-access on a fresh process always misses and
    /// falls through to the post-lock double-check. Eviction lives on
    /// `evict_user` so restore / delete-user / offline-quarantine all
    /// invalidate the cache through the single shared owner.
    task_keys_migration_cache: Arc<DashMap<String, bool>>,
}

impl RuntimeRecoveryCoordinator {
    pub fn for_data_dir(data_dir: &Path) -> Self {
        let replica_manager = ReplicaManager::new(data_dir);
        let sync_storage_manager = Arc::new(SyncStorageManager::new(data_dir));
        let merged_sync_storage_manager = Arc::new(MergedSyncStorageManager::new(data_dir));
        Self::new(
            data_dir,
            replica_manager,
            sync_storage_manager,
            merged_sync_storage_manager,
            BridgeFreshnessTracker::new(),
        )
    }

    pub fn new(
        data_dir: &Path,
        replica_manager: ReplicaManager,
        sync_storage_manager: Arc<SyncStorageManager>,
        merged_sync_storage_manager: Arc<MergedSyncStorageManager>,
        bridge_freshness: BridgeFreshnessTracker,
    ) -> Self {
        let coordinator = Self {
            data_dir: data_dir.to_path_buf(),
            quarantined_users: Arc::new(DashSet::new()),
            startup_recovery_snapshot: Arc::new(RwLock::new(None)),
            replica_manager,
            sync_storage_manager,
            merged_sync_storage_manager,
            bridge_freshness,
            task_mutation_locks: Arc::new(DashMap::new()),
            task_keys_migration_cache: Arc::new(DashMap::new()),
        };
        coordinator.update_quarantined_user_metric();
        coordinator
    }

    /// Get-or-create the per-user mutation lock for task-key allocation.
    /// Callers hold the returned mutex across reservation → attach → TC
    /// commit → finalise; the reaper coordinator follows the same lock
    /// order. Returns `Arc<Mutex<()>>` so the caller can `lock().await`
    /// on the cloned handle without holding a `DashMap` ref guard
    /// (which is `!Send` past an await point).
    pub fn task_mutation_lock(&self, user_id: &str) -> Arc<Mutex<()>> {
        self.task_mutation_locks
            .entry(user_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Fast-path check for the Phase 4 task-keys backfill (server#130).
    /// Returns `true` only when a previous `mark_task_keys_migration_complete`
    /// call has populated the cache for this user. A `false` return is
    /// authoritative-cache-miss, NOT authoritative-not-migrated — callers
    /// must fall through to a DB read under the per-user mutation lock to
    /// double-check before deciding to run the backfill.
    pub fn task_keys_migration_marked(&self, user_id: &str) -> bool {
        self.task_keys_migration_cache
            .get(user_id)
            .map(|e| *e.value())
            .unwrap_or(false)
    }

    /// Mark the Phase 4 backfill as complete for the given user. Called
    /// by `task_keys::backfill::backfill_user_task_keys` once the DB row
    /// has its `task_keys_migrated_at` populated, and on cache-miss
    /// fast-path read paths once the column has been observed populated.
    pub fn mark_task_keys_migration_complete(&self, user_id: &str) {
        self.task_keys_migration_cache
            .insert(user_id.to_string(), true);
    }

    pub fn start(&self) {
        let coordinator = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                interval.tick().await;
                coordinator.sync_offline_markers();
            }
        });
    }

    pub fn is_user_quarantined(&self, user_id: &str) -> bool {
        self.quarantined_users.contains(user_id)
    }

    pub fn quarantined_user_count(&self) -> usize {
        self.quarantined_users.len()
    }

    pub fn startup_recovery_snapshot(&self) -> Option<StartupRecoverySnapshot> {
        self.startup_recovery_snapshot
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    pub fn set_startup_recovery_snapshot(&self, summary: StartupRecoverySummary) {
        if let Ok(mut guard) = self.startup_recovery_snapshot.write() {
            *guard = Some(summary.into());
        }
    }

    pub fn user_offline_marker(&self, user_id: &str) -> PathBuf {
        self.data_dir.join("users").join(user_id).join(".offline")
    }

    pub fn mark_user_offline(&self, user_id: &str) -> bool {
        self.persist_offline_marker(user_id);
        let inserted = self.quarantined_users.insert(user_id.to_string());
        self.evict_user(user_id);
        self.update_quarantined_user_metric();
        inserted
    }

    pub fn clear_user_quarantine(&self, user_id: &str) -> bool {
        self.remove_offline_marker(user_id);
        let was_quarantined = self.quarantined_users.remove(user_id).is_some();
        self.evict_user(user_id);
        self.update_quarantined_user_metric();
        was_quarantined
    }

    pub fn quarantine_user(&self, user_id: &str) {
        self.mark_user_offline(user_id);
    }

    pub fn sync_offline_markers_now(&self) {
        self.sync_offline_markers();
    }

    /// Evict all per-user runtime state caches: replica, sync storage,
    /// merged gateway state, and bridge freshness. The single shared owner
    /// of the user-level eviction recipe — see ADR-0002 review 2026-05-04
    /// § P1 / issue #121.
    pub fn evict_user(&self, user_id: &str) -> EvictUserOutcome {
        let replica_evicted = self.replica_manager.evict(user_id);
        let sync_evicted = self.sync_storage_manager.evict_user(user_id);
        let merged_sync_evicted = self.merged_sync_storage_manager.evict_user(user_id);
        crate::merged_sync_gateway::inbound::evict_inbound_add_lock(user_id);
        crate::merged_sync_gateway::projection::evict_merged_append_lock(user_id);
        self.bridge_freshness.clear_user(user_id);
        // Drop the per-user mutation lock (server#130). Safe even if a
        // mutation was holding it — Arc keeps the lock alive on the
        // holder's stack, and a fresh reservation under the same user_id
        // post-eviction lazily creates a new entry.
        self.task_mutation_locks.remove(user_id);
        // Drop the Phase 4 backfill fast-path cache entry (server#130).
        // Restore / delete-user / offline-quarantine all funnel through
        // `evict_user`, so this single line keeps the migration-status
        // cache aligned with the DB across every reset path that the
        // CLAUDE.md § Runtime cache eviction gotcha pins.
        self.task_keys_migration_cache.remove(user_id);
        EvictUserOutcome {
            replica_evicted,
            sync_evicted,
            merged_sync_evicted,
        }
    }

    /// Evict all per-device runtime state: device cryptor cache and bridge
    /// freshness for the (user, device) pair. The single shared owner of
    /// the device-level eviction recipe — see ADR-0002 review 2026-05-04
    /// § P1 / issue #121.
    pub fn evict_device(&self, user_id: &str, client_id: &str) {
        crate::tc_sync::cryptor_cache::evict_device(client_id);
        self.bridge_freshness.remove_device(user_id, client_id);
    }

    fn persist_offline_marker(&self, user_id: &str) {
        let marker = self.user_offline_marker(user_id);
        if let Some(parent) = marker.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                tracing::warn!("failed to create offline marker dir for user {user_id}: {err}");
                return;
            }
        }
        if let Err(err) = std::fs::write(&marker, b"offline\n") {
            tracing::warn!("failed to write offline marker for user {user_id}: {err}");
        }
    }

    fn remove_offline_marker(&self, user_id: &str) {
        let marker = self.user_offline_marker(user_id);
        if let Err(err) = std::fs::remove_file(&marker) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("failed to remove offline marker for user {user_id}: {err}");
            }
        }
    }

    fn sync_offline_markers(&self) {
        let users_dir = self.data_dir.join("users");
        let mut expected = HashSet::new();

        if let Ok(entries) = std::fs::read_dir(&users_dir) {
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                let user_id = entry.file_name().to_string_lossy().to_string();
                if entry.path().join(".offline").exists() {
                    expected.insert(user_id.clone());
                    if !self.quarantined_users.contains(&user_id) {
                        self.quarantined_users.insert(user_id.clone());
                        self.evict_user(&user_id);
                    }
                }
            }
        }

        let current: Vec<String> = self
            .quarantined_users
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for user_id in current {
            if !expected.contains(&user_id) {
                self.quarantined_users.remove(&user_id);
                self.evict_user(&user_id);
            }
        }

        self.update_quarantined_user_metric();
    }

    fn update_quarantined_user_metric(&self) {
        metrics::set_quarantined_user_count(self.quarantined_users.len());
    }
}
