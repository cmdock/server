use std::time::Instant;

use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::audit;
use crate::auth::runtime_access::enforce_runtime_access;
use crate::metrics as m;

/// Authenticated device info returned by authenticate_sync_client.
pub struct SyncAuth {
    pub user_id: String,
    pub device: crate::store::models::DeviceRecord,
}

fn bootstrap_device_pending_and_expired(device: &crate::store::models::DeviceRecord) -> bool {
    if device.bootstrap_status.as_deref() != Some("pending_delivery") {
        return false;
    }
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    device
        .bootstrap_expires_at
        .as_deref()
        .is_some_and(|expires_at| expires_at <= now.as_str())
}

pub async fn authenticate_sync_client(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<SyncAuth, Response> {
    let header_val = headers
        .get("x-client-id")
        .ok_or_else(|| StatusCode::BAD_REQUEST.into_response())?
        .to_str()
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    let client_id_str = Uuid::parse_str(header_val)
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())?
        .to_string();

    let start = Instant::now();
    let device = state
        .store
        .get_device(&client_id_str)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;

    m::record_config_db_op("sync_auth_check", start.elapsed().as_secs_f64());

    let device = match device {
        Some(d) if d.status == "revoked" => {
            tracing::warn!(
                target: "audit",
                action = "auth.failure",
                source = "api",
                client_ip = %audit::client_ip(headers, state.config.server.trust_forwarded_headers),
                reason = "device_revoked",
                client_id = %client_id_str,
                user_id = %d.user_id,
            );
            return Err((StatusCode::FORBIDDEN, "Device has been revoked").into_response());
        }
        Some(d) if d.bootstrap_status.as_deref() == Some("abandoned") => {
            tracing::warn!(
                target: "audit",
                action = "auth.failure",
                source = "api",
                client_ip = %audit::client_ip(headers, state.config.server.trust_forwarded_headers),
                reason = "bootstrap_device_abandoned",
                client_id = %client_id_str,
                user_id = %d.user_id,
            );
            return Err((StatusCode::FORBIDDEN, "Bootstrap device has expired").into_response());
        }
        Some(d) if bootstrap_device_pending_and_expired(&d) => {
            tracing::warn!(
                target: "audit",
                action = "auth.failure",
                source = "api",
                client_ip = %audit::client_ip(headers, state.config.server.trust_forwarded_headers),
                reason = "bootstrap_device_expired",
                client_id = %client_id_str,
                user_id = %d.user_id,
            );
            return Err((StatusCode::FORBIDDEN, "Bootstrap device has expired").into_response());
        }
        Some(d) => d,
        None => {
            tracing::warn!(
                target: "audit",
                action = "auth.failure",
                source = "api",
                client_ip = %audit::client_ip(headers, state.config.server.trust_forwarded_headers),
                reason = "device_not_registered",
                client_id = %client_id_str,
            );
            return Err(StatusCode::FORBIDDEN.into_response());
        }
    };

    let user_id = device.user_id.clone();

    enforce_runtime_access(
        state.store.clone(),
        headers,
        &user_id,
        Some(client_id_str.as_str()),
        state.config.server.trust_forwarded_headers,
    )
    .await
    .map_err(|rejection| (rejection.status, rejection.message).into_response())?;

    let ip = audit::client_ip(headers, state.config.server.trust_forwarded_headers).to_string();
    if let Err(err) = state.store.touch_device(&device.client_id, &ip).await {
        tracing::warn!(
            "failed to update sync recency for user {} device {}: {err}",
            user_id,
            device.client_id
        );
    }

    Ok(SyncAuth { user_id, device })
}
