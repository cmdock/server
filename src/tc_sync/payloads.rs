use std::sync::Arc;

use axum::http::StatusCode;
use uuid::Uuid;

use crate::app_state::AppState;

use super::crypto::SyncCryptor;

pub fn ensure_device_bridge_ready(
    state: &AppState,
    device: &crate::store::models::DeviceRecord,
) -> Result<(), StatusCode> {
    if state.config.master_key.is_some() && device.encryption_secret_enc.is_none() {
        tracing::error!(
            "registered device {} for user {} has no stored encryption secret",
            device.client_id,
            device.user_id
        );
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok(())
}

fn load_device_cryptor(
    state: &AppState,
    device: &crate::store::models::DeviceRecord,
) -> Result<Arc<SyncCryptor>, StatusCode> {
    let master_key = state
        .config
        .master_key
        .ok_or(StatusCode::PRECONDITION_FAILED)?;
    crate::tc_sync::cryptor_cache::get_or_create_device(device, &master_key).map_err(|err| {
        tracing::error!(
            "failed to load device cryptor for user {} device {}: {err}",
            device.user_id,
            device.client_id,
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

/// Translate a device-encrypted protocol body into plaintext for the merged
/// gateway. The gateway stores plaintext merged-chain segments internally.
pub async fn translate_inbound_device_payload_plaintext(
    state: &AppState,
    device: &crate::store::models::DeviceRecord,
    version_id: Uuid,
    body: &[u8],
) -> Result<Vec<u8>, StatusCode> {
    if state.config.master_key.is_none() {
        return Ok(body.to_vec());
    }
    let device_cryptor = load_device_cryptor(state, device)?;
    device_cryptor.unseal(version_id, body).map_err(|err| {
        tracing::warn!(
            "rejected invalid sync payload for user {} device {} version {}: {err}",
            device.user_id,
            device.client_id,
            version_id
        );
        StatusCode::BAD_REQUEST
    })
}

/// Translate a plaintext merged-chain payload to the requesting device's
/// encryption envelope.
pub async fn translate_outbound_plaintext_payload(
    state: &AppState,
    device: &crate::store::models::DeviceRecord,
    version_id: Uuid,
    plaintext: &[u8],
) -> Result<Vec<u8>, StatusCode> {
    if state.config.master_key.is_none() {
        return Ok(plaintext.to_vec());
    }
    let device_cryptor = load_device_cryptor(state, device)?;
    device_cryptor.seal(version_id, plaintext).map_err(|err| {
        tracing::error!(
            "failed to encrypt sync payload for user {} device {} version {}: {err}",
            device.user_id,
            device.client_id,
            version_id
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })
}
