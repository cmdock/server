use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::tc_sync::storage::SyncStorage;

use super::{
    BackupFileEntry, BackupManifest, BackupServiceError, BackupSnapshotSummary, ReplicaBackupEntry,
    CURRENT_CONFIG_SCHEMA_VERSION, CURRENT_SERVER_VERSION,
};

pub(super) fn manifest_to_summary(path: &Path, manifest: &BackupManifest) -> BackupSnapshotSummary {
    BackupSnapshotSummary {
        timestamp: path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| manifest.created_at.clone()),
        path: path.display().to_string(),
        server_version: manifest.server_version.clone(),
        users: manifest.contents.replicas.len(),
        task_count: sum_replica_task_counts(&manifest.contents.replicas),
        total_size_bytes: manifest.total_size_bytes,
        secrets_included: manifest.secrets_included,
        backup_type: manifest.backup_type.clone(),
    }
}

pub(super) fn read_and_validate_snapshot(
    snapshot_dir: &Path,
) -> Result<BackupManifest, BackupServiceError> {
    let manifest_path = snapshot_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Err(BackupServiceError::ManifestMissing);
    }
    let manifest = load_manifest(&manifest_path)?;
    verify_snapshot_files(snapshot_dir, &manifest)?;
    Ok(manifest)
}

pub(super) fn load_manifest(path: &Path) -> Result<BackupManifest, BackupServiceError> {
    let raw = fs::read(path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => BackupServiceError::ManifestMissing,
        _ => BackupServiceError::ManifestInvalid(err.to_string()),
    })?;
    serde_json::from_slice(&raw).map_err(|err| BackupServiceError::ManifestInvalid(err.to_string()))
}

pub(super) fn verify_restore_compatibility(
    snapshot_dir: &Path,
    manifest: &BackupManifest,
) -> Result<(), BackupServiceError> {
    if version_gt(&manifest.minimum_server_version, CURRENT_SERVER_VERSION) {
        return Err(BackupServiceError::VersionIncompatible {
            required: manifest.minimum_server_version.clone(),
            current: CURRENT_SERVER_VERSION.to_string(),
        });
    }
    if manifest.schema_version > CURRENT_CONFIG_SCHEMA_VERSION {
        return Err(BackupServiceError::SchemaIncompatible {
            snapshot: manifest.schema_version,
            current: CURRENT_CONFIG_SCHEMA_VERSION,
        });
    }

    for entry in &manifest.contents.replicas {
        if let Some(sync_schema_version) = entry.sync_schema_version {
            if sync_schema_version > SyncStorage::current_schema_version() {
                return Err(BackupServiceError::SchemaIncompatible {
                    snapshot: sync_schema_version,
                    current: SyncStorage::current_schema_version(),
                });
            }
        }
        if let Some(sync_db) = &entry.sync_db {
            let path = resolve_snapshot_path(snapshot_dir, &sync_db.file)?;
            if path.exists() {
                let version = SyncStorage::inspect_schema_version(&path).map_err(|err| {
                    BackupServiceError::ManifestInvalid(format!(
                        "failed to inspect sync schema {}: {err}",
                        path.display()
                    ))
                })?;
                if version.is_some_and(|version| version > SyncStorage::current_schema_version()) {
                    return Err(BackupServiceError::SchemaIncompatible {
                        snapshot: version.unwrap_or_default(),
                        current: SyncStorage::current_schema_version(),
                    });
                }
            }
        }
    }

    Ok(())
}

fn verify_snapshot_files(
    snapshot_dir: &Path,
    manifest: &BackupManifest,
) -> Result<(), BackupServiceError> {
    verify_file_entry(snapshot_dir, &manifest.contents.config_db)?;
    for entry in &manifest.contents.replicas {
        if let Some(replica_db) = &entry.replica_db {
            verify_file_entry(snapshot_dir, replica_db)?;
        }
        if let Some(sync_db) = &entry.sync_db {
            verify_file_entry(snapshot_dir, sync_db)?;
        }
    }
    Ok(())
}

