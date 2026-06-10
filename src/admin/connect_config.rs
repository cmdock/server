use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::admin::services::connect_config::{
    self as service, ConnectConfigService, IssueRequest, IssueSource,
};
use crate::app_state::AppState;
use crate::auth::OperatorAuth;

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
        })?
        .to_string();

    let svc = ConnectConfigService::new(state.store.clone());
    let outcome = svc
        .issue(IssueRequest {
            user_id,
            display_name: body.name,
            server_url,
            ttl_minutes: None,
            source: IssueSource::Http {
                client_ip: service::http_client_ip(
                    &headers,
                    state.config.server.trust_forwarded_headers,
                ),
                request_id: service::http_request_id(&headers),
            },
        })
        .await
        .map_err(|err| {
            tracing::error!("Failed to issue connect-config credential: {err}");
            // 412 PRECONDITION_FAILED for any user-facing URL validation
            // failure (empty, invalid, non-https, missing host, credentials,
            // path/query/fragment present, or unconfigured public URL on the
            // server). All such messages share the "connect-config server
            // URL" prefix from `normalize_connect_server_url`. Pre-#123 the
            // HTTP path validated the URL inline and returned 412 for any
            // of these; we preserve that behaviour through the service
            // boundary by matching on the shared prefix. Anything else is
            // a genuine internal error → 500. (#123 codex iter1)
            let chain = err.chain().map(|e| e.to_string()).collect::<Vec<_>>();
            let is_url_validation = chain
                .iter()
                .any(|m| m.contains("connect-config server URL"));
            if is_url_validation {
                (StatusCode::PRECONDITION_FAILED, err.to_string())
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal error".to_string(),
                )
            }
        })?;

    Ok(Json(CreateConnectConfigResponse {
        credential: outcome.credential,
        token_id: outcome.token_id,
        server_url: outcome.server_url,
    }))
}
