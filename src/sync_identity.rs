use thiserror::Error;
use uuid::Uuid;

use crate::crypto;
use crate::store::ConfigStore;

#[derive(Debug, Clone)]
pub struct CreatedCanonicalSyncIdentity {
    pub client_id: String,
}

#[derive(Debug, Clone)]
pub struct DecryptedCanonicalSyncIdentity {
    pub client_id: String,
    pub encryption_secret_hex: String,
}

#[derive(Debug, Clone)]
pub struct CanonicalSyncSecret {
    pub client_id: String,
    pub secret_raw: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum SyncIdentityError {
    #[error("User already has a canonical sync identity (client_id: {0})")]
    AlreadyExists(String),
    #[error("No canonical sync identity for user {0}")]
    MissingIdentity(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub async fn create_canonical_sync_identity(
    store: &dyn ConfigStore,
    user_id: &str,
    master_key: [u8; 32],
) -> Result<CreatedCanonicalSyncIdentity, SyncIdentityError> {
    if let Some(existing) = store.get_replica_by_user(user_id).await? {
        return Err(SyncIdentityError::AlreadyExists(existing.id));
    }

    let client_id = Uuid::new_v4().to_string();
    let mut secret_bytes = [0u8; 32];
    use ring::rand::{SecureRandom, SystemRandom};
    SystemRandom::new()
        .fill(&mut secret_bytes)
        .map_err(|_| anyhow::anyhow!("failed to generate random secret"))?;

    let encrypted = crypto::encrypt_secret(&secret_bytes, &master_key)?;
    let encrypted_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &encrypted);

    store
        .create_replica(user_id, &client_id, &encrypted_b64)
        .await?;

    Ok(CreatedCanonicalSyncIdentity { client_id })
}

pub async fn decrypt_canonical_sync_secret(
    store: &dyn ConfigStore,
    user_id: &str,
    master_key: [u8; 32],
) -> Result<DecryptedCanonicalSyncIdentity, SyncIdentityError> {
    let secret = load_canonical_sync_secret(store, user_id, master_key).await?;
    Ok(DecryptedCanonicalSyncIdentity {
        client_id: secret.client_id,
        encryption_secret_hex: hex::encode(secret.secret_raw),
    })
}

pub async fn load_canonical_sync_secret(
    store: &dyn ConfigStore,
    user_id: &str,
    master_key: [u8; 32],
) -> Result<CanonicalSyncSecret, SyncIdentityError> {
    let replica = store
        .get_replica_by_user(user_id)
        .await?
        .ok_or_else(|| SyncIdentityError::MissingIdentity(user_id.to_string()))?;

    let encrypted = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &replica.encryption_secret_enc,
    )
    .map_err(anyhow::Error::from)?;
    let secret = crypto::decrypt_secret(&encrypted, &master_key)?;

    Ok(CanonicalSyncSecret {
        client_id: replica.id,
        secret_raw: secret.to_vec(),
    })
}

pub async fn delete_canonical_sync_identity(
    store: &dyn ConfigStore,
    user_id: &str,
) -> Result<bool, SyncIdentityError> {
    Ok(store.delete_replica(user_id).await?)
}
