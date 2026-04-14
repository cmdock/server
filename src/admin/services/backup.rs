use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app_state::AppState;

#[path = "backup/manifest.rs"]
mod manifest;
#[path = "backup/restore.rs"]
mod restore;
#[path = "backup/snapshot.rs"]
mod snapshot;

pub(super) const BACKUP_MANIFEST_VERSION: u32 = 1;
// Keep this in step with the highest config migration number in `migrations/`.
pub(super) const CURRENT_CONFIG_SCHEMA_VERSION: i64 = 18;
pub(super) const CURRENT_SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct BackupSnapshotSummary {
    pub timestamp: String,
    pub path: String,
    pub server_version: String,
    pub users: usize,
    pub task_count: Option<u64>,
    pub total_size_bytes: u64,
    pub secrets_included: bool,
    pub backup_type: String,
}

#[derive(Debug, Clone)]
pub struct RestoredReplicaSummary {
    pub user_id: String,
    pub username: String,
    pub task_count: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RestoreSummary {
    pub restored_from: String,
    pub pre_restore_snapshot: String,
    pub users_restored: usize,
    pub replicas_restored: usize,
    pub secrets_restored: bool,
    pub config_database_restored: bool,
    pub replicas: Vec<RestoredReplicaSummary>,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupServiceError {
    #[error("backup_dir is not configured")]
    BackupDirNotConfigured,
    #[error("backup staging directory is not writable: {0}")]
    BackupDirNotWritable(String),
    #[error("another backup or restore is already running")]
    BackupInProgress,
    #[error("failed to checkpoint SQLite state: {0}")]
    CheckpointFailed(String),
    #[error("snapshot not found")]
    SnapshotNotFound,
    #[error("manifest.json is missing")]
    ManifestMissing,
    #[error("manifest.json is invalid: {0}")]
    ManifestInvalid(String),
    #[error("checksum verification failed: {0}")]
    ChecksumMismatch(String),
    #[error("backup requires server version {required}, current server is {current}")]
    VersionIncompatible { required: String, current: String },
    #[error("backup schema version {snapshot} is newer than current server supports ({current})")]
    SchemaIncompatible { snapshot: i64, current: i64 },
    #[error("another backup or restore is already running")]
    RestoreInProgress,
    #[error("{message}")]
    RestoreFailedRolledBack {
        message: String,
        pre_restore_snapshot: String,
    },
    #[error("{0}")]
    Internal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnapshotKind {
    Full,
    PreRestore,
}

impl SnapshotKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::PreRestore => "pre_restore",
        }
    }

