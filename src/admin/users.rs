use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::admin::openapi::sqlite_utc_to_rfc3339;
use crate::app_state::AppState;
use crate::auth::OperatorAuth;
use crate::recovery::UserRecoveryAssessment;

#[derive(Serialize, ToSchema)]
#[schema(example = json!([{
    "id": "86a9cca3-5689-41e4-8361-8075c9c49b38",
    "username": "simon",
    "createdAt": "2026-04-02T10:00:00+00:00",
    "deviceCount": 2,
    "lastSyncAt": "2026-04-02T10:05:00+00:00"
}]))]
#[serde(rename_all = "camelCase")]
pub struct AdminUserSummary {
    #[schema(format = "uuid", example = "86a9cca3-5689-41e4-8361-8075c9c49b38")]
    pub id: String,
    pub username: String,
    #[schema(format = "date-time", example = "2026-04-02T10:00:00+00:00")]
    pub created_at: String,
    pub device_count: usize,
    #[schema(format = "date-time", example = "2026-04-02T10:05:00+00:00")]
    pub last_sync_at: Option<String>,
}

/// Per-user diagnostic information.
#[derive(Serialize, ToSchema)]
#[schema(example = json!({
    "userId": "86a9cca3-5689-41e4-8361-8075c9c49b38",
    "replicaCached": false,
    "taskCount": null,
    "pendingCount": null,
    "replicaDirExists": true,
    "replicaDirSizeBytes": 16384,
    "quarantined": false,
    "recoveryAssessment": {
        "userId": "86a9cca3-5689-41e4-8361-8075c9c49b38",
        "status": "healthy",
        "userDirExists": true,
        "canonicalReplicaExists": true,
        "syncIdentityExists": true,
        "sharedSyncDbExists": true,
        "sharedSyncSchemaVersion": 2,
        "expectedSyncSchemaVersion": 2,
        "sharedSyncUpgradeNeeded": false,
        "deviceCount": 1,
        "activeDeviceCount": 1,
        "missingDeviceSecrets": [],
        "sharedSyncDbError": null,
        "notes": []
    },
    "integrityCheck": {
        "replica": "ok",
        "sync": ["sync.sqlite: ok"]
    }
}))]
pub struct UserStats {
    pub user_id: String,
    pub replica_cached: bool,
    pub task_count: Option<usize>,
    pub pending_count: Option<usize>,
    pub replica_dir_exists: bool,
    pub replica_dir_size_bytes: Option<u64>,
    pub quarantined: bool,
    pub recovery_assessment: UserRecoveryAssessment,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity_check: Option<IntegrityResult>,
}

#[derive(Serialize, ToSchema)]
#[schema(example = json!({
    "replica": "ok",
    "sync": ["sync.sqlite: ok"]
}))]
pub struct IntegrityResult {
    pub replica: Option<String>,
    pub sync: Vec<String>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IntegrityMode {
    Quick,
    Full,
}

#[derive(Deserialize)]
pub struct UserStatsQuery {
    pub integrity: Option<IntegrityMode>,
}

#[derive(Serialize, ToSchema)]
#[schema(example = json!({
    "userId": "86a9cca3-5689-41e4-8361-8075c9c49b38",
    "username": "simon",
    "deviceCountRemoved": 2,
    "replicaDirRemoved": true
}))]
#[serde(rename_all = "camelCase")]
pub struct DeleteUserResponse {
    #[schema(format = "uuid", example = "86a9cca3-5689-41e4-8361-8075c9c49b38")]
    pub user_id: String,
    pub username: String,
    pub device_count_removed: usize,
    pub replica_dir_removed: bool,
}

#[utoipa::path(
    get,
    path = "/admin/users",
    operation_id = "listAdminUsers",
    responses(
        (status = 200, description = "Operator user list", body = Vec<AdminUserSummary>),
        (status = 401, description = "Invalid operator token"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn list_users(
    State(state): State<AppState>,
    _auth: OperatorAuth,
) -> Result<Json<Vec<AdminUserSummary>>, StatusCode> {
    let users = state
        .store
        .list_users()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut response = Vec::with_capacity(users.len());
    for user in users {
        let devices = state
            .store
            .list_devices(&user.id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let last_sync_at = devices
            .iter()
            .filter_map(|device| device.last_sync_at.as_deref())
            .max()
            .map(sqlite_utc_to_rfc3339);
        response.push(AdminUserSummary {
            id: user.id,
            username: user.username,
            created_at: sqlite_utc_to_rfc3339(&user.created_at),
            device_count: devices.len(),
            last_sync_at,
        });
    }

    Ok(Json(response))
}
