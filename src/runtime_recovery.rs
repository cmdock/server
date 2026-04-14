use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock;

use dashmap::DashSet;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::metrics;
use crate::recovery::StartupRecoverySummary;
use crate::replica::ReplicaManager;
use crate::sync_bridge::BridgeFreshnessTracker;
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

#[derive(Clone)]
pub struct RuntimeRecoveryCoordinator {
    data_dir: PathBuf,
    quarantined_users: Arc<DashSet<String>>,
    startup_recovery_snapshot: Arc<RwLock<Option<StartupRecoverySnapshot>>>,
    replica_manager: ReplicaManager,
    sync_storage_manager: Arc<SyncStorageManager>,
    bridge_freshness: BridgeFreshnessTracker,
}

impl RuntimeRecoveryCoordinator {
    pub fn for_data_dir(data_dir: &Path) -> Self {
        let replica_manager = ReplicaManager::new(data_dir);
        let sync_storage_manager = Arc::new(SyncStorageManager::new(data_dir));
        Self::new(
            data_dir,
            replica_manager,
            sync_storage_manager,
            BridgeFreshnessTracker::new(),
        )
    }

    pub fn new(
        data_dir: &Path,
        replica_manager: ReplicaManager,
        sync_storage_manager: Arc<SyncStorageManager>,
        bridge_freshness: BridgeFreshnessTracker,
    ) -> Self {
        let coordinator = Self {
            data_dir: data_dir.to_path_buf(),
            quarantined_users: Arc::new(DashSet::new()),
            startup_recovery_snapshot: Arc::new(RwLock::new(None)),
            replica_manager,
            sync_storage_manager,
            bridge_freshness,
        };
        coordinator.update_quarantined_user_metric();
        coordinator
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
        self.evict_user_runtime_state(user_id);
        self.update_quarantined_user_metric();
        inserted
    }

    pub fn clear_user_quarantine(&self, user_id: &str) -> bool {
        self.remove_offline_marker(user_id);
        let was_quarantined = self.quarantined_users.remove(user_id).is_some();
        self.evict_user_runtime_state(user_id);
        self.update_quarantined_user_metric();
        was_quarantined
    }

    pub fn quarantine_user(&self, user_id: &str) {
        self.mark_user_offline(user_id);
    }

    pub fn sync_offline_markers_now(&self) {
        self.sync_offline_markers();
    }

    fn evict_user_runtime_state(&self, user_id: &str) {
        self.replica_manager.evict(user_id);
        self.sync_storage_manager.evict_user(user_id);
        crate::sync_bridge::evict_cryptor(user_id);
        self.bridge_freshness.clear_user(user_id);
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
                        self.evict_user_runtime_state(&user_id);
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
                self.evict_user_runtime_state(&user_id);
            }
        }

        self.update_quarantined_user_metric();
    }

    fn update_quarantined_user_metric(&self) {
        metrics::set_quarantined_user_count(self.quarantined_users.len());
    }
}
