use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::app_state::AppState;
use crate::metrics;
use crate::recovery::{
    assess_user_recovery, RecoveryStatus, StartupRecoverySummary, UserRecoveryAssessment,
};
use crate::runtime_recovery::RuntimeRecoveryCoordinator;
use crate::store::ConfigStore;

#[derive(Clone)]
pub struct RecoveryCoordinator {
    store: Arc<dyn ConfigStore>,
    data_dir: PathBuf,
    runtime: Arc<RuntimeRecoveryCoordinator>,
}

impl RecoveryCoordinator {
    pub fn for_running_state(state: &AppState) -> Self {
        Self {
            store: state.store.clone(),
            data_dir: state.data_dir.clone(),
            runtime: state.recovery_runtime.clone(),
        }
    }

    pub fn for_local(store: Arc<dyn ConfigStore>, data_dir: &Path) -> Self {
        Self {
            store,
            data_dir: data_dir.to_path_buf(),
            runtime: Arc::new(RuntimeRecoveryCoordinator::for_data_dir(data_dir)),
        }
    }

    pub async fn assess_user(&self, user_id: &str) -> anyhow::Result<UserRecoveryAssessment> {
        self.assess_user_with_source(user_id, "service").await
    }

    pub async fn assess_user_with_source(
        &self,
        user_id: &str,
        source: &'static str,
    ) -> anyhow::Result<UserRecoveryAssessment> {
        let assessment = assess_user_recovery(&self.store, &self.data_dir, user_id).await?;
        metrics::record_recovery_assessment(
            match assessment.status {
                RecoveryStatus::Healthy => "healthy",
                RecoveryStatus::Rebuildable => "rebuildable",
                RecoveryStatus::NeedsOperatorAttention => "needs_operator_attention",
            },
            source,
        );
        Ok(assessment)
    }

    pub fn take_user_offline(
        &self,
        user_id: &str,
        source: &'static str,
        client_ip: &str,
        reason: Option<&str>,
    ) -> bool {
        let changed = self.runtime.mark_user_offline(user_id);
        metrics::record_recovery_transition("offline", source, changed);
        match reason {
            Some(reason) => tracing::info!(
                target: "audit",
                action = "admin.user.quarantine",
                source = source,
                client_ip = client_ip,
                user_id = %user_id,
                reason = reason,
                changed = changed,
            ),
            None => tracing::info!(
                target: "audit",
                action = "admin.user.quarantine",
                source = source,
                client_ip = client_ip,
                user_id = %user_id,
                changed = changed,
            ),
        }
        changed
    }

    pub fn bring_user_online(&self, user_id: &str, source: &'static str, client_ip: &str) -> bool {
        let changed = self.runtime.clear_user_quarantine(user_id);
        metrics::record_recovery_transition("online", source, changed);
        tracing::info!(
            target: "audit",
            action = "admin.user.unquarantine",
            source = source,
            client_ip = client_ip,
            user_id = %user_id,
            changed = changed,
        );
        changed
    }

    pub fn is_user_offline(&self, user_id: &str) -> bool {
        self.runtime.is_user_quarantined(user_id)
    }

    pub fn sync_offline_markers(&self) {
        self.runtime.sync_offline_markers_now();
    }

    pub async fn startup_assessment(&self) -> anyhow::Result<StartupRecoverySummary> {
        self.sync_offline_markers();

        let users = self.store.list_users().await?;
        let known_user_ids: HashSet<String> = users.iter().map(|user| user.id.clone()).collect();

        let mut orphan_user_dirs = Vec::new();
        let users_dir = self.data_dir.join("users");
        if users_dir.exists() {
            for entry in std::fs::read_dir(&users_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let user_id = entry.file_name().to_string_lossy().to_string();
                if !known_user_ids.contains(&user_id) {
                    orphan_user_dirs.push(user_id);
                }
            }
        }
        orphan_user_dirs.sort();

        let mut healthy_users = 0;
        let mut rebuildable_users = 0;
        let mut needs_operator_attention_users = 0;
        let mut newly_offlined_users = Vec::new();
        let already_offline_users = users
            .iter()
            .filter(|user| self.is_user_offline(&user.id))
            .count();

        for user in users {
            let assessment = self.assess_user_with_source(&user.id, "startup").await?;
            match assessment.status {
                RecoveryStatus::Healthy => {
                    healthy_users += 1;
                }
                RecoveryStatus::Rebuildable => {
                    rebuildable_users += 1;
                    tracing::warn!(
                        user_id = %user.id,
                        shared_sync_db_exists = assessment.shared_sync_db_exists,
                        shared_sync_schema_version = ?assessment.shared_sync_schema_version,
                        shared_sync_upgrade_needed = assessment.shared_sync_upgrade_needed,
                        notes = ?assessment.notes,
                        "startup recovery assessment: user is rebuildable"
                    );
                }
                RecoveryStatus::NeedsOperatorAttention => {
                    needs_operator_attention_users += 1;
                    if !self.is_user_offline(&user.id) {
                        self.take_user_offline(
                            &user.id,
                            "startup",
                            "local",
                            Some("startup_recovery_assessment"),
                        );
                        newly_offlined_users.push(user.id.clone());
                    }
                    tracing::error!(
                        user_id = %user.id,
                        missing_device_secrets = ?assessment.missing_device_secrets,
                        shared_sync_db_exists = assessment.shared_sync_db_exists,
                        shared_sync_schema_version = ?assessment.shared_sync_schema_version,
                        shared_sync_db_error = ?assessment.shared_sync_db_error,
                        notes = ?assessment.notes,
                        "startup recovery assessment: user requires operator attention and is offline"
                    );
                }
            }
        }

        if !orphan_user_dirs.is_empty() {
            tracing::warn!(
                orphan_user_dirs = ?orphan_user_dirs,
                "startup recovery assessment: orphan user directories found on disk"
            );
        }

        let summary = StartupRecoverySummary {
            total_users: known_user_ids.len(),
            healthy_users,
            rebuildable_users,
            needs_operator_attention_users,
            already_offline_users,
            newly_offlined_users,
            orphan_user_dirs,
        };

        metrics::set_startup_recovery_summary(
            summary.total_users,
            summary.healthy_users,
            summary.rebuildable_users,
            summary.needs_operator_attention_users,
            summary.already_offline_users,
            summary.newly_offlined_users.len(),
            summary.orphan_user_dirs.len(),
        );
        self.runtime.set_startup_recovery_snapshot(summary.clone());

        Ok(summary)
    }
}
