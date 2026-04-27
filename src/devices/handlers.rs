use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::app_state::AppState;
use crate::auth::AuthUser;

/// Device as returned by GET /api/devices (no secrets).
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(example = json!({
    "clientId": "c0c173fd-706d-4c9b-aed2-ca6dde18347c",
    "name": "Simon iPhone",
    "registeredAt": "2026-03-31 06:06:09",
    "lastSyncAt": "2026-03-31 12:30:00",
    "lastSyncIp": "203.0.113.42",
    "status": "active"
}))]
pub struct DeviceResponse {
    pub client_id: String,
    pub name: String,
    pub registered_at: String,
    pub last_sync_at: Option<String>,
    pub last_sync_ip: Option<String>,
    pub status: String,
}

/// Response for POST /api/devices — includes secrets (shown once).
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(example = json!({
    "clientId": "c0c173fd-706d-4c9b-aed2-ca6dde18347c",
    "encryptionSecret": "89b5db8c4955726106a0040118b276d54d0d1c9c35b137451ac3b564a177a375",
    "name": "Simon iPhone",
    "taskrcLines": [
        "sync.server.url=https://YOUR_SERVER",
        "sync.server.client_id=c0c173fd-706d-4c9b-aed2-ca6dde18347c",
        "sync.encryption_secret=89b5db8c4955726106a0040118b276d54d0d1c9c35b137451ac3b564a177a375"
    ]
}))]
pub struct RegisterDeviceResponse {
    pub client_id: String,
    pub encryption_secret: String,
    pub name: String,
    /// Ready-to-paste .taskrc lines (convenience for TW CLI users).
    pub taskrc_lines: Vec<String>,
}

/// Request body for POST /api/devices.
#[derive(Debug, Deserialize, ToSchema, Validate)]
#[schema(example = json!({ "name": "Work MacBook" }))]
pub struct RegisterDeviceRequest {
    /// Human-readable device name.
    #[garde(
        length(min = 1, max = 255),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub name: String,
}

/// Request body for PATCH /api/devices/{client_id}.
#[derive(Debug, Deserialize, ToSchema, Validate)]
#[schema(example = json!({ "name": "Work MacBook" }))]
pub struct RenameDeviceRequest {
    #[garde(
        length(min = 1, max = 255),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub name: String,
}

/// List all registered devices for the authenticated user.
#[utoipa::path(
    get,
    path = "/api/devices",
    operation_id = "listDevices",
    responses(
        (status = 200, description = "List of registered devices", body = Vec<DeviceResponse>),
        (status = 401, description = "Unauthorised"),
    ),
    tag = "devices"
)]
pub async fn list_devices(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<DeviceResponse>>, StatusCode> {
    let devices = state.store.list_devices(&auth.user_id).await.map_err(|e| {
        tracing::error!("Failed to list devices: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(
        devices
            .into_iter()
            .map(|d| DeviceResponse {
                client_id: d.client_id,
                name: d.name,
                registered_at: d.registered_at,
                last_sync_at: d.last_sync_at,
                last_sync_ip: d.last_sync_ip,
                status: d.status,
            })
            .collect(),
    ))
}

/// Register a new device for the authenticated user.
///
/// Server generates a unique `client_id` and derives a per-device `encryption_secret`
/// from the user's master secret via HKDF. Both are returned in the response (shown once).
/// The returned values can be pasted into `.taskrc` for Taskwarrior or entered
/// manually into another client such as the iOS app.
#[utoipa::path(
    post,
    path = "/api/devices",
    operation_id = "registerDevice",
    request_body = RegisterDeviceRequest,
    responses(
        (status = 201, description = "Device registered", body = RegisterDeviceResponse),
        (status = 400, description = "Invalid name"),
        (status = 403, description = "Runtime policy blocks device provisioning"),
        (status = 401, description = "Unauthorised"),
        (status = 412, description = "Bootstrap prerequisites missing"),
        (status = 503, description = "Runtime policy is stale or not applied"),
    ),
    tag = "devices"
)]
pub async fn register_device(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<RegisterDeviceRequest>,
) -> Result<(StatusCode, Json<RegisterDeviceResponse>), (StatusCode, String)> {
    crate::validation::validate_or_bad_request_text(&body, "Invalid device name")?;

    let server_url = state
        .config
        .public_server_url(None)
        .map_err(|err| (StatusCode::PRECONDITION_FAILED, err.to_string()))?;

    let provisioned = crate::devices::service::provision_device(
        &*state.store,
        &state.config.server.data_dir,
        &auth.user_id,
        &body.name,
        state.config.master_key,
    )
    .await
    .map_err(|e| match e {
        crate::devices::service::ProvisionDeviceError::InvalidName => {
            (StatusCode::BAD_REQUEST, e.to_string())
        }
        crate::devices::service::ProvisionDeviceError::MissingMasterKey => {
            (StatusCode::BAD_REQUEST, e.to_string())
        }
        crate::devices::service::ProvisionDeviceError::RuntimePolicyBlocked => {
            (StatusCode::FORBIDDEN, e.to_string())
        }
        crate::devices::service::ProvisionDeviceError::RuntimePolicyNotCurrent => {
            (StatusCode::SERVICE_UNAVAILABLE, e.to_string())
        }
        crate::devices::service::ProvisionDeviceError::MissingCanonicalSync => {
            (StatusCode::PRECONDITION_FAILED, e.to_string())
        }
        crate::devices::service::ProvisionDeviceError::MissingStoredSecret => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal error".to_string(),
        ),
        crate::devices::service::ProvisionDeviceError::Internal(inner) => {
            tracing::error!("Failed to register device: {inner}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error".to_string(),
            )
        }
    })?;

    tracing::info!(
        target: "audit",
        action = "device.register",
        source = "api",
        user_id = %auth.user_id,
        client_id = %provisioned.client_id,
        device_name = %provisioned.name,
    );

    Ok((
        StatusCode::CREATED,
        Json(RegisterDeviceResponse {
            client_id: provisioned.client_id.clone(),
            encryption_secret: provisioned.encryption_secret_hex.clone(),
            name: provisioned.name,
            taskrc_lines: vec![
                format!("sync.server.url={server_url}"),
                format!("sync.server.client_id={}", provisioned.client_id),
                format!(
                    "sync.encryption_secret={}",
                    provisioned.encryption_secret_hex
                ),
            ],
        }),
    ))
}

