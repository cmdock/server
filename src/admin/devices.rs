use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::admin::handlers::{require_existing_user, validate_user_id};
use crate::admin::openapi::{sqlite_utc_to_rfc3339, BootstrapStatusSchema, DeviceStatusSchema};
use crate::app_state::AppState;
use crate::audit;
use crate::auth::OperatorAuth;
use crate::devices::service::{
    delete_owned_device, load_owned_device, provision_device, rename_owned_device,
    render_taskrc_lines, rotate_owned_device, set_owned_device_revoked, DeviceLifecycleError,
};
use crate::store::models::DeviceRecord;

#[derive(Debug, Serialize, ToSchema)]
#[schema(example = json!({
    "clientId": "10acf916-1bfc-41c0-ab14-ce324b931180",
    "name": "Work MacBook",
    "registeredAt": "2026-04-02T10:00:00+00:00",
    "lastSyncAt": "2026-04-02T10:05:00+00:00",
    "lastSyncIp": "203.0.113.10",
    "status": "active",
    "syncRuntime": "mergedGateway",
    "sourceAccount": "PERSONAL",
    "bootstrapRequestId": "8b6f6952-b9e7-4dd8-8143-b2af7a6ae20d",
    "bootstrapStatus": "acknowledged",
    "bootstrapExpiresAt": "2026-04-03T10:00:00+00:00"
}))]
#[serde(rename_all = "camelCase")]
pub struct OperatorDeviceResponse {
    #[schema(format = "uuid", example = "10acf916-1bfc-41c0-ab14-ce324b931180")]
    pub client_id: String,
    pub name: String,
    #[schema(format = "date-time", example = "2026-04-02T10:00:00+00:00")]
    pub registered_at: String,
    #[schema(format = "date-time", example = "2026-04-02T10:05:00+00:00")]
    pub last_sync_at: Option<String>,
    pub last_sync_ip: Option<String>,
    #[schema(value_type = DeviceStatusSchema, example = "active")]
    pub status: String,
    #[schema(example = "mergedGateway")]
    pub sync_runtime: String,
    #[schema(example = "PERSONAL")]
    pub source_account: String,
    #[schema(format = "uuid", example = "8b6f6952-b9e7-4dd8-8143-b2af7a6ae20d")]
    pub bootstrap_request_id: Option<String>,
    #[schema(value_type = Option<BootstrapStatusSchema>, example = "acknowledged")]
    pub bootstrap_status: Option<String>,
    #[schema(format = "date-time", example = "2026-04-03T10:00:00+00:00")]
    pub bootstrap_expires_at: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema, Validate)]
