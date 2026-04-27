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

async fn load_sync_cryptors(
    state: &AppState,
    device: &crate::store::models::DeviceRecord,
) -> Result<(Arc<SyncCryptor>, Arc<SyncCryptor>), StatusCode> {
    let master_key = state
        .config
        .master_key
        .ok_or(StatusCode::PRECONDITION_FAILED)?;
    let replica = state
        .store
        .get_replica_by_user(&device.user_id)
        .await
        .map_err(|err| {
            tracing::error!(
                "failed to load canonical sync identity for user {}: {err}",
                device.user_id
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::PRECONDITION_FAILED)?;
    let canonical = crate::tc_sync::cryptor_cache::get_or_create_canonical(
        &device.user_id,
        &replica,
        &master_key,
    )
    .map_err(|err| {
        tracing::error!(
            "failed to load canonical cryptor for user {}: {err}",
            device.user_id
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let device_cryptor = crate::tc_sync::cryptor_cache::get_or_create_device(device, &master_key)
        .map_err(|err| {
        tracing::error!(
            "failed to load device cryptor for user {} device {}: {err}",
            device.user_id,
            device.client_id
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok((canonical, device_cryptor))
}

pub async fn translate_inbound_device_payload(
    state: &AppState,
    device: &crate::store::models::DeviceRecord,
    version_id: Uuid,
    body: &[u8],
) -> Result<Vec<u8>, StatusCode> {
    if state.config.master_key.is_none() {
        return Ok(body.to_vec());
    }
    let (canonical, device_cryptor) = load_sync_cryptors(state, device).await?;
    let plaintext = device_cryptor.unseal(version_id, body).map_err(|err| {
        tracing::warn!(
            "rejected invalid sync payload for user {} device {} version {}: {err}",
            device.user_id,
            device.client_id,
            version_id
        );
        StatusCode::BAD_REQUEST
    })?;
    canonical.seal(version_id, &plaintext).map_err(|err| {
        tracing::error!(
            "failed to translate sync payload into canonical envelope for user {} device {} version {}: {err}",
            device.user_id,
            device.client_id,
            version_id
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

pub async fn translate_outbound_canonical_payload(
    state: &AppState,
    device: &crate::store::models::DeviceRecord,
    version_id: Uuid,
    body: &[u8],
) -> Result<Vec<u8>, StatusCode> {
    if state.config.master_key.is_none() {
        return Ok(body.to_vec());
    }
    let (canonical, device_cryptor) = load_sync_cryptors(state, device).await?;
    let plaintext = canonical.unseal(version_id, body).map_err(|err| {
        tracing::error!(
            "failed to decode canonical sync payload for user {} version {}: {err}",
            device.user_id,
            version_id
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    device_cryptor.seal(version_id, &plaintext).map_err(|err| {
        tracing::error!(
            "failed to re-encrypt sync payload for user {} device {} version {}: {err}",
            device.user_id,
            device.client_id,
            version_id
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })
}