/// Revoke a device (sets status to "revoked"). The device row is preserved for audit.
/// Revocation blocks future sync requests for that `client_id` without affecting
/// the user's other devices.
#[utoipa::path(
    delete,
    path = "/api/devices/{client_id}",
    operation_id = "revokeDevice",
    params(("client_id" = String, Path, description = "Device client_id to revoke")),
    responses(
        (status = 204, description = "Device revoked"),
        (status = 404, description = "Device not found or not owned by user"),
        (status = 401, description = "Unauthorised"),
    ),
    tag = "devices"
)]
pub async fn revoke_device(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(client_id): Path<String>,
) -> StatusCode {
    match crate::devices::service::set_owned_device_revoked(
        &*state.store,
        &auth.user_id,
        &client_id,
        true,
    )
    .await
    {
        Ok(()) => {
            crate::tc_sync::cryptor_cache::evict_device(&client_id);
            state.runtime_sync.remove_device(&auth.user_id, &client_id);

            tracing::info!(
                target: "audit",
                action = "device.revoke",
                source = "api",
                user_id = %auth.user_id,
                client_id = %client_id,
            );
            StatusCode::NO_CONTENT
        }
        Err(crate::devices::service::DeviceLifecycleError::NotFound) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("Failed to revoke device: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Rename a device. Does not affect credentials or sync behavior.
#[utoipa::path(
    patch,
    path = "/api/devices/{client_id}",
    operation_id = "renameDevice",
    params(("client_id" = String, Path, description = "Device client_id to rename")),
    request_body = RenameDeviceRequest,
    responses(
        (status = 200, description = "Device renamed"),
        (status = 404, description = "Device not found or not owned by user"),
        (status = 401, description = "Unauthorised"),
    ),
    tag = "devices"
)]
pub async fn rename_device(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(client_id): Path<String>,
    Json(body): Json<RenameDeviceRequest>,
) -> StatusCode {
    if crate::validation::validate_or_bad_request(&body, "Invalid device name").is_err() {
        return StatusCode::BAD_REQUEST;
    }

    match crate::devices::service::rename_owned_device(
        &*state.store,
        &auth.user_id,
        &client_id,
        &body.name,
    )
    .await
    {
        Ok(_) => StatusCode::OK,
        Err(crate::devices::service::DeviceLifecycleError::InvalidName) => StatusCode::BAD_REQUEST,
        Err(crate::devices::service::DeviceLifecycleError::NotFound) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("Failed to rename device: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
