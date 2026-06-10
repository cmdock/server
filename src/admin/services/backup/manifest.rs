use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{ffi::ErrorCode, OpenFlags};
use sha2::{Digest, Sha256};

use crate::{store::models::TaskScopeKind, tc_sync::storage::SyncStorage};

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

    verify_task_scope_metadata(snapshot_dir, manifest)?;

    for entry in &manifest.contents.replicas {
        if let Some(sync_schema_version) = entry.sync_schema_version {
            if sync_schema_version > SyncStorage::current_schema_version() {
                return Err(BackupServiceError::SchemaIncompatible {
                    snapshot: sync_schema_version,
                    current: SyncStorage::current_schema_version(),
                });
            }
        }
        let sync_entry = entry.sync_db.as_ref().or(entry.merged_sync_db.as_ref());
        if let Some(sync_db) = sync_entry {
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

fn verify_task_scope_metadata(
    snapshot_dir: &Path,
    manifest: &BackupManifest,
) -> Result<(), BackupServiceError> {
    if manifest
        .contents
        .replicas
        .iter()
        .all(|entry| entry.task_scope.is_none())
    {
        return Ok(());
    }

    let config_path = resolve_snapshot_path(snapshot_dir, &manifest.contents.config_db.file)?;
    let conn = match rusqlite::Connection::open_with_flags(
        &config_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(conn) => {
            conn.busy_timeout(Duration::from_secs(1)).map_err(|err| {
                BackupServiceError::ManifestInvalid(format!(
                    "failed to configure config DB read timeout for Task Scope metadata validation {}: {err}",
                    config_path.display()
                ))
            })?;
            conn
        }
        Err(err) if should_defer_to_restore_apply(&err) => {
            // Keep restore behavior for otherwise checksum-valid but structurally
            // broken config DBs: the restore apply path reports the DB failure and
            // rolls back, as older backups did before Task Scope manifest metadata.
            tracing::warn!(
                path = %config_path.display(),
                error = %err,
                "Skipping Task Scope manifest metadata validation because snapshot config DB is structurally unreadable"
            );
            return Ok(());
        }
        Err(err) => {
            return Err(BackupServiceError::ManifestInvalid(format!(
                "failed to open config DB for Task Scope metadata validation {}: {err}",
                config_path.display()
            )));
        }
    };
    let has_task_scopes = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'task_scopes')",
        [],
        |row| row.get::<_, bool>(0),
    );
    let has_task_scopes = match has_task_scopes {
        Ok(has_task_scopes) => has_task_scopes,
        Err(err) if should_defer_to_restore_apply(&err) => {
            // If sqlite_master itself cannot be read because the DB is corrupt,
            // defer to restore apply so the existing corrupt-DB rollback path
            // remains the source of the error.
            tracing::warn!(
                path = %config_path.display(),
                error = %err,
                "Skipping Task Scope manifest metadata validation because snapshot config schema is structurally unreadable"
            );
            return Ok(());
        }
        Err(err) => {
            return Err(BackupServiceError::ManifestInvalid(format!(
                "failed to inspect task_scopes table in {}: {err}",
                config_path.display()
            )));
        }
    };
    if !has_task_scopes {
        return Err(BackupServiceError::ManifestInvalid(
            "manifest contains Task Scope metadata but config DB has no task_scopes table"
                .to_string(),
        ));
    }

    for entry in &manifest.contents.replicas {
        let Some(task_scope) = &entry.task_scope else {
            // Mixed manifests are accepted during rolling upgrades: replicas
            // with Task Scope metadata are checked; legacy entries remain valid.
            continue;
        };
        if task_scope.kind != TaskScopeKind::Personal.as_str()
            || task_scope.task_scope_id.is_empty()
            || task_scope.key_prefix.is_empty()
        {
            return Err(BackupServiceError::ManifestInvalid(format!(
                "Personal Task Scope metadata mismatch for user {}",
                entry.user_id
            )));
        }
        let matches = conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM task_scopes
                 WHERE id = ?1
                   AND kind = ?2
                   AND owner_runtime_user_id = ?3
                   AND key_prefix = ?4
                   AND status = 'active'
             )",
            [
                task_scope.task_scope_id.as_str(),
                TaskScopeKind::Personal.as_str(),
                entry.user_id.as_str(),
                task_scope.key_prefix.as_str(),
            ],
            |row| row.get::<_, bool>(0),
        );
        match matches {
            Ok(true) => {}
            Ok(false) => {
                return Err(BackupServiceError::ManifestInvalid(format!(
                    "Personal Task Scope metadata mismatch for user {}",
                    entry.user_id
                )));
            }
            Err(err) => {
                return Err(BackupServiceError::ManifestInvalid(format!(
                    "failed to verify Personal Task Scope metadata for user {}: {err}",
                    entry.user_id
                )));
            }
        }
    }

    Ok(())
}

