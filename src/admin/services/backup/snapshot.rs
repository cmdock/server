use std::fs;
use std::path::Path;

use chrono::Utc;

use crate::{app_state::AppState, store::models::UserRecord, tc_sync::storage::SyncStorage};

use super::{
    manifest::{file_entry_from_path, inspect_task_count, load_manifest, sum_replica_task_counts},
    BackupContents, BackupSecrets, BackupServiceError, BackupSnapshotSummary, ReplicaBackupEntry,
    RestoreInstructions, ServerConfigSnapshot, SnapshotKind, WebhookSecret,
    BACKUP_MANIFEST_VERSION, CURRENT_CONFIG_SCHEMA_VERSION, CURRENT_SERVER_VERSION,
};

pub(super) async fn create_snapshot(
    state: &AppState,
    backup_root: &Path,
    kind: SnapshotKind,
    include_secrets: bool,
    apply_retention: bool,
) -> Result<BackupSnapshotSummary, BackupServiceError> {
    let now = Utc::now();
    let snapshot_name = kind.snapshot_name(now);
    let snapshot_dir = backup_root.join(&snapshot_name);
    fs::create_dir_all(&snapshot_dir)
        .map_err(|err| BackupServiceError::BackupDirNotWritable(err.to_string()))?;

    let result = create_snapshot_inner(
        state,
        backup_root,
        &snapshot_dir,
        &snapshot_name,
        now,
        kind,
        include_secrets,
        apply_retention,
    )
    .await;

    if result.is_err() {
        let _ = fs::remove_dir_all(&snapshot_dir);
    }

    result
}

#[allow(clippy::too_many_arguments)]
async fn create_snapshot_inner(
    state: &AppState,
    backup_root: &Path,
    snapshot_dir: &Path,
    snapshot_name: &str,
    now: chrono::DateTime<Utc>,
    kind: SnapshotKind,
    include_secrets: bool,
    apply_retention: bool,
) -> Result<BackupSnapshotSummary, BackupServiceError> {
    state
        .store
        .checkpoint_database()
        .await
        .map_err(|err| BackupServiceError::CheckpointFailed(err.to_string()))?;

    let config_dest = snapshot_dir.join("config.sqlite");
    state
        .store
        .backup_to_path(&config_dest)
        .await
        .map_err(|err| BackupServiceError::Internal(format!("failed to copy config DB: {err}")))?;
    let config_db = file_entry_from_path(snapshot_dir, &config_dest)?;

    let users = state
        .store
        .list_users()
        .await
        .map_err(|err| BackupServiceError::Internal(format!("failed to list users: {err}")))?;

    let mut replicas = Vec::with_capacity(users.len());
    for user in &users {
        replicas.push(snapshot_user(state, snapshot_dir, user).await?);
    }

    let total_size_bytes = config_db.size_bytes
        + replicas
            .iter()
            .map(|entry| {
                entry.replica_db.as_ref().map_or(0, |file| file.size_bytes)
                    + entry.sync_db.as_ref().map_or(0, |file| file.size_bytes)
            })
            .sum::<u64>();

    let manifest = super::BackupManifest {
        cmdock_backup_version: BACKUP_MANIFEST_VERSION,
        created_at: now.to_rfc3339(),
        server_version: CURRENT_SERVER_VERSION.to_string(),
        minimum_server_version: CURRENT_SERVER_VERSION.to_string(),
        schema_version: CURRENT_CONFIG_SCHEMA_VERSION,
        backup_type: kind.as_str().to_string(),
        consistency: "checkpoint_complete".to_string(),
        secrets_included: include_secrets,
        total_size_bytes,
        server_config_snapshot: ServerConfigSnapshot {
            public_base_url: state.config.server.public_base_url.clone(),
            backup_dir: state
                .config
                .backup_dir
                .as_ref()
                .map(|path| path.display().to_string()),
            backup_retention_count: state.config.backup_retention_count,
        },
        contents: BackupContents {
            config_db,
            replicas,
        },
        secrets: include_secrets.then(|| BackupSecrets {
            admin_token: state.operator_token(),
            webhook_secrets: Vec::<WebhookSecret>::new(),
        }),
        restore_instructions: RestoreInstructions {
            minimum_server_version: CURRENT_SERVER_VERSION.to_string(),
            steps: vec![
                format!("1. Install cmdock-server >= {CURRENT_SERVER_VERSION}"),
                "2. Configure backup_dir to point to the directory containing this backup"
                    .to_string(),
                "3. Run: cmdock-admin backup restore <timestamp>".to_string(),
                "4. If secrets were not included: reconfigure admin token".to_string(),
                "5. Verify: cmdock-admin doctor".to_string(),
            ],
            manual_steps: vec![
                "If cmdock-admin is not available:".to_string(),
                "1. Stop the server".to_string(),
                "2. Copy config.sqlite to the server data directory".to_string(),
                "3. Copy users/* into the server data directory".to_string(),
                "4. Start the server".to_string(),
                "5. Reconfigure admin token and TLS if secrets were not included".to_string(),
            ],
        },
    };

    let manifest_path = snapshot_dir.join("manifest.json");
    let manifest_json = serde_json::to_vec_pretty(&manifest)
        .map_err(|err| BackupServiceError::Internal(format!("failed to encode manifest: {err}")))?;
    fs::write(&manifest_path, manifest_json)
        .map_err(|err| BackupServiceError::Internal(format!("failed to write manifest: {err}")))?;

    if apply_retention && state.config.backup_retention_count > 0 {
        enforce_retention(backup_root, state.config.backup_retention_count)?;
    }

    Ok(BackupSnapshotSummary {
        timestamp: snapshot_name.to_string(),
        path: snapshot_dir.display().to_string(),
        server_version: CURRENT_SERVER_VERSION.to_string(),
        users: users.len(),
        task_count: sum_replica_task_counts(&manifest.contents.replicas),
        total_size_bytes,
        secrets_included: include_secrets,
        backup_type: kind.as_str().to_string(),
    })
}