    fn snapshot_name(self, now: chrono::DateTime<Utc>) -> String {
        let stamp = now.format("%Y-%m-%dT%H-%M-%S").to_string();
        match self {
            Self::Full => stamp,
            Self::PreRestore => format!("pre-restore-{stamp}"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct BackupManifest {
    cmdock_backup_version: u32,
    created_at: String,
    server_version: String,
    minimum_server_version: String,
    schema_version: i64,
    backup_type: String,
    consistency: String,
    secrets_included: bool,
    total_size_bytes: u64,
    server_config_snapshot: ServerConfigSnapshot,
    contents: BackupContents,
    #[serde(skip_serializing_if = "Option::is_none")]
    secrets: Option<BackupSecrets>,
    restore_instructions: RestoreInstructions,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct ServerConfigSnapshot {
    public_base_url: Option<String>,
    backup_dir: Option<String>,
    backup_retention_count: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct BackupContents {
    config_db: BackupFileEntry,
    replicas: Vec<ReplicaBackupEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct BackupFileEntry {
    file: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct ReplicaBackupEntry {
    user_id: String,
    username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    replica_db: Option<BackupFileEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_db: Option<BackupFileEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_sync: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_schema_version: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct BackupSecrets {
    #[serde(skip_serializing_if = "Option::is_none")]
    admin_token: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    webhook_secrets: Vec<WebhookSecret>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct WebhookSecret {
    webhook_id: String,
    secret: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct RestoreInstructions {
    minimum_server_version: String,
    steps: Vec<String>,
    manual_steps: Vec<String>,
}

pub async fn create_backup(
    state: &AppState,
    include_secrets: bool,
) -> Result<BackupSnapshotSummary, BackupServiceError> {
    let _guard = state
        .admin_operation_lock
        .try_lock()
        .map_err(|_| BackupServiceError::BackupInProgress)?;
    let backup_root = configured_backup_root(state)?;
    ensure_backup_root_writable(&backup_root)?;
    snapshot::create_snapshot(
        state,
        &backup_root,
        SnapshotKind::Full,
        include_secrets,
        true,
    )
    .await
}

pub async fn list_backups(
    state: &AppState,
) -> Result<Vec<BackupSnapshotSummary>, BackupServiceError> {
    let Some(backup_root) = state.config.backup_dir.as_ref() else {
        return Ok(Vec::new());
    };
    if !backup_root.exists() {
        return Ok(Vec::new());
    }

    let mut backups = Vec::new();
    let entries = fs::read_dir(backup_root)
        .map_err(|err| BackupServiceError::Internal(format!("failed to read backup dir: {err}")))?;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }
        match manifest::load_manifest(&manifest_path) {
            Ok(manifest) => backups.push(manifest::manifest_to_summary(&entry.path(), &manifest)),
            Err(err) => {
                tracing::warn!(
                    path = %manifest_path.display(),
                    error = %err,
                    "Skipping invalid backup manifest during list"
                );
            }
        }
    }

    backups.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(backups)
}

pub async fn restore_backup(
    state: &AppState,
    timestamp: &str,
) -> Result<RestoreSummary, BackupServiceError> {
    manifest::validate_snapshot_name(timestamp)?;

    let _guard = state
        .admin_operation_lock
        .try_lock()
        .map_err(|_| BackupServiceError::RestoreInProgress)?;
    let backup_root =
        configured_backup_root(state).or(Err(BackupServiceError::SnapshotNotFound))?;
    let snapshot_dir = backup_root.join(timestamp);
    if !snapshot_dir.is_dir() {
        return Err(BackupServiceError::SnapshotNotFound);
    }

    let manifest = manifest::read_and_validate_snapshot(&snapshot_dir)?;
    manifest::verify_restore_compatibility(&snapshot_dir, &manifest)?;

    let pre_restore =
        snapshot::create_snapshot(state, &backup_root, SnapshotKind::PreRestore, true, false)
            .await
            .map_err(|err| BackupServiceError::Internal(err.to_string()))?;
    let pre_restore_dir = backup_root.join(&pre_restore.timestamp);
    let pre_restore_manifest = manifest::read_and_validate_snapshot(&pre_restore_dir)?;

    let previous_quarantine = restore::collect_quarantined_users(state).await?;
    restore::quarantine_all_users(state).await?;

    match restore::apply_snapshot_to_live(state, &snapshot_dir, &manifest).await {
        Ok(()) => {
            restore::restore_quarantine_state(state, &previous_quarantine).await?;
            Ok(RestoreSummary {
                restored_from: timestamp.to_string(),
                pre_restore_snapshot: pre_restore.timestamp,
                users_restored: manifest.contents.replicas.len(),
                replicas_restored: manifest
                    .contents
                    .replicas
                    .iter()
                    .filter(|entry| entry.replica_db.is_some())
                    .count(),
                secrets_restored: manifest
                    .secrets
                    .as_ref()
                    .and_then(|secrets| secrets.admin_token.as_ref())
                    .is_some(),
                config_database_restored: true,
                replicas: manifest
                    .contents
                    .replicas
                    .iter()
                    .filter(|entry| entry.replica_db.is_some())
                    .map(|entry| RestoredReplicaSummary {
                        user_id: entry.user_id.clone(),
                        username: entry.username.clone(),
                        task_count: entry.task_count,
                    })
                    .collect(),
            })
        }
        Err(restore_err) => {
            let rollback_result =
                restore::apply_snapshot_to_live(state, &pre_restore_dir, &pre_restore_manifest)
                    .await;
            let quarantine_result =
                restore::restore_quarantine_state(state, &previous_quarantine).await;

            let mut message = format!(
                "restore from {timestamp} failed; server rolled back to {pre_restore_snapshot}: {restore_err}",
                pre_restore_snapshot = pre_restore.timestamp
            );
            if let Err(rollback_err) = rollback_result {
                message.push_str(&format!("; rollback also failed: {rollback_err}"));
            }
            if let Err(quarantine_err) = quarantine_result {
                message.push_str(&format!("; quarantine reset failed: {quarantine_err}"));
            }
            Err(BackupServiceError::RestoreFailedRolledBack {
                message,
                pre_restore_snapshot: pre_restore.timestamp,
            })
        }
    }
}

pub(super) fn configured_backup_root(state: &AppState) -> Result<PathBuf, BackupServiceError> {
    state
        .config
        .backup_dir
        .clone()
        .ok_or(BackupServiceError::BackupDirNotConfigured)
}

pub(super) fn ensure_backup_root_writable(path: &Path) -> Result<(), BackupServiceError> {
    fs::create_dir_all(path)
        .map_err(|err| BackupServiceError::BackupDirNotWritable(err.to_string()))?;
    let probe = path.join(format!(".cmdock-backup-write-test-{}", Uuid::new_v4()));
    fs::write(&probe, b"ok")
        .map_err(|err| BackupServiceError::BackupDirNotWritable(err.to_string()))?;
    let _ = fs::remove_file(&probe);
    Ok(())
}