fn should_defer_to_restore_apply(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(error, _)
            if matches!(error.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

fn verify_snapshot_files(
    snapshot_dir: &Path,
    manifest: &BackupManifest,
) -> Result<(), BackupServiceError> {
    let mut verified = HashSet::new();
    verify_file_entry_once(snapshot_dir, &manifest.contents.config_db, &mut verified)?;
    for entry in &manifest.contents.replicas {
        verify_alias_pair(
            "replica_db",
            &entry.replica_db,
            "canonical_task_db",
            &entry.canonical_task_db,
        )?;
        verify_alias_pair(
            "sync_db",
            &entry.sync_db,
            "merged_sync_db",
            &entry.merged_sync_db,
        )?;
        if let Some(replica_db) = &entry.replica_db {
            verify_file_entry_once(snapshot_dir, replica_db, &mut verified)?;
        }
        if let Some(canonical_task_db) = &entry.canonical_task_db {
            verify_file_entry_once(snapshot_dir, canonical_task_db, &mut verified)?;
        }
        if let Some(sync_db) = &entry.sync_db {
            verify_file_entry_once(snapshot_dir, sync_db, &mut verified)?;
        }
        if let Some(merged_sync_db) = &entry.merged_sync_db {
            verify_file_entry_once(snapshot_dir, merged_sync_db, &mut verified)?;
        }
    }
    Ok(())
}

fn verify_alias_pair(
    legacy_name: &str,
    legacy: &Option<BackupFileEntry>,
    task_scope_name: &str,
    task_scope: &Option<BackupFileEntry>,
) -> Result<(), BackupServiceError> {
    let (Some(legacy), Some(task_scope)) = (legacy, task_scope) else {
        return Ok(());
    };
    if legacy.file != task_scope.file
        || legacy.sha256 != task_scope.sha256
        || legacy.size_bytes != task_scope.size_bytes
    {
        return Err(BackupServiceError::ManifestInvalid(format!(
            "backup manifest aliases {legacy_name} and {task_scope_name} disagree"
        )));
    }
    Ok(())
}

fn verify_file_entry_once(
    snapshot_dir: &Path,
    entry: &BackupFileEntry,
    verified: &mut HashSet<String>,
) -> Result<(), BackupServiceError> {
    if verified.insert(entry.file.clone()) {
        verify_file_entry(snapshot_dir, entry)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite_error(code: ErrorCode) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code: code as i32,
            },
            None,
        )
    }

    #[test]
    fn task_scope_validation_defer_classifier_only_allows_structural_db_failures() {
        assert!(should_defer_to_restore_apply(&sqlite_error(
            ErrorCode::DatabaseCorrupt
        )));
        assert!(should_defer_to_restore_apply(&sqlite_error(
            ErrorCode::NotADatabase
        )));
        assert!(!should_defer_to_restore_apply(&sqlite_error(
            ErrorCode::DatabaseBusy
        )));
        assert!(!should_defer_to_restore_apply(&sqlite_error(
            ErrorCode::PermissionDenied
        )));
        assert!(!should_defer_to_restore_apply(&sqlite_error(
            ErrorCode::SystemIoFailure
        )));
    }
}
