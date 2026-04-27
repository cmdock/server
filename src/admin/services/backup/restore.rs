use std::collections::HashSet;
use std::fs;
use std::path::Path;

use uuid::Uuid;

use crate::app_state::AppState;

use super::{
    manifest::resolve_snapshot_path, snapshot::copy_file, BackupManifest, BackupServiceError,
};

pub(super) async fn apply_snapshot_to_live(
    state: &AppState,
    snapshot_dir: &Path,
    manifest: &BackupManifest,
) -> Result<(), BackupServiceError> {
    let staging_root = state
        .data_dir
        .join(format!(".restore-stage-{}", Uuid::new_v4()));
    fs::create_dir_all(&staging_root).map_err(|err| {
        BackupServiceError::Internal(format!("failed to create restore staging dir: {err}"))
    })?;

    let config_src = resolve_snapshot_path(snapshot_dir, &manifest.contents.config_db.file)?;
    copy_file(&config_src, &staging_root.join("config.sqlite"))?;
    stage_user_files(snapshot_dir, manifest, &staging_root)?;

    let live_users_dir = state.data_dir.join("users");
    let old_users_dir = state
        .data_dir
        .join(format!(".restore-users-prev-{}", Uuid::new_v4()));
    let staged_users_dir = staging_root.join("users");

    let old_users_present = if live_users_dir.exists() {
        fs::rename(&live_users_dir, &old_users_dir).map_err(|err| {
            BackupServiceError::Internal(format!("failed to stage current users dir: {err}"))
        })?;
        true
    } else {
        false
    };

    if staged_users_dir.exists() {
        if let Err(err) = fs::rename(&staged_users_dir, &live_users_dir) {
            if old_users_present {
                let _ = fs::rename(&old_users_dir, &live_users_dir);
            }
            return Err(BackupServiceError::Internal(format!(
                "failed to swap staged users dir into place: {err}"
            )));
        }
    } else {
        fs::create_dir_all(&live_users_dir).map_err(|err| {
            BackupServiceError::Internal(format!("failed to create live users dir: {err}"))
        })?;
    }

    let config_restore_result = async {
        state
            .store
            .restore_from_path(&staging_root.join("config.sqlite"))
            .await
            .map_err(|err| {
                BackupServiceError::Internal(format!("failed to restore config DB: {err}"))
            })?;
        state.store.run_migrations().await.map_err(|err| {
            BackupServiceError::Internal(format!("failed to run migrations after restore: {err}"))
        })?;
        Ok::<(), BackupServiceError>(())
    }
    .await;

    if let Err(err) = config_restore_result {
        let _ = fs::remove_dir_all(&live_users_dir);
        if old_users_present {
            let _ = fs::rename(&old_users_dir, &live_users_dir);
        }
        let _ = fs::remove_dir_all(&staging_root);
        return Err(err);
    }

    if let Some(secrets) = manifest.secrets.as_ref() {
        state.set_operator_token(secrets.admin_token.clone());
    }

    if old_users_present {
        let _ = fs::remove_dir_all(&old_users_dir);
    }
    let _ = fs::remove_dir_all(&staging_root);
    Ok(())
}

fn stage_user_files(
    snapshot_dir: &Path,
    manifest: &BackupManifest,
    staging_root: &Path,
) -> Result<(), BackupServiceError> {
    for entry in &manifest.contents.replicas {
        if let Some(replica_db) = &entry.replica_db {
            let src = resolve_snapshot_path(snapshot_dir, &replica_db.file)?;
            let dst = staging_root
                .join("users")
                .join(&entry.user_id)
                .join("taskchampion.sqlite3");
            copy_file(&src, &dst)?;
        }
        if let Some(sync_db) = &entry.sync_db {
            let src = resolve_snapshot_path(snapshot_dir, &sync_db.file)?;
            let dst = staging_root
                .join("users")
                .join(&entry.user_id)
                .join("sync.sqlite");
            copy_file(&src, &dst)?;
        }
    }
    Ok(())
}

pub(super) async fn collect_quarantined_users(
    state: &AppState,
) -> Result<HashSet<String>, BackupServiceError> {
    let users = state
        .store
        .list_users()
        .await
        .map_err(|err| BackupServiceError::Internal(format!("failed to list users: {err}")))?;
    Ok(users
        .into_iter()
        .filter(|user| state.is_user_quarantined(&user.id))
        .map(|user| user.id)
        .collect())
}

pub(super) async fn quarantine_all_users(state: &AppState) -> Result<(), BackupServiceError> {
    let users = state
        .store
        .list_users()
        .await
        .map_err(|err| BackupServiceError::Internal(format!("failed to list users: {err}")))?;
    for user in users {
        state.mark_user_offline(&user.id);
    }
    Ok(())
}

pub(super) async fn restore_quarantine_state(
    state: &AppState,
    previous: &HashSet<String>,
) -> Result<(), BackupServiceError> {
    let users = state
        .store
        .list_users()
        .await
        .map_err(|err| BackupServiceError::Internal(format!("failed to list users: {err}")))?;
    for user in users {
        if previous.contains(&user.id) {
            state.mark_user_offline(&user.id);
        } else {
            state.clear_user_quarantine(&user.id);
        }
    }
    state.sync_offline_markers_now();
    Ok(())
}