#[schema(example = json!({ "name": "Work MacBook", "publicServerUrlOverride": "https://sync.example.com" }))]
#[serde(rename_all = "camelCase")]
pub struct OperatorCreateDeviceRequest {
    #[garde(
        length(min = 1, max = 255),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub name: String,
    #[garde(inner(
        length(min = 1, max = 2048),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    ))]
    pub public_server_url_override: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[schema(example = json!({
    "userId": "86a9cca3-5689-41e4-8361-8075c9c49b38",
    "clientId": "10acf916-1bfc-41c0-ab14-ce324b931180",
    "encryptionSecret": "1d4d4f77f3f7e303a347d8ce266e46af406387238fdcf688df2f22122617f371",
    "name": "Work MacBook",
    "taskrcLines": [
        "sync.server.url=https://sync.example.com",
        "sync.server.client_id=10acf916-1bfc-41c0-ab14-ce324b931180",
        "sync.encryption_secret=1d4d4f77f3f7e303a347d8ce266e46af406387238fdcf688df2f22122617f371"
    ]
}))]
#[serde(rename_all = "camelCase")]
pub struct OperatorCreateDeviceResponse {
    #[schema(format = "uuid", example = "86a9cca3-5689-41e4-8361-8075c9c49b38")]
    pub user_id: String,
    #[schema(format = "uuid", example = "10acf916-1bfc-41c0-ab14-ce324b931180")]
    pub client_id: String,
    pub encryption_secret: String,
    pub name: String,
    pub taskrc_lines: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[schema(example = json!({
    "userId": "86a9cca3-5689-41e4-8361-8075c9c49b38",
    "oldClientId": "10acf916-1bfc-41c0-ab14-ce324b931180",
    "clientId": "f1447d4a-11d0-40f2-b6c0-36c7cefb59e4",
    "encryptionSecret": "1d4d4f77f3f7e303a347d8ce266e46af406387238fdcf688df2f22122617f371",
    "name": "Work MacBook (rotated)",
    "taskrcLines": [
        "sync.server.url=https://sync.example.com",
        "sync.server.client_id=f1447d4a-11d0-40f2-b6c0-36c7cefb59e4",
        "sync.encryption_secret=1d4d4f77f3f7e303a347d8ce266e46af406387238fdcf688df2f22122617f371"
    ]
}))]
#[serde(rename_all = "camelCase")]
pub struct OperatorRotateDeviceResponse {
    #[schema(format = "uuid", example = "86a9cca3-5689-41e4-8361-8075c9c49b38")]
    pub user_id: String,
    #[schema(format = "uuid", example = "10acf916-1bfc-41c0-ab14-ce324b931180")]
    pub old_client_id: String,
    #[schema(format = "uuid", example = "f1447d4a-11d0-40f2-b6c0-36c7cefb59e4")]
    pub client_id: String,
    pub encryption_secret: String,
    pub name: String,
    pub taskrc_lines: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema, Validate)]
#[schema(example = json!({ "name": "Personal iPhone" }))]
#[serde(rename_all = "camelCase")]
pub struct OperatorRenameDeviceRequest {
    #[garde(
        length(min = 1, max = 255),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub name: String,
}

