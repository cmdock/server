use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::admin::services::backup::{
    self, BackupServiceError, BackupSnapshotSummary, RestoreSummary, RestoredReplicaSummary,
};
use crate::app_state::AppState;
use crate::auth::OperatorAuth;

#[derive(Debug, Deserialize, ToSchema)]
pub struct BackupCreateQuery {
    pub include_secrets: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupRestoreRequest {
    pub timestamp: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupCreateResponse {
    pub timestamp: String,
    pub path: String,
    pub users: usize,
    pub total_size_bytes: u64,
    pub secrets_included: bool,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupSummaryResponse {
    pub timestamp: String,
    pub server_version: String,
    pub users: usize,
    pub task_count: Option<u64>,
    pub total_size_bytes: u64,
    pub secrets_included: bool,
    pub backup_type: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupListResponse {
    pub backups: Vec<BackupSummaryResponse>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupRestoreResponse {
    pub restored_from: String,
    pub pre_restore_snapshot: String,
    pub users_restored: usize,
    pub replicas_restored: usize,
    pub secrets_restored: bool,
    pub config_database_restored: bool,
    pub replicas: Vec<BackupRestoreReplicaResponse>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupRestoreReplicaResponse {
    pub user_id: String,
    pub username: String,
    pub task_count: Option<u64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackupErrorResponse {
    pub code: String,
    pub message: String,
}

#[utoipa::path(
    post,
    path = "/admin/backup",
    params(
        ("include_secrets" = Option<bool>, Query, description = "Include server-owned secrets in the snapshot")
    ),
    responses(
        (status = 201, description = "Backup created", body = BackupCreateResponse),
        (status = 409, description = "Backup in progress", body = BackupErrorResponse),
        (status = 500, description = "Backup failed", body = BackupErrorResponse),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn create_backup(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Query(query): Query<BackupCreateQuery>,
) -> Result<(StatusCode, Json<BackupCreateResponse>), (StatusCode, Json<BackupErrorResponse>)> {
    let created = backup::create_backup(&state, query.include_secrets.unwrap_or(false))
        .await
        .map_err(map_backup_error)?;
    Ok((
        StatusCode::CREATED,
        Json(BackupCreateResponse {
            timestamp: created.timestamp,
            path: created.path,
            users: created.users,
            total_size_bytes: created.total_size_bytes,
            secrets_included: created.secrets_included,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/admin/backup/list",
    responses(
        (status = 200, description = "Known backup snapshots", body = BackupListResponse),
        (status = 500, description = "List failed", body = BackupErrorResponse),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn list_backups(
    State(state): State<AppState>,
    _auth: OperatorAuth,
) -> Result<Json<BackupListResponse>, (StatusCode, Json<BackupErrorResponse>)> {
    let backups = backup::list_backups(&state)
        .await
        .map_err(map_backup_error)?;
    Ok(Json(BackupListResponse {
        backups: backups.into_iter().map(map_snapshot_summary).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/admin/backup/restore",
    request_body = BackupRestoreRequest,
    responses(
        (status = 200, description = "Backup restored", body = BackupRestoreResponse),
        (status = 404, description = "Snapshot not found", body = BackupErrorResponse),
        (status = 409, description = "Restore rejected", body = BackupErrorResponse),
        (status = 422, description = "Manifest or checksum validation failed", body = BackupErrorResponse),
        (status = 500, description = "Restore failed after rollback", body = BackupErrorResponse),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn restore_backup(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Json(body): Json<BackupRestoreRequest>,
) -> Result<Json<BackupRestoreResponse>, (StatusCode, Json<BackupErrorResponse>)> {
    let restored = backup::restore_backup(&state, &body.timestamp)
        .await
        .map_err(map_backup_error)?;
    Ok(Json(map_restore_summary(restored)))
}

fn map_snapshot_summary(summary: BackupSnapshotSummary) -> BackupSummaryResponse {
    BackupSummaryResponse {
        timestamp: summary.timestamp,
        server_version: summary.server_version,
        users: summary.users,
        task_count: summary.task_count,
        total_size_bytes: summary.total_size_bytes,
        secrets_included: summary.secrets_included,
        backup_type: summary.backup_type,
    }
}

fn map_restore_summary(summary: RestoreSummary) -> BackupRestoreResponse {
    BackupRestoreResponse {
        restored_from: summary.restored_from,
        pre_restore_snapshot: summary.pre_restore_snapshot,
        users_restored: summary.users_restored,
        replicas_restored: summary.replicas_restored,
        secrets_restored: summary.secrets_restored,
        config_database_restored: summary.config_database_restored,
        replicas: summary
            .replicas
            .into_iter()
            .map(map_restored_replica_summary)
            .collect(),
    }
}

fn map_restored_replica_summary(summary: RestoredReplicaSummary) -> BackupRestoreReplicaResponse {
    BackupRestoreReplicaResponse {
        user_id: summary.user_id,
        username: summary.username,
        task_count: summary.task_count,
    }
}

fn map_backup_error(err: BackupServiceError) -> (StatusCode, Json<BackupErrorResponse>) {
    let (status, code) = match &err {
        BackupServiceError::BackupDirNotConfigured => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "BACKUP_DIR_NOT_CONFIGURED",
        ),
        BackupServiceError::BackupDirNotWritable(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "BACKUP_DIR_NOT_WRITABLE")
        }
        BackupServiceError::BackupInProgress => (StatusCode::CONFLICT, "BACKUP_IN_PROGRESS"),
        BackupServiceError::CheckpointFailed(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "CHECKPOINT_FAILED")
        }
        BackupServiceError::SnapshotNotFound => (StatusCode::NOT_FOUND, "SNAPSHOT_NOT_FOUND"),
        BackupServiceError::ManifestMissing => {
            (StatusCode::UNPROCESSABLE_ENTITY, "MANIFEST_MISSING")
        }
        BackupServiceError::ManifestInvalid(_) => {
            (StatusCode::UNPROCESSABLE_ENTITY, "MANIFEST_INVALID")
        }
        BackupServiceError::ChecksumMismatch(_) => {
            (StatusCode::UNPROCESSABLE_ENTITY, "CHECKSUM_MISMATCH")
        }
        BackupServiceError::VersionIncompatible { .. } => {
            (StatusCode::CONFLICT, "VERSION_INCOMPATIBLE")
        }
        BackupServiceError::SchemaIncompatible { .. } => {
            (StatusCode::CONFLICT, "SCHEMA_INCOMPATIBLE")
        }
        BackupServiceError::RestoreInProgress => (StatusCode::CONFLICT, "RESTORE_IN_PROGRESS"),
        BackupServiceError::RestoreFailedRolledBack { .. } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "RESTORE_FAILED_ROLLED_BACK",
        ),
        BackupServiceError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
    };

    (
        status,
        Json(BackupErrorResponse {
            code: code.to_string(),
            message: err.to_string(),
        }),
    )
}
