use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::admin::handlers::{require_existing_user, validate_user_id};
use crate::admin::services::recovery::RecoveryCoordinator;
use crate::app_state::AppState;
use crate::auth::OperatorAuth;

#[derive(Serialize, ToSchema)]
#[schema(example = json!({
    "success": true,
    "message": "User 86a9cca3-5689-41e4-8361-8075c9c49b38 brought back online"
}))]
pub struct AdminActionResponse {
    pub success: bool,
    pub message: String,
}

#[utoipa::path(
    post,
    path = "/admin/user/{user_id}/evict",
    operation_id = "evictAdminUserRuntimeState",
    params(("user_id" = String, Path, description = "User ID")),
    responses(
        (status = 200, description = "Replica and sync runtime state evicted", body = AdminActionResponse),
        (status = 400, description = "Invalid user ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User not found"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn evict_replica(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path(user_id): Path<String>,
) -> Result<Json<AdminActionResponse>, StatusCode> {
    validate_user_id(&user_id)?;
    require_existing_user(&state, &user_id).await?;
    let replica_evicted = state.replica_manager.evict(&user_id);
    let sync_evicted = state.sync_storage_manager.evict_user(&user_id);
    state.runtime_sync.clear_user(&user_id);
    crate::sync_bridge::evict_cryptor(&user_id);
    if replica_evicted || sync_evicted {
        tracing::info!(
            "Admin: evicted replica={replica_evicted} sync={sync_evicted} for user {user_id}"
        );
        Ok(Json(AdminActionResponse {
            success: true,
            message: format!(
                "Evicted cached connections for user {user_id} (replica={replica_evicted}, sync={sync_evicted}). Next request will reopen."
            ),
        }))
    } else {
        Ok(Json(AdminActionResponse {
            success: true,
            message: format!("User {user_id} was not in cache (no action needed)."),
        }))
    }
}

#[utoipa::path(
    post,
    path = "/admin/user/{user_id}/offline",
    operation_id = "takeAdminUserOffline",
    params(("user_id" = String, Path, description = "User ID")),
    responses(
        (status = 200, description = "User quarantined", body = AdminActionResponse),
        (status = 400, description = "Invalid user ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User not found"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn quarantine_user(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path(user_id): Path<String>,
) -> Result<Json<AdminActionResponse>, StatusCode> {
    validate_user_id(&user_id)?;
    require_existing_user(&state, &user_id).await?;
    let recovery = RecoveryCoordinator::for_running_state(&state);
    recovery.take_user_offline(&user_id, "api", "operator", None);
    Ok(Json(AdminActionResponse {
        success: true,
        message: format!("User {user_id} quarantined — all requests will return 503"),
    }))
}

#[utoipa::path(
    post,
    path = "/admin/user/{user_id}/online",
    operation_id = "bringAdminUserOnline",
    params(("user_id" = String, Path, description = "User ID")),
    responses(
        (status = 200, description = "User brought online or already online", body = AdminActionResponse),
        (status = 400, description = "Invalid user ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User not found"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn unquarantine_user(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path(user_id): Path<String>,
) -> Result<Json<AdminActionResponse>, StatusCode> {
    validate_user_id(&user_id)?;
    require_existing_user(&state, &user_id).await?;
    let recovery = RecoveryCoordinator::for_running_state(&state);
    let was_quarantined = recovery.bring_user_online(&user_id, "api", "operator");
    Ok(Json(AdminActionResponse {
        success: true,
        message: if was_quarantined {
            format!("User {user_id} brought back online")
        } else {
            format!("User {user_id} was not quarantined")
        },
    }))
}

#[utoipa::path(
    post,
    path = "/admin/user/{user_id}/checkpoint",
    operation_id = "checkpointAdminUserReplica",
    params(("user_id" = String, Path, description = "User ID")),
    responses(
        (status = 200, description = "WAL checkpoint attempted", body = AdminActionResponse),
        (status = 400, description = "Invalid user ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User not found"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn checkpoint_replica(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path(user_id): Path<String>,
) -> Result<Json<AdminActionResponse>, StatusCode> {
    validate_user_id(&user_id)?;
    require_existing_user(&state, &user_id).await?;
    let replica_dir = state.user_replica_dir(&user_id);
    let db_path = replica_dir.join("taskchampion.sqlite3");

    if !db_path.exists() {
        return Ok(Json(AdminActionResponse {
            success: false,
            message: format!("Replica not found for user {user_id}"),
        }));
    }

    let result = tokio::task::spawn_blocking(move || {
        let conn = rusqlite::Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok::<_, rusqlite::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            tracing::info!("Admin: WAL checkpoint for user {user_id}");
            Ok(Json(AdminActionResponse {
                success: true,
                message: format!("WAL checkpoint completed for user {user_id}"),
            }))
        }
        _ => {
            tracing::error!("Admin: WAL checkpoint failed for user {user_id}");
            Ok(Json(AdminActionResponse {
                success: false,
                message: format!("WAL checkpoint failed for user {user_id}"),
            }))
        }
    }
}
