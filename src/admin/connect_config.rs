use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::app_state::AppState;
use crate::audit;
use crate::auth::OperatorAuth;
use crate::connect_config::{
    normalize_connect_server_url, normalize_optional_display_name, DEFAULT_CONNECT_TOKEN_BYTES,
    DEFAULT_CONNECT_TOKEN_TTL_MINUTES,
};

use super::handlers::{require_existing_user, validate_user_id};

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(example = json!({
    "name": "Simon's iPhone"
}))]
pub struct CreateConnectConfigRequest {
    pub name: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(example = json!({
    "credential": "FYnel6MP4Sd6XO1jPp9FE0YM",
    "tokenId": "cc_a1b2c3d4e5f6",
    "serverUrl": "https://tasks.example.com"
}))]
pub struct CreateConnectConfigResponse {
    pub credential: String,
    pub token_id: String,
    pub server_url: String,
}

#[utoipa::path(
    post,
    path = "/admin/user/{user_id}/connect-config",
    operation_id = "createAdminUserConnectConfig",
    request_body = CreateConnectConfigRequest,
    params(
        ("user_id" = String, Path, description = "User ID")
    ),
    responses(
        (status = 200, description = "Short-lived connect-config credential", body = CreateConnectConfigResponse),
        (status = 400, description = "Invalid user ID or connect-config request"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User not found"),
        (status = 412, description = "Server public base URL is not configured"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn create_connect_config(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(body): Json<CreateConnectConfigRequest>,
) -> Result<Json<CreateConnectConfigResponse>, (StatusCode, String)> {
    validate_user_id(&user_id).map_err(|status| (status, status.to_string()))?;
    require_existing_user(&state, &user_id)
        .await
        .map_err(|status| (status, status.to_string()))?;

    let server_url = state
        .config
        .server
        .public_base_url
        .as_deref()
        .ok_or_else(|| {
            (
                StatusCode::PRECONDITION_FAILED,
                "connect-config server URL is not configured".to_string(),
            )
        })
        .and_then(|raw| {
            normalize_connect_server_url(raw)
                .map_err(|err| (StatusCode::PRECONDITION_FAILED, err.to_string()))
        })?;

    let expires_at = (Utc::now() + Duration::minutes(DEFAULT_CONNECT_TOKEN_TTL_MINUTES))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    let issued = state
        .store
        .create_connect_config_token(&user_id, &expires_at, DEFAULT_CONNECT_TOKEN_BYTES)
        .await
        .map_err(|err| {
            tracing::error!("Failed to create connect-config token: {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error".to_string(),
            )
        })?;

    let display_name = normalize_optional_display_name(body.name.as_deref());

    tracing::info!(
        target: "boundary",
        event = "connect_config.token_issued",
        component = "cmdock/server",
        correlation_id = %issued.token_id,
        credential_hash_prefix = %issued.credential_hash_prefix,
        source = "api",
        user_id = %user_id,
        expires_at = %issued.expires_at,
        request_id = ?audit::request_id(&headers),
    );

    tracing::info!(
        target: "audit",
        action = "connect_config.generate",
        source = "api",
        client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
        user_id = %user_id,
        token_id = %issued.token_id,
        credential_hash_prefix = %issued.credential_hash_prefix,
        expires_at = %issued.expires_at,
        request_id = ?audit::request_id(&headers),
        name = ?display_name,
    );

    Ok(Json(CreateConnectConfigResponse {
        credential: issued.token,
        token_id: issued.token_id,
        server_url,
    }))
}