async fn snapshot_user(
    state: &AppState,
    snapshot_dir: &Path,
    user: &UserRecord,
) -> Result<ReplicaBackupEntry, BackupServiceError> {
    let user_dir = state.user_replica_dir(&user.id);
    let snapshot_user_dir = snapshot_dir.join("users").join(&user.id);
    let replica_src = user_dir.join("taskchampion.sqlite3");
    let sync_src = user_dir.join("sync.sqlite");

    let replica_db = if replica_src.exists() {
        checkpoint_sqlite_file(&replica_src)?;
        copy_file(
            &replica_src,
            &snapshot_user_dir.join("taskchampion.sqlite3"),
        )?;
        Some(file_entry_from_path(
            snapshot_dir,
            &snapshot_user_dir.join("taskchampion.sqlite3"),
        )?)
    } else {
        None
    };

    let sync_db = if sync_src.exists() {
        checkpoint_sqlite_file(&sync_src)?;
        copy_file(&sync_src, &snapshot_user_dir.join("sync.sqlite"))?;
        Some(file_entry_from_path(
            snapshot_dir,
            &snapshot_user_dir.join("sync.sqlite"),
        )?)
    } else {
        None
    };

    let sync_schema_version = if sync_src.exists() {
        Some(
            SyncStorage::inspect_schema_version(&sync_src)
                .map_err(|err| {
                    BackupServiceError::Internal(format!(
                        "failed to inspect sync schema for user {}: {err}",
                        user.id
                    ))
                })?
                .unwrap_or_default(),
        )
    } else {
        None
    };

    let devices =
        state.store.list_devices(&user.id).await.map_err(|err| {
            BackupServiceError::Internal(format!("failed to list devices: {err}"))
        })?;
    let last_sync = devices
        .iter()
        .filter_map(|device| device.last_sync_at.clone())
        .max();

    Ok(ReplicaBackupEntry {
        user_id: user.id.clone(),
        username: user.username.clone(),
        replica_db,
        sync_db,
        task_count: inspect_task_count(&replica_src),
        last_sync,
        sync_schema_version,
    })
}

fn enforce_retention(backup_root: &Path, retention_count: usize) -> Result<(), BackupServiceError> {
    let mut backups = Vec::new();
    for entry in fs::read_dir(backup_root)
        .map_err(|err| BackupServiceError::Internal(format!("failed to read backup dir: {err}")))?
        .flatten()
    {
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }
        if let Ok(manifest) = load_manifest(&manifest_path) {
            backups.push((entry.path(), manifest.created_at));
        }
    }
    backups.sort_by(|a, b| b.1.cmp(&a.1));
    for (path, _) in backups.into_iter().skip(retention_count) {
        fs::remove_dir_all(&path).map_err(|err| {
            BackupServiceError::Internal(format!(
                "failed to remove expired backup {}: {err}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

pub(super) fn checkpoint_sqlite_file(path: &Path) -> Result<(), BackupServiceError> {
    let conn = rusqlite::Connection::open(path).map_err(|err| {
        BackupServiceError::CheckpointFailed(format!("failed to open {}: {err}", path.display()))
    })?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|err| BackupServiceError::CheckpointFailed(format!("{}: {err}", path.display())))
}

pub(super) fn copy_file(src: &Path, dst: &Path) -> Result<(), BackupServiceError> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            BackupServiceError::Internal(format!("failed to create {}: {err}", parent.display()))
        })?;
    }
    fs::copy(src, dst).map_err(|err| {
        BackupServiceError::Internal(format!(
            "failed to copy {} -> {}: {err}",
            src.display(),
            dst.display()
        ))
    })?;
    Ok(())
}
