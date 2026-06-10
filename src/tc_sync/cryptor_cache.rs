//! Shared SyncCryptor cache for both the sync bridge and TC protocol handlers.
//!
//! Two caches:
//! - **Canonical cache**: keyed by user_id, for the sync bridge (one per user)
//! - **Device cache**: keyed by client_id, for TC protocol handlers (one per device)

use std::sync::{Arc, LazyLock};

use dashmap::DashMap;
use uuid::Uuid;

use crate::crypto;
use crate::store::models::{DeviceRecord, ReplicaRecord};

use super::crypto::SyncCryptor;

/// Cache of canonical (per-user) cryptors. Used by the sync bridge.
static CANONICAL_CACHE: LazyLock<DashMap<String, Arc<SyncCryptor>>> = LazyLock::new(DashMap::new);

/// Cache of per-device cryptors. Used by TC sync protocol handlers.
static DEVICE_CACHE: LazyLock<DashMap<String, Arc<SyncCryptor>>> = LazyLock::new(DashMap::new);

/// Get or create a cached canonical cryptor for a user (sync bridge use).
///
/// The canonical cryptor uses the replica's client_id as PBKDF2 salt and
/// the user's master encryption secret as the password. This matches what
/// the sync bridge uses for REST ↔ TC translation.
pub fn get_or_create_canonical(
    user_id: &str,
    replica: &ReplicaRecord,
    master_key: &[u8; 32],
) -> Result<Arc<SyncCryptor>, anyhow::Error> {
    if let Some(cached) = CANONICAL_CACHE.get(user_id) {
        return Ok(Arc::clone(&cached));
    }

    let secret_enc = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &replica.encryption_secret_enc,
    )?;
    let secret_raw = crypto::decrypt_secret(&secret_enc, master_key)?;

    // TW CLI passes hex-encoded secret bytes to PBKDF2 — we must match
    let secret_hex = hex::encode(&secret_raw);
    let client_id = Uuid::parse_str(&replica.id)?;
    let cryptor = Arc::new(SyncCryptor::new(client_id, secret_hex.as_bytes())?);

    let entry = CANONICAL_CACHE
        .entry(user_id.to_string())
        .or_insert(cryptor);
    Ok(entry.value().clone())
}

/// Get or create a cached device cryptor for a specific device.
///
/// The device cryptor uses the device's client_id as PBKDF2 salt and
/// the device's HKDF-derived encryption secret as the password.
pub fn get_or_create_device(
    device: &DeviceRecord,
    master_key: &[u8; 32],
) -> Result<Arc<SyncCryptor>, anyhow::Error> {
    if let Some(cached) = DEVICE_CACHE.get(&device.client_id) {
        return Ok(Arc::clone(&cached));
    }

    let secret_enc_b64 = device
        .encryption_secret_enc
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("device has no stored encryption secret"))?;

    let secret_enc =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, secret_enc_b64)?;
    let secret_raw = crypto::decrypt_secret(&secret_enc, master_key)?;

    // Device secret is passed to TW CLI as hex, which passes hex bytes to PBKDF2
    let secret_hex = hex::encode(&secret_raw);
    let client_id = Uuid::parse_str(&device.client_id)?;
    let cryptor = Arc::new(SyncCryptor::new(client_id, secret_hex.as_bytes())?);

    let entry = DEVICE_CACHE
        .entry(device.client_id.clone())
        .or_insert(cryptor);
    Ok(entry.value().clone())
}

/// Evict a canonical cryptor (e.g. on key rotation).
pub fn evict_canonical(user_id: &str) {
    CANONICAL_CACHE.remove(user_id);
}

/// Evict a device cryptor (e.g. on device revocation).
pub fn evict_device(client_id: &str) {
    DEVICE_CACHE.remove(client_id);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::store::models::{DeviceRecord, ReplicaRecord};

    use super::*;

    fn master_key() -> [u8; 32] {
        [42u8; 32]
    }

    fn encrypted_secret_b64(raw: &[u8]) -> String {
        let encrypted = crypto::encrypt_secret(raw, &master_key()).unwrap();
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, encrypted)
    }

    fn replica_record(user_id: &str, client_id: &str) -> ReplicaRecord {
        ReplicaRecord {
            id: client_id.to_string(),
            user_id: user_id.to_string(),
            label: "default".to_string(),
            encryption_secret_enc: encrypted_secret_b64(b"canonical-secret-32-bytes-material"),
            created_at: "2026-04-01 00:00:00".to_string(),
        }
    }

    fn device_record(user_id: &str, client_id: &str) -> DeviceRecord {
        DeviceRecord {
            client_id: client_id.to_string(),
            user_id: user_id.to_string(),
            name: "Test Device".to_string(),
            encryption_secret_enc: Some(encrypted_secret_b64(b"device-secret-32-bytes-material!!")),
            registered_at: "2026-04-01 00:00:00".to_string(),
            last_sync_at: None,
            last_sync_ip: None,
            status: "active".to_string(),
            bootstrap_request_id: None,
            bootstrap_status: None,
            bootstrap_requested_username: None,
            bootstrap_create_user_if_missing: None,
            bootstrap_expires_at: None,
        }
    }

    #[test]
    fn canonical_cache_reuses_entries_until_evicted() {
        let user_id = "user-cache-test";
        let replica = replica_record(user_id, "11111111-1111-1111-1111-111111111111");

        evict_canonical(user_id);

        let first = get_or_create_canonical(user_id, &replica, &master_key()).unwrap();
        let second = get_or_create_canonical(user_id, &replica, &master_key()).unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "canonical cache should reuse the same Arc while cached"
        );

        evict_canonical(user_id);

        let third = get_or_create_canonical(user_id, &replica, &master_key()).unwrap();
        assert!(
            !Arc::ptr_eq(&first, &third),
            "canonical cache should build a new Arc after eviction"
        );

        evict_canonical(user_id);
    }

    #[test]
    fn device_cache_reuses_entries_until_evicted() {
        let client_id = "22222222-2222-2222-2222-222222222222";
        let device = device_record("user-cache-test", client_id);

        evict_device(client_id);

        let first = get_or_create_device(&device, &master_key()).unwrap();
        let second = get_or_create_device(&device, &master_key()).unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "device cache should reuse the same Arc while cached"
        );

        evict_device(client_id);

        let third = get_or_create_device(&device, &master_key()).unwrap();
        assert!(
            !Arc::ptr_eq(&first, &third),
            "device cache should build a new Arc after eviction"
        );

        evict_device(client_id);
    }

    #[test]
    fn device_cache_requires_stored_secret() {
        let client_id = "33333333-3333-3333-3333-333333333333";
        let mut device = device_record("user-cache-test", client_id);
        device.encryption_secret_enc = None;

        evict_device(client_id);

        let err = match get_or_create_device(&device, &master_key()) {
            Ok(_) => panic!("expected device cryptor creation to fail without a stored secret"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("no stored encryption secret"));
    }
}
