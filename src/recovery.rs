use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::app_state::AppState;
use crate::store::ConfigStore;
use crate::tc_sync::storage::SyncStorage;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStatus {
    Healthy,
    Rebuildable,
    NeedsOperatorAttention,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "userId": "86a9cca3-5689-41e4-8361-8075c9c49b38",
    "status": "rebuildable",
    "userDirExists": true,
    "canonicalReplicaExists": false,
    "syncIdentityExists": true,
    "sharedSyncDbExists": false,
    "sharedSyncSchemaVersion": null,
    "expectedSyncSchemaVersion": 2,
    "sharedSyncUpgradeNeeded": false,
    "deviceCount": 2,
    "activeDeviceCount": 2,
    "missingDeviceSecrets": [],
    "sharedSyncDbError": null,
    "notes": [
        "The shared sync DB is missing. It can be rebuilt from canonical state."
    ]
}))]
pub struct UserRecoveryAssessment {
    pub user_id: String,
    pub status: RecoveryStatus,
    pub user_dir_exists: bool,
    pub canonical_replica_exists: bool,
    pub sync_identity_exists: bool,
    pub shared_sync_db_exists: bool,
    pub shared_sync_schema_version: Option<i64>,
    pub expected_sync_schema_version: i64,
    pub shared_sync_upgrade_needed: bool,
    pub device_count: usize,
    pub active_device_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub missing_device_secrets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub shared_sync_db_error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupRecoverySummary {
    pub total_users: usize,
    pub healthy_users: usize,
    pub rebuildable_users: usize,
    pub needs_operator_attention_users: usize,
    pub already_offline_users: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub newly_offlined_users: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub orphan_user_dirs: Vec<String>,
}

pub async fn assess_user_recovery(
    store: &Arc<dyn ConfigStore>,
    data_dir: &Path,
    user_id: &str,
) -> anyhow::Result<UserRecoveryAssessment> {
    let devices = store.list_devices(user_id).await?;
    let active_devices: Vec<_> = devices.iter().filter(|d| d.status == "active").collect();
    let sync_identity_exists = store.get_replica_by_user(user_id).await?.is_some();

    let user_dir = data_dir.join("users").join(user_id);
    let canonical_replica_exists = user_dir.join("taskchampion.sqlite3").exists();
    let user_dir_exists = user_dir.exists();
    let shared_sync_db_path = SyncStorage::user_db_path(&user_dir);
    let shared_sync_db_exists = shared_sync_db_path.exists();

    let (shared_sync_schema_version, shared_sync_db_error) = if shared_sync_db_exists {
        match SyncStorage::inspect_schema_version(&shared_sync_db_path) {
            Ok(version) => (version, None),
            Err(err) => (None, Some(err.to_string())),
        }
    } else {
        (None, None)
    };
    let expected_sync_schema_version = SyncStorage::current_schema_version();
    let shared_sync_upgrade_needed = shared_sync_db_exists
        && shared_sync_db_error.is_none()
        && shared_sync_schema_version != Some(expected_sync_schema_version);

    let missing_device_secrets: Vec<String> = active_devices
        .iter()
        .filter(|device| device.encryption_secret_enc.is_none())
        .map(|device| device.client_id.clone())
        .collect();

    let mut notes = Vec::new();
    let status = if !missing_device_secrets.is_empty() {
        notes.push("One or more active devices are missing stored encryption secrets.".to_string());
        RecoveryStatus::NeedsOperatorAttention
    } else if !active_devices.is_empty() && !sync_identity_exists {
        notes.push(
            "The user has registered devices but no canonical sync identity in config.sqlite."
                .to_string(),
        );
        RecoveryStatus::NeedsOperatorAttention
    } else if !active_devices.is_empty() && !canonical_replica_exists {
        notes.push(
            "The canonical TaskChampion replica is missing. It can be rebuilt or re-created from sync state."
                .to_string(),
        );
        RecoveryStatus::Rebuildable
    } else if shared_sync_schema_version.is_some_and(|v| v > expected_sync_schema_version) {
        let actual_version = shared_sync_schema_version.unwrap();
        notes.push(format!(
            "The shared sync DB schema_version is newer than this binary supports ({actual_version} > {expected_sync_schema_version})."
        ));
        RecoveryStatus::NeedsOperatorAttention
    } else if (sync_identity_exists || !active_devices.is_empty()) && !shared_sync_db_exists {
        notes.push(
            "The shared sync DB is missing. It can be rebuilt from canonical state.".to_string(),
        );
        RecoveryStatus::Rebuildable
    } else if shared_sync_db_error.is_some() {
        notes.push(
            "The shared sync DB could not be inspected cleanly. It may need rebuild or operator review."
                .to_string(),
        );
        RecoveryStatus::Rebuildable
    } else if shared_sync_upgrade_needed {
        notes.push(
            "The shared sync DB is older than the current runtime storage level and needs uplift."
                .to_string(),
        );
        RecoveryStatus::Rebuildable
    } else {
        if devices.is_empty() {
            notes.push("No devices are currently registered for this user.".to_string());
        }
        RecoveryStatus::Healthy
    };

    Ok(UserRecoveryAssessment {
        user_id: user_id.to_string(),
        status,
        user_dir_exists,
        canonical_replica_exists,
        sync_identity_exists,
        shared_sync_db_exists,
        shared_sync_schema_version,
        expected_sync_schema_version,
        shared_sync_upgrade_needed,
        device_count: devices.len(),
        active_device_count: active_devices.len(),
        missing_device_secrets,
        shared_sync_db_error,
        notes,
    })
}

pub async fn run_startup_recovery_assessment(
    state: &AppState,
) -> anyhow::Result<StartupRecoverySummary> {
    crate::admin::services::recovery::RecoveryCoordinator::for_running_state(state)
        .startup_assessment()
        .await
}