fn verify_file_entry(
    snapshot_dir: &Path,
    entry: &BackupFileEntry,
) -> Result<(), BackupServiceError> {
    let path = resolve_snapshot_path(snapshot_dir, &entry.file)?;
    if !path.exists() {
        return Err(BackupServiceError::ChecksumMismatch(format!(
            "{} is missing from backup",
            entry.file
        )));
    }
    let metadata = fs::metadata(&path).map_err(|err| {
        BackupServiceError::ChecksumMismatch(format!("failed to stat {}: {err}", entry.file))
    })?;
    if metadata.len() != entry.size_bytes {
        return Err(BackupServiceError::ChecksumMismatch(format!(
            "{} size mismatch (expected {}, got {})",
            entry.file,
            entry.size_bytes,
            metadata.len()
        )));
    }
    let digest = sha256_file(&path).map_err(|err| {
        BackupServiceError::ChecksumMismatch(format!("failed to hash {}: {err}", entry.file))
    })?;
    if digest != entry.sha256 {
        return Err(BackupServiceError::ChecksumMismatch(format!(
            "{} checksum mismatch",
            entry.file
        )));
    }
    Ok(())
}

pub(super) fn resolve_snapshot_path(
    snapshot_dir: &Path,
    relative: &str,
) -> Result<PathBuf, BackupServiceError> {
    let path = Path::new(relative);
    if path.is_absolute()
        || relative.contains('\\')
        || relative.split('/').any(|segment| segment == "..")
    {
        return Err(BackupServiceError::ManifestInvalid(format!(
            "invalid backup path {relative}"
        )));
    }
    Ok(snapshot_dir.join(path))
}

pub(super) fn file_entry_from_path(
    snapshot_dir: &Path,
    path: &Path,
) -> Result<BackupFileEntry, BackupServiceError> {
    let metadata = fs::metadata(path).map_err(|err| {
        BackupServiceError::Internal(format!("failed to stat {}: {err}", path.display()))
    })?;
    let relative = path
        .strip_prefix(snapshot_dir)
        .map_err(|err| BackupServiceError::Internal(format!("failed to relativise path: {err}")))?;
    Ok(BackupFileEntry {
        file: relative.to_string_lossy().replace('\\', "/"),
        size_bytes: metadata.len(),
        sha256: sha256_file(path).map_err(|err| {
            BackupServiceError::Internal(format!("failed to hash {}: {err}", path.display()))
        })?,
    })
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

pub(super) fn inspect_task_count(path: &Path) -> Option<u64> {
    if !path.exists() {
        return None;
    }
    let conn = rusqlite::Connection::open(path).ok()?;
    let has_tasks_table: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tasks')",
            [],
            |row| row.get(0),
        )
        .ok()?;
    if !has_tasks_table {
        return None;
    }
    conn.query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get::<_, i64>(0))
        .ok()
        .map(|count| count.max(0) as u64)
}

pub(super) fn sum_replica_task_counts(replicas: &[ReplicaBackupEntry]) -> Option<u64> {
    replicas
        .iter()
        .map(|entry| entry.task_count)
        .try_fold(0_u64, |total, count| count.map(|count| total + count))
}

pub(super) fn validate_snapshot_name(value: &str) -> Result<(), BackupServiceError> {
    if value.is_empty()
        || value.contains('/')
        || value.contains('\\')
        || value.contains("..")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.'))
    {
        return Err(BackupServiceError::SnapshotNotFound);
    }
    Ok(())
}

fn version_gt(required: &str, current: &str) -> bool {
    parse_version(required) > parse_version(current)
}

fn parse_version(raw: &str) -> Vec<u64> {
    raw.split(['-', '+'])
        .next()
        .unwrap_or(raw)
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}
