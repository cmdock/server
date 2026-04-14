use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::admin::handlers::{require_existing_user, validate_user_id};
use crate::admin::openapi::sqlite_utc_to_rfc3339;
use crate::admin::services::sync_identity::SyncIdentityService;
use crate::app_state::AppState;
use crate::audit;
use crate::auth::OperatorAuth;
use crate::store::models::ReplicaRecord;

#[derive(Debug, Serialize, ToSchema)]
#[schema(example = json!({
    "userId": "86a9cca3-5689-41e4-8361-8075c9c49b38",
    "clientId": "2658d2e4-8c97-4128-8053-eab3cffe7241",
    "label": "Canonical Sync Identity",
    "createdAt": "2026-04-02T10:00:00+00:00"
}))]
#[serde(rename_all = "camelCase")]
pub struct OperatorSyncIdentityResponse {
    #[schema(format = "uuid", example = "86a9cca3-5689-41e4-8361-8075c9c49b38")]
    pub user_id: String,
    #[schema(format = "uuid", example = "2658d2e4-8c97-4128-8053-eab3cffe7241")]
    pub client_id: String,
    pub label: String,
    #[schema(format = "date-time", example = "2026-04-02T10:00:00+00:00")]
    pub created_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[schema(example = json!({
    "userId": "86a9cca3-5689-41e4-8361-8075c9c49b38",
    "clientId": "2658d2e4-8c97-4128-8053-eab3cffe7241",
    "label": "Canonical Sync Identity",
    "createdAt": "2026-04-02T10:00:00+00:00",
    "created": true
}))]
#[serde(rename_all = "camelCase")]
pub struct EnsureOperatorSyncIdentityResponse {
    #[schema(format = "uuid", example = "86a9cca3-5689-41e4-8361-8075c9c49b38")]
    pub user_id: String,
    #[schema(format = "uuid", example = "2658d2e4-8c97-4128-8053-eab3cffe7241")]
    pub client_id: String,
    pub label: String,
    #[schema(format = "date-time", example = "2026-04-02T10:00:00+00:00")]
    pub created_at: String,
    pub created: bool,
}

#[utoipa::path(
    get,
    path = "/admin/user/{user_id}/sync-identity",
    operation_id = "getOperatorSyncIdentity",
    params(("user_id" = String, Path, description = "User ID")),
    responses(
        (status = 200, description = "Canonical sync identity", body = OperatorSyncIdentityResponse),
        (status = 400, description = "Invalid user ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User or canonical sync identity not found"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn get_sync_identity(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path(user_id): Path<String>,
) -> Result<Json<OperatorSyncIdentityResponse>, StatusCode> {
    validate_user_id(&user_id)?;
    require_existing_user(&state, &user_id).await?;

    let service = SyncIdentityService::new(state.store.clone());
    let replica = service
        .get_for_user(&user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(sync_identity_response(&user_id, replica)))
}

#[utoipa::path(
    post,
    path = "/admin/user/{user_id}/sync-identity/ensure",
    operation_id = "ensureOperatorSyncIdentity",
    params(("user_id" = String, Path, description = "User ID")),
    responses(
        (status = 200, description = "Canonical sync identity ensured", body = EnsureOperatorSyncIdentityResponse),
        (status = 400, description = "Invalid user ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User not found"),
        (status = 412, description = "Server bootstrap prerequisites are not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn ensure_sync_identity(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Result<Json<EnsureOperatorSyncIdentityResponse>, (StatusCode, String)> {
    validate_user_id(&user_id).map_err(|status| (status, "Invalid user ID".to_string()))?;
    require_existing_user(&state, &user_id)
        .await
        .map_err(|status| {
            let message = match status {
                StatusCode::NOT_FOUND => "User not found",
                _ => "Internal error",
            };
            (status, message.to_string())
        })?;

    let service = SyncIdentityService::new(state.store.clone());
    let master_key = state.config.master_key.ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            "Server has no master key configured (CMDOCK_MASTER_KEY)".to_string(),
        )
    })?;
    let (replica, created) = service
        .ensure_record_for_user(&user_id, master_key)
        .await
        .map_err(map_ensure_error)?;

    if created {
        tracing::info!(
            target: "audit",
            action = "replica.create",
            source = "api",
            client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
            user_id = %user_id,
            client_id = %replica.id,
        );
    }

    Ok(Json(EnsureOperatorSyncIdentityResponse {
        user_id,
        client_id: replica.id,
        label: replica.label,
        created_at: sqlite_utc_to_rfc3339(&replica.created_at),
        created,
    }))
}

fn map_ensure_error(err: crate::sync_identity::SyncIdentityError) -> (StatusCode, String) {
    match err {
        crate::sync_identity::SyncIdentityError::AlreadyExists(_)
        | crate::sync_identity::SyncIdentityError::MissingIdentity(_) => {
            tracing::error!("Unexpected sync identity ensure state: {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error".to_string(),
            )
        }
        crate::sync_identity::SyncIdentityError::Internal(inner) => {
            tracing::error!("Failed to ensure sync identity: {inner}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error".to_string(),
            )
        }
    }
}

fn sync_identity_response(user_id: &str, replica: ReplicaRecord) -> OperatorSyncIdentityResponse {
    OperatorSyncIdentityResponse {
        user_id: user_id.to_string(),
        client_id: replica.id,
        label: replica.label,
        created_at: sqlite_utc_to_rfc3339(&replica.created_at),
    }
}
