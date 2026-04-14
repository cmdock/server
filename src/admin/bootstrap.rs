use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::admin::openapi::BootstrapStatusSchema;
use crate::admin::services::bootstrap::{
    BootstrapError, BootstrapService, BootstrapUserDeviceRequest,
};
use crate::app_state::AppState;
use crate::audit;
use crate::auth::OperatorAuth;

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "username": "staging-smoke-user",
    "createUserIfMissing": true,
    "deviceName": "Hosted iPhone",
    "bootstrapRequestId": "8b6f6952-b9e7-4dd8-8143-b2af7a6ae20d",
    "publicServerUrlOverride": "https://sync.example.com"
}))]
#[serde(rename_all = "camelCase")]
pub struct BootstrapUserDeviceRequestBody {
    #[schema(format = "uuid", example = "86a9cca3-5689-41e4-8361-8075c9c49b38")]
    pub user_id: Option<String>,
    pub username: Option<String>,
    #[serde(default)]
    pub create_user_if_missing: bool,
    pub device_name: String,
    #[schema(format = "uuid", example = "8b6f6952-b9e7-4dd8-8143-b2af7a6ae20d")]
    pub bootstrap_request_id: String,
    pub public_server_url_override: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[schema(example = json!({
    "userId": "86a9cca3-5689-41e4-8361-8075c9c49b38",
    "username": "staging-smoke-user",
    "canonicalClientId": "2658d2e4-8c97-4128-8053-eab3cffe7241",
    "deviceClientId": "6402175f-f9c5-45bd-b6db-2c225a1c2472",
    "encryptionSecret": "1d4d4f77f3f7e303a347d8ce266e46af406387238fdcf688df2f22122617f371",
    "serverUrl": "https://sync.example.com",
    "taskrcLines": [
        "sync.server.url=https://sync.example.com",
        "sync.server.client_id=6402175f-f9c5-45bd-b6db-2c225a1c2472",
        "sync.encryption_secret=1d4d4f77f3f7e303a347d8ce266e46af406387238fdcf688df2f22122617f371"
    ],
    "bootstrapStatus": "pending_delivery",
    "createdUser": true
}))]
#[serde(rename_all = "camelCase")]
pub struct BootstrapUserDeviceResponse {
    #[schema(format = "uuid", example = "86a9cca3-5689-41e4-8361-8075c9c49b38")]
    pub user_id: String,
    pub username: String,
    #[schema(format = "uuid", example = "2658d2e4-8c97-4128-8053-eab3cffe7241")]
    pub canonical_client_id: String,
    #[schema(format = "uuid", example = "6402175f-f9c5-45bd-b6db-2c225a1c2472")]
    pub device_client_id: String,
    pub encryption_secret: String,
    pub server_url: String,
    pub taskrc_lines: Vec<String>,
    #[schema(value_type = BootstrapStatusSchema, example = "pending_delivery")]
    pub bootstrap_status: String,
    pub created_user: bool,
}

#[utoipa::path(
    post,
    path = "/admin/bootstrap/user-device",
    operation_id = "bootstrapUserDevice",
    request_body = BootstrapUserDeviceRequestBody,
    responses(
        (status = 200, description = "User/device bootstrap payload", body = BootstrapUserDeviceResponse),
        (status = 400, description = "Invalid bootstrap request"),
        (status = 403, description = "Runtime policy blocks device provisioning"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User not found"),
        (status = 409, description = "bootstrapRequestId conflicts with an existing request"),
        (status = 412, description = "Server bootstrap prerequisites are not configured"),
        (status = 503, description = "Runtime policy is stale or not applied"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn bootstrap_user_device(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    headers: HeaderMap,
    Json(body): Json<BootstrapUserDeviceRequestBody>,
) -> Result<Json<BootstrapUserDeviceResponse>, (StatusCode, String)> {
    let public_server_url = state
        .config
        .public_server_url(body.public_server_url_override.as_deref())
        .map_err(|err| (StatusCode::PRECONDITION_FAILED, err.to_string()))?;

    let service = BootstrapService::new(state.store.clone(), state.data_dir.clone());
    let result = service
        .bootstrap_user_device(
            BootstrapUserDeviceRequest {
                user_id: body.user_id,
                username: body.username,
                create_user_if_missing: body.create_user_if_missing,
                device_name: body.device_name,
                bootstrap_request_id: body.bootstrap_request_id,
            },
            state.config.master_key,
        )
        .await
        .map_err(map_bootstrap_error)?;

    tracing::info!(
        target: "audit",
        action = "admin.bootstrap.user_device",
        source = "api",
        client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
        user_id = %result.user.id,
        username = %result.user.username,
        device_client_id = %result.device_client_id,
        canonical_client_id = %result.canonical_client_id,
        bootstrap_status = %result.bootstrap_status,
        created_user = result.created_user,
    );

    Ok(Json(BootstrapUserDeviceResponse {
        user_id: result.user.id,
        username: result.user.username,
        canonical_client_id: result.canonical_client_id,
        device_client_id: result.device_client_id.clone(),
        encryption_secret: result.encryption_secret_hex.clone(),
        server_url: public_server_url.clone(),
        taskrc_lines: vec![
            format!("sync.server.url={public_server_url}"),
            format!("sync.server.client_id={}", result.device_client_id),
            format!("sync.encryption_secret={}", result.encryption_secret_hex),
        ],
        bootstrap_status: result.bootstrap_status,
        created_user: result.created_user,
    }))
}

#[utoipa::path(
    post,
    path = "/admin/bootstrap/{bootstrap_request_id}/ack",
    operation_id = "acknowledgeBootstrapRequest",
    params(
        ("bootstrap_request_id" = String, Path, description = "Bootstrap request UUID", format = "uuid", example = "8b6f6952-b9e7-4dd8-8143-b2af7a6ae20d")
    ),
    responses(
        (status = 204, description = "Bootstrap request acknowledged"),
        (status = 400, description = "Invalid bootstrap request ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "Bootstrap request not found"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn acknowledge_bootstrap_request(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    headers: HeaderMap,
    Path(bootstrap_request_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let service = BootstrapService::new(state.store.clone(), state.data_dir.clone());
    let acknowledged = service
        .acknowledge_bootstrap_request(&bootstrap_request_id)
        .await
        .map_err(map_bootstrap_error)?;
    if !acknowledged {
        return Err((
            StatusCode::NOT_FOUND,
            "Bootstrap request not found".to_string(),
        ));
    }

    tracing::info!(
        target: "audit",
        action = "admin.bootstrap.ack",
        source = "api",
        client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
        bootstrap_request_id = %bootstrap_request_id,
    );

    Ok(StatusCode::NO_CONTENT)
}

fn map_bootstrap_error(err: BootstrapError) -> (StatusCode, String) {
    match err {
        BootstrapError::InvalidBootstrapRequestId
        | BootstrapError::InvalidDeviceName
        | BootstrapError::MissingUserSelector
        | BootstrapError::CreateRequiresUsername => (StatusCode::BAD_REQUEST, err.to_string()),
        BootstrapError::UserNotFound => (StatusCode::NOT_FOUND, err.to_string()),
        BootstrapError::BootstrapRequestConflict => (StatusCode::CONFLICT, err.to_string()),
        BootstrapError::MissingMasterKey
        | BootstrapError::Provision(
            crate::devices::service::ProvisionDeviceError::MissingCanonicalSync,
        ) => (StatusCode::PRECONDITION_FAILED, err.to_string()),
        BootstrapError::Provision(crate::devices::service::ProvisionDeviceError::InvalidName) => {
            (StatusCode::BAD_REQUEST, err.to_string())
        }
        BootstrapError::Provision(
            crate::devices::service::ProvisionDeviceError::RuntimePolicyBlocked,
        ) => (StatusCode::FORBIDDEN, err.to_string()),
        BootstrapError::Provision(
            crate::devices::service::ProvisionDeviceError::RuntimePolicyNotCurrent,
        ) => (StatusCode::SERVICE_UNAVAILABLE, err.to_string()),
        BootstrapError::Provision(
            crate::devices::service::ProvisionDeviceError::MissingMasterKey,
        ) => (StatusCode::PRECONDITION_FAILED, err.to_string()),
        BootstrapError::Provision(_) | BootstrapError::Internal(_) => {
            tracing::error!("Bootstrap failed: {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error".to_string(),
            )
        }
    }
}