#[utoipa::path(
    get,
    path = "/admin/user/{user_id}/devices",
    operation_id = "listOperatorDevices",
    params(("user_id" = String, Path, description = "User ID")),
    responses(
        (status = 200, description = "Devices for the target user", body = Vec<OperatorDeviceResponse>),
        (status = 400, description = "Invalid user ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User not found"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn list_devices(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path(user_id): Path<String>,
) -> Result<Json<Vec<OperatorDeviceResponse>>, StatusCode> {
    validate_user_id(&user_id)?;
    require_existing_user(&state, &user_id).await?;

    let devices = state
        .store
        .list_devices(&user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let source_account = gateway_source_account(&state, &user_id).await?;
    Ok(Json(
        devices
            .into_iter()
            .map(|device| map_device(device, &source_account))
            .collect(),
    ))
}

/// Operator authority identifier for `/admin/*` audit entries. The admin
/// HTTP surface authenticates a single shared operator bearer token
/// (`[admin].http_token` / `CMDOCK_ADMIN_TOKEN`) via `OperatorAuth`, never a
/// user API token or a sync credential. There is no per-operator principal,
/// so the ADR-0012 gate-6 "operator authority identifier" field records the
/// authority *type*; `client_ip` carries the source address that
/// distinguishes operators in practice.
const OPERATOR_AUTHORITY_ADMIN_HTTP: &str = "admin_http_token";

/// Best-effort canonical sync-identity client id for audit enrichment. The
/// ADR-0012 gate-6 audit set lists "sync identity" separately from the
/// per-device "device binding"; this resolves the user's canonical sync
/// identity. Returns an empty string if it cannot be read — audit enrichment
/// must never fail the underlying lifecycle operation.
async fn canonical_sync_identity_for_audit(state: &AppState, user_id: &str) -> String {
    crate::admin::services::sync_identity::SyncIdentityService::new(state.store.clone())
        .get_for_user(user_id)
        .await
        .ok()
        .flatten()
        .map(|replica| replica.id)
        .unwrap_or_default()
}

#[utoipa::path(
    post,
    path = "/admin/user/{user_id}/devices",
    operation_id = "createOperatorDevice",
    params(("user_id" = String, Path, description = "User ID")),
    request_body = OperatorCreateDeviceRequest,
    responses(
        (status = 201, description = "Device created for target user", body = OperatorCreateDeviceResponse),
        (status = 400, description = "Invalid request"),
        (status = 403, description = "Runtime policy blocks device provisioning"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User not found"),
        (status = 412, description = "Server bootstrap prerequisites are not configured"),
        (status = 503, description = "Runtime policy is stale or not applied"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn create_device(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(body): Json<OperatorCreateDeviceRequest>,
) -> Result<(StatusCode, Json<OperatorCreateDeviceResponse>), (StatusCode, String)> {
    validate_user_id(&user_id).map_err(|status| (status, "Invalid user ID".to_string()))?;
    crate::validation::validate_or_bad_request_text(&body, "Invalid device request")?;
    require_existing_user(&state, &user_id)
        .await
        .map_err(map_user_error)?;

    let server_url = state
        .config
        .public_server_url(body.public_server_url_override.as_deref())
        .map_err(|err| (StatusCode::PRECONDITION_FAILED, err.to_string()))?;

    let provisioned = provision_device(
        &*state.store,
        &state.config.server.data_dir,
        &user_id,
        &body.name,
        state.config.master_key,
    )
    .await
    .map_err(map_provision_error)?;

    let sync_identity = canonical_sync_identity_for_audit(&state, &user_id).await;
    tracing::info!(
        target: "audit",
        action = "device.register",
        source = "api",
        authority = OPERATOR_AUTHORITY_ADMIN_HTTP,
        client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
        request_id = ?audit::request_id(&headers),
        user_id = %user_id,
        sync_identity = %sync_identity,
        client_id = %provisioned.client_id,
        device_name = %provisioned.name,
        outcome = "success",
        reason = "first_provisioning",
    );

    Ok((
        StatusCode::CREATED,
        Json(OperatorCreateDeviceResponse {
            user_id,
            client_id: provisioned.client_id.clone(),
            encryption_secret: provisioned.encryption_secret_hex.clone(),
            name: provisioned.name,
            taskrc_lines: render_taskrc_lines(
                &server_url,
                &provisioned.client_id,
                &provisioned.encryption_secret_hex,
            ),
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/admin/user/{user_id}/devices/{client_id}",
    operation_id = "getOperatorDevice",
    params(
        ("user_id" = String, Path, description = "User ID"),
        ("client_id" = String, Path, description = "Device client ID")
    ),
    responses(
        (status = 200, description = "Device metadata", body = OperatorDeviceResponse),
        (status = 400, description = "Invalid user ID or device client ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User or device not found"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn get_device(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path((user_id, client_id)): Path<(String, String)>,
) -> Result<Json<OperatorDeviceResponse>, StatusCode> {
    validate_user_id(&user_id)?;
    validate_client_id(&client_id)?;
    require_existing_user(&state, &user_id).await?;

    let device = load_owned_device(&*state.store, &user_id, &client_id)
        .await
        .map_err(|err| match err {
            DeviceLifecycleError::NotFound => StatusCode::NOT_FOUND,
            DeviceLifecycleError::Internal(_) | DeviceLifecycleError::Provision(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            DeviceLifecycleError::InvalidName | DeviceLifecycleError::DeleteRequiresRevoked => {
                StatusCode::BAD_REQUEST
            }
        })?;
    let source_account = gateway_source_account(&state, &user_id).await?;
    Ok(Json(map_device(device, &source_account)))
}

#[utoipa::path(
    patch,
    path = "/admin/user/{user_id}/devices/{client_id}",
    operation_id = "renameOperatorDevice",
    params(
        ("user_id" = String, Path, description = "User ID"),
        ("client_id" = String, Path, description = "Device client ID")
    ),
    request_body = OperatorRenameDeviceRequest,
    responses(
        (status = 204, description = "Device renamed"),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User or device not found"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn rename_device(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    headers: HeaderMap,
    Path((user_id, client_id)): Path<(String, String)>,
    Json(body): Json<OperatorRenameDeviceRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    validate_user_id(&user_id).map_err(|status| (status, "Invalid user ID".to_string()))?;
    validate_client_id(&client_id)
        .map_err(|status| (status, "Invalid device client ID".to_string()))?;
    crate::validation::validate_or_bad_request_text(&body, "Invalid device name")?;
    require_existing_user(&state, &user_id)
        .await
        .map_err(map_user_error)?;
    let name = rename_owned_device(&*state.store, &user_id, &client_id, &body.name)
        .await
        .map_err(map_lifecycle_error)?;

    tracing::info!(
        target: "audit",
        action = "device.rename",
        source = "api",
        client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
        user_id = %user_id,
        client_id = %client_id,
        device_name = %name,
    );

    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/admin/user/{user_id}/devices/{client_id}/revoke",
    operation_id = "revokeOperatorDevice",
    params(
        ("user_id" = String, Path, description = "User ID"),
        ("client_id" = String, Path, description = "Device client ID")
    ),
    responses(
        (status = 204, description = "Device revoked"),
        (status = 400, description = "Invalid user ID or device client ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User or device not found"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn revoke_device(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    headers: HeaderMap,
    Path((user_id, client_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, String)> {
    mutate_device_status(state, headers, user_id, client_id, true).await
}

#[utoipa::path(
    post,
    path = "/admin/user/{user_id}/devices/{client_id}/unrevoke",
    operation_id = "unrevokeOperatorDevice",
    params(
        ("user_id" = String, Path, description = "User ID"),
        ("client_id" = String, Path, description = "Device client ID")
    ),
    responses(
        (status = 204, description = "Device unrevoked"),
        (status = 400, description = "Invalid user ID or device client ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User or device not found"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn unrevoke_device(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    headers: HeaderMap,
    Path((user_id, client_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, String)> {
    mutate_device_status(state, headers, user_id, client_id, false).await
}

#[utoipa::path(
    post,
    path = "/admin/user/{user_id}/devices/{client_id}/rotate",
    operation_id = "rotateOperatorDevice",
    params(
        ("user_id" = String, Path, description = "User ID"),
        ("client_id" = String, Path, description = "Old device client ID")
    ),
    responses(
        (status = 201, description = "Replacement gateway-bound TW identity created and old device revoked", body = OperatorRotateDeviceResponse),
        (status = 400, description = "Invalid user ID or device client ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User or device not found"),
        (status = 412, description = "Server bootstrap prerequisites are not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn rotate_device(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    headers: HeaderMap,
    Path((user_id, client_id)): Path<(String, String)>,
) -> Result<(StatusCode, Json<OperatorRotateDeviceResponse>), (StatusCode, String)> {
    validate_user_id(&user_id).map_err(|status| (status, "Invalid user ID".to_string()))?;
    validate_client_id(&client_id)
        .map_err(|status| (status, "Invalid device client ID".to_string()))?;
    require_existing_user(&state, &user_id)
        .await
        .map_err(map_user_error)?;

    // Validate the public server URL BEFORE mutating credential state. Rotation
    // revokes the old device and provisions a replacement; if we deferred this
    // check until after `rotate_owned_device`, a missing/invalid
    // `public_base_url` would return 412 *after* the swap committed — the
    // operator would never receive the new credential and the `device.rotate`
    // audit event below would never fire, leaving exactly the kind of silent
    // credential transition ADR-0012 gate-6 exists to make auditable. Computing
    // it first means a config error fails fast with zero state change, matching
    // `create_device`'s ordering, and preserving the ADR-0012 gate-6 invariant
    // that a committed rotation always reaches its audit emit.
    let server_url = state
        .config
        .public_server_url(None)
        .map_err(|err| (StatusCode::PRECONDITION_FAILED, err.to_string()))?;

    let replacement = rotate_owned_device(
        &*state.store,
        &state.config.server.data_dir,
        &user_id,
        &client_id,
        state.config.master_key,
    )
    .await
    .map_err(map_lifecycle_error)?;

    state.recovery_runtime.evict_device(&user_id, &client_id);

    // ADR-0012 gate-6 lost-response rotation recovery audit. The required
    // field set (§gate6 acceptance criteria) is: timestamp (added by the JSON
    // audit layer), operator authority identifier (`authority`), Runtime User
    // (`user_id`), sync identity (`sync_identity`), device binding
    // (`old_client_id` + `client_id`), outcome (`outcome`),
    // correlation/request ID (`request_id`), and source address (`client_ip`).
    let sync_identity = canonical_sync_identity_for_audit(&state, &user_id).await;
    tracing::info!(
        target: "audit",
        action = "device.rotate",
        source = "api",
        authority = OPERATOR_AUTHORITY_ADMIN_HTTP,
        client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
        request_id = ?audit::request_id(&headers),
        user_id = %user_id,
        sync_identity = %sync_identity,
        old_client_id = %client_id,
        client_id = %replacement.client_id,
        outcome = "success",
        reason = "rotation_recovery",
    );

    Ok((
        StatusCode::CREATED,
        Json(OperatorRotateDeviceResponse {
            user_id,
            old_client_id: client_id,
            client_id: replacement.client_id.clone(),
            encryption_secret: replacement.encryption_secret_hex.clone(),
            name: replacement.name,
            taskrc_lines: render_taskrc_lines(
                &server_url,
                &replacement.client_id,
                &replacement.encryption_secret_hex,
            ),
        }),
    ))
}

#[utoipa::path(
    delete,
    path = "/admin/user/{user_id}/devices/{client_id}",
    operation_id = "deleteOperatorDevice",
    params(
        ("user_id" = String, Path, description = "User ID"),
        ("client_id" = String, Path, description = "Device client ID")
    ),
    responses(
        (status = 204, description = "Revoked device deleted"),
        (status = 400, description = "Invalid user ID or device client ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User or device not found"),
        (status = 409, description = "Active devices must be revoked before deletion"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn delete_device(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    headers: HeaderMap,
    Path((user_id, client_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, String)> {
    validate_user_id(&user_id).map_err(|status| (status, "Invalid user ID".to_string()))?;
    validate_client_id(&client_id)
        .map_err(|status| (status, "Invalid device client ID".to_string()))?;
    require_existing_user(&state, &user_id)
        .await
        .map_err(map_user_error)?;

    delete_owned_device(
        &*state.store,
        &state.config.server.data_dir,
        &user_id,
        &client_id,
    )
    .await
    .map_err(map_lifecycle_error)?;

    state.recovery_runtime.evict_device(&user_id, &client_id);

    tracing::info!(
        target: "audit",
        action = "device.delete",
        source = "api",
        client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
        user_id = %user_id,
        client_id = %client_id,
    );

    Ok(StatusCode::NO_CONTENT)
}

async fn mutate_device_status(
    state: AppState,
    headers: HeaderMap,
    user_id: String,
    client_id: String,
    revoke: bool,
) -> Result<StatusCode, (StatusCode, String)> {
    validate_user_id(&user_id).map_err(|status| (status, "Invalid user ID".to_string()))?;
    validate_client_id(&client_id)
        .map_err(|status| (status, "Invalid device client ID".to_string()))?;
    require_existing_user(&state, &user_id)
        .await
        .map_err(map_user_error)?;
    set_owned_device_revoked(&*state.store, &user_id, &client_id, revoke)
        .await
        .map_err(map_lifecycle_error)?;

    state.recovery_runtime.evict_device(&user_id, &client_id);

    tracing::info!(
        target: "audit",
        action = if revoke { "device.revoke" } else { "device.unrevoke" },
        source = "api",
        client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
        user_id = %user_id,
        client_id = %client_id,
    );

    Ok(StatusCode::NO_CONTENT)
}

async fn gateway_source_account(state: &AppState, user_id: &str) -> Result<String, StatusCode> {
    state
        .store
        .get_user_prefix(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        .map(|prefix| prefix.unwrap_or_else(|| "personal".to_string()))
}

fn map_device(device: DeviceRecord, source_account: &str) -> OperatorDeviceResponse {
    OperatorDeviceResponse {
        client_id: device.client_id,
        name: device.name,
        registered_at: sqlite_utc_to_rfc3339(&device.registered_at),
        last_sync_at: device
            .last_sync_at
            .map(|value| sqlite_utc_to_rfc3339(&value)),
        last_sync_ip: device.last_sync_ip,
        status: device.status,
        sync_runtime: "mergedGateway".to_string(),
        source_account: source_account.to_string(),
        bootstrap_request_id: device.bootstrap_request_id,
        bootstrap_status: device.bootstrap_status,
        bootstrap_expires_at: device
            .bootstrap_expires_at
            .map(|value| sqlite_utc_to_rfc3339(&value)),
    }
}

fn validate_client_id(client_id: &str) -> Result<(), StatusCode> {
    Uuid::parse_str(client_id)
        .map(|_| ())
        .map_err(|_| StatusCode::BAD_REQUEST)
}

fn map_lifecycle_error(err: DeviceLifecycleError) -> (StatusCode, String) {
    match err {
        DeviceLifecycleError::InvalidName => (StatusCode::BAD_REQUEST, err.to_string()),
        DeviceLifecycleError::NotFound => (StatusCode::NOT_FOUND, err.to_string()),
        DeviceLifecycleError::DeleteRequiresRevoked => (StatusCode::CONFLICT, err.to_string()),
        DeviceLifecycleError::Provision(provision_err) => map_provision_error(provision_err),
        DeviceLifecycleError::Internal(inner) => {
            tracing::error!("Device lifecycle failed: {inner}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error".to_string(),
            )
        }
    }
}

fn map_user_error(status: StatusCode) -> (StatusCode, String) {
    let message = match status {
        StatusCode::NOT_FOUND => "User not found",
        StatusCode::BAD_REQUEST => "Invalid user ID",
        _ => "Internal error",
    };
    (status, message.to_string())
}

fn map_provision_error(err: crate::devices::service::ProvisionDeviceError) -> (StatusCode, String) {
    match err {
        crate::devices::service::ProvisionDeviceError::InvalidName => {
            (StatusCode::BAD_REQUEST, err.to_string())
        }
        crate::devices::service::ProvisionDeviceError::RuntimePolicyBlocked => {
            (StatusCode::FORBIDDEN, err.to_string())
        }
        crate::devices::service::ProvisionDeviceError::RuntimePolicyNotCurrent => {
            (StatusCode::SERVICE_UNAVAILABLE, err.to_string())
        }
        crate::devices::service::ProvisionDeviceError::MissingCanonicalSync
        | crate::devices::service::ProvisionDeviceError::MissingMasterKey => {
            (StatusCode::PRECONDITION_FAILED, err.to_string())
        }
        crate::devices::service::ProvisionDeviceError::MissingStoredSecret => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal error".to_string(),
        ),
        crate::devices::service::ProvisionDeviceError::BootstrapRequestConflict => {
            // Operator device path doesn't supply a bootstrap_request_id;
            // this variant only fires from the bootstrap flow.
            tracing::error!("unexpected bootstrap conflict on admin device create");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error".to_string(),
            )
        }
        crate::devices::service::ProvisionDeviceError::Internal(inner) => {
            tracing::error!("Failed to create operator device: {inner}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error".to_string(),
            )
        }
    }
}
