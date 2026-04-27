use axum::{
    extract::{Path, State},
    http::HeaderMap,
    http::StatusCode,
    Json,
};
use uuid::Uuid;

use crate::admin::handlers::validate_user_id;
use crate::admin::users::DeleteUserResponse;
use crate::app_state::AppState;
use crate::auth::OperatorAuth;
use crate::runtime_policy::{runtime_delete_message, RuntimeDeleteDecision, RuntimePolicyService};

#[utoipa::path(
    delete,
    path = "/admin/user/{user_id}",
    operation_id = "deleteAdminUser",
    params(("user_id" = String, Path, description = "User ID")),
    responses(
        (status = 200, description = "User deleted", body = DeleteUserResponse),
        (status = 400, description = "Invalid user ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 403, description = "Runtime policy forbids deletion"),
        (status = 404, description = "User not found"),
        (status = 500, description = "Deletion failed"),
        (status = 503, description = "Runtime policy is stale or not applied"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn delete_user(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Result<Json<DeleteUserResponse>, (StatusCode, String)> {
    validate_user_id(&user_id).map_err(|status| (status, "Invalid user ID".to_string()))?;

    let user = state
        .store
        .get_user_by_id(&user_id)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load user".to_string(),
            )
        })?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;

    enforce_delete_policy(&state, &user.id).await?;

    let devices = state.store.list_devices(&user.id).await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to list devices".to_string(),
        )
    })?;
    let device_count_removed = devices.len();

    let replica_dir = state.user_replica_dir(&user.id);
    let replica_dir_exists = replica_dir.exists();
    if replica_dir_exists {
        state.mark_user_offline(&user.id);
    } else {
        evict_user_runtime_state(&state, &user.id);
    }

    let staged_replica_dir = stage_replica_dir_for_delete(&state, &user.id, &replica_dir)?;
    let deleted = match state.store.delete_user(&user.id).await {
        Ok(deleted) => deleted,
        Err(err) => {
            rollback_delete_staging(&state, &user.id, staged_replica_dir.as_ref(), &replica_dir);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to delete user from config store: {err}"),
            ));
        }
    };

    if !deleted {
        rollback_delete_staging(&state, &user.id, staged_replica_dir.as_ref(), &replica_dir);
        return Err((StatusCode::NOT_FOUND, "User not found".to_string()));
    }

    if let Some(staged) = staged_replica_dir.as_ref() {
        if let Err(err) = std::fs::remove_dir_all(staged) {
            state.clear_user_quarantine(&user.id);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "User deleted from config store, but failed to remove staged replica directory {}: {err}",
                    staged.display()
                ),
            ));
        }
    }

    state.clear_user_quarantine(&user.id);

    tracing::info!(
        target: "audit",
        action = "user.delete",
        source = "api",
        client_ip = %crate::audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
        user_id = %user.id,
        username = %user.username,
        device_count_removed = device_count_removed,
        replica_dir_removed = replica_dir_exists,
    );

    Ok(Json(DeleteUserResponse {
        user_id: user.id,
        username: user.username,
        device_count_removed,
        replica_dir_removed: replica_dir_exists,
    }))
}

async fn enforce_delete_policy(
    state: &AppState,
    user_id: &str,
) -> Result<(), (StatusCode, String)> {
    let policy_service = RuntimePolicyService::new(state.store.clone());
    match policy_service
        .runtime_delete_for_user(user_id)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load runtime policy".to_string(),
            )
        })? {
        RuntimeDeleteDecision::Allow => Ok(()),
        RuntimeDeleteDecision::Forbidden => Err((
            StatusCode::FORBIDDEN,
            runtime_delete_message(RuntimeDeleteDecision::Forbidden)
                .unwrap_or("User deletion forbidden")
                .to_string(),
        )),
        RuntimeDeleteDecision::NotCurrent => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            runtime_delete_message(RuntimeDeleteDecision::NotCurrent)
                .unwrap_or("Runtime policy is stale or not applied")
                .to_string(),
        )),
    }
}

fn evict_user_runtime_state(state: &AppState, user_id: &str) {
    state.replica_manager.evict(user_id);
    state.sync_storage_manager.evict_user(user_id);
    state.runtime_sync.clear_user(user_id);
    crate::sync_bridge::evict_cryptor(user_id);
}

fn stage_replica_dir_for_delete(
    state: &AppState,
    user_id: &str,
    replica_dir: &std::path::Path,
) -> Result<Option<std::path::PathBuf>, (StatusCode, String)> {
    if !replica_dir.exists() {
        return Ok(None);
    }

    let staging_root = state.data_dir.join(".delete-staging");
    std::fs::create_dir_all(&staging_root).map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to prepare delete staging directory: {err}"),
        )
    })?;
    let staged = staging_root.join(format!("{}-{}", user_id, Uuid::new_v4()));
    std::fs::rename(replica_dir, &staged).map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to stage replica directory for deletion: {err}"),
        )
    })?;
    Ok(Some(staged))
}

fn rollback_delete_staging(
    state: &AppState,
    user_id: &str,
    staged_replica_dir: Option<&std::path::PathBuf>,
    replica_dir: &std::path::Path,
) {
    if let Some(staged) = staged_replica_dir {
        let _ = std::fs::rename(staged, replica_dir);
    }
    state.clear_user_quarantine(user_id);
}
