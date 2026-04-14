use std::path::{Path, PathBuf};

use thiserror::Error;
use uuid::Uuid;

use crate::crypto;
use crate::runtime_policy::{self, RuntimeAccessDecision};
use crate::store::models::DeviceRecord;
use crate::store::ConfigStore;
use crate::sync_identity;

#[derive(Debug, Clone)]
pub struct ProvisionedDevice {
    pub client_id: String,
    pub name: String,
    pub encryption_secret_hex: String,
}

pub fn render_taskrc_lines(server_url: &str, client_id: &str, secret_hex: &str) -> Vec<String> {
    vec![
        format!("sync.server.url={server_url}"),
        format!("sync.server.client_id={client_id}"),
        format!("sync.encryption_secret={secret_hex}"),
    ]
}

#[derive(Debug, Clone, Default)]
pub struct BootstrapDeviceMetadata {
    pub bootstrap_request_id: String,
    pub bootstrap_requested_username: Option<String>,
    pub bootstrap_create_user_if_missing: bool,
    pub bootstrap_expires_at: String,
}

#[derive(Debug, Error)]
pub enum ProvisionDeviceError {
    #[error("Device name must be 1-255 characters")]
    InvalidName,
    #[error("Server has no master key configured (CMDOCK_MASTER_KEY)")]
    MissingMasterKey,
    #[error("Runtime access blocked by policy")]
    RuntimePolicyBlocked,
    #[error("Runtime policy is stale or not applied")]
    RuntimePolicyNotCurrent,
    #[error("User has no canonical sync identity. Run: admin sync create <user_id>")]
    MissingCanonicalSync,
    #[error("Stored device secret is missing")]
    MissingStoredSecret,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum DeviceLifecycleError {
    #[error("Device name must be 1-255 characters")]
    InvalidName,
    #[error("Device not found")]
    NotFound,
    #[error("Active devices must be revoked before deletion")]
    DeleteRequiresRevoked,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub async fn provision_device(
    store: &dyn ConfigStore,
    data_dir: &Path,
    user_id: &str,
    name: &str,
    master_key: Option<[u8; 32]>,
) -> Result<ProvisionedDevice, ProvisionDeviceError> {
    provision_device_with_metadata(store, data_dir, user_id, name, master_key, None).await
}

pub async fn provision_device_with_metadata(
    store: &dyn ConfigStore,
    data_dir: &Path,
    user_id: &str,
    name: &str,
    master_key: Option<[u8; 32]>,
    bootstrap: Option<BootstrapDeviceMetadata>,
) -> Result<ProvisionedDevice, ProvisionDeviceError> {
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 255 {
        return Err(ProvisionDeviceError::InvalidName);
    }

    match runtime_policy::runtime_access_for_user(store, user_id).await? {
        RuntimeAccessDecision::Allow => {}
        RuntimeAccessDecision::Blocked => {
            return Err(ProvisionDeviceError::RuntimePolicyBlocked);
        }
        RuntimeAccessDecision::NotCurrent => {
            return Err(ProvisionDeviceError::RuntimePolicyNotCurrent);
        }
    }

    let master_key = master_key.ok_or(ProvisionDeviceError::MissingMasterKey)?;

    let master_secret =
        match sync_identity::load_canonical_sync_secret(store, user_id, master_key).await {
            Ok(secret) => secret,
            Err(sync_identity::SyncIdentityError::MissingIdentity(_)) => {
                return Err(ProvisionDeviceError::MissingCanonicalSync);
            }
            Err(sync_identity::SyncIdentityError::Internal(err)) => {
                return Err(ProvisionDeviceError::Internal(err));
            }
            Err(sync_identity::SyncIdentityError::AlreadyExists(_)) => {
                unreachable!("load_canonical_sync_secret cannot return AlreadyExists");
            }
        };

    let client_id = Uuid::new_v4().to_string();
    let device_secret_raw =
        crypto::derive_device_secret(&master_secret.secret_raw, client_id.as_bytes())?;
    let encryption_secret_hex = hex::encode(&device_secret_raw);

    let device_secret_enc = crypto::encrypt_secret(&device_secret_raw, &master_key)?;
    let device_secret_enc_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &device_secret_enc,
    );

    let user_dir = data_dir.join("users").join(user_id);
    crate::tc_sync::storage::SyncStorage::open(&user_dir)?;

    match bootstrap {
        Some(bootstrap) => {
            store
                .create_bootstrap_device(
                    user_id,
                    &client_id,
                    &name,
                    &device_secret_enc_b64,
                    &bootstrap.bootstrap_request_id,
                    bootstrap.bootstrap_requested_username.as_deref(),
                    bootstrap.bootstrap_create_user_if_missing,
                    &bootstrap.bootstrap_expires_at,
                )
                .await?;
        }
        None => {
            store
                .create_device(user_id, &client_id, &name, Some(&device_secret_enc_b64))
                .await?;
        }
    }

    Ok(ProvisionedDevice {
        client_id,
        name,
        encryption_secret_hex,
    })
}

pub fn decrypt_device_secret_hex(
    device: &crate::store::models::DeviceRecord,
    master_key: Option<[u8; 32]>,
) -> Result<String, ProvisionDeviceError> {
    let master_key = master_key.ok_or(ProvisionDeviceError::MissingMasterKey)?;
    let enc = device
        .encryption_secret_enc
        .as_deref()
        .ok_or(ProvisionDeviceError::MissingStoredSecret)?;
    let ciphertext = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, enc)
        .map_err(anyhow::Error::from)?;
    let plaintext = crypto::decrypt_secret(&ciphertext, &master_key)?;
    Ok(hex::encode(plaintext))
}

pub async fn load_owned_device(
    store: &dyn ConfigStore,
    user_id: &str,
    client_id: &str,
) -> Result<DeviceRecord, DeviceLifecycleError> {
    let device = store.get_device(client_id).await?;
    match device {
        Some(device) if device.user_id == user_id => Ok(device),
        Some(_) | None => Err(DeviceLifecycleError::NotFound),
    }
}

pub async fn rename_owned_device(
    store: &dyn ConfigStore,
    user_id: &str,
    client_id: &str,
    name: &str,
) -> Result<String, DeviceLifecycleError> {
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 255 {
        return Err(DeviceLifecycleError::InvalidName);
    }
    load_owned_device(store, user_id, client_id).await?;
    let renamed = store.update_device_name(user_id, client_id, &name).await?;
    if !renamed {
        return Err(DeviceLifecycleError::NotFound);
    }
    Ok(name)
}

pub async fn set_owned_device_revoked(
    store: &dyn ConfigStore,
    user_id: &str,
    client_id: &str,
    revoke: bool,
) -> Result<(), DeviceLifecycleError> {
    load_owned_device(store, user_id, client_id).await?;
    let changed = if revoke {
        store.revoke_device(user_id, client_id).await?
    } else {
        store.unrevoke_device(user_id, client_id).await?
    };
    if !changed {
        return Err(DeviceLifecycleError::NotFound);
    }
    Ok(())
}

pub async fn delete_owned_device(
    store: &dyn ConfigStore,
    data_dir: &Path,
    user_id: &str,
    client_id: &str,
) -> Result<(), DeviceLifecycleError> {
    let device = load_owned_device(store, user_id, client_id).await?;
    if device.status != "revoked" {
        return Err(DeviceLifecycleError::DeleteRequiresRevoked);
    }

    let deleted = store.delete_device(user_id, client_id).await?;
    if !deleted {
        return Err(DeviceLifecycleError::NotFound);
    }

    let legacy_db_path = legacy_device_db_path(data_dir, user_id, client_id);
    if legacy_db_path.exists() {
        std::fs::remove_file(legacy_db_path).map_err(anyhow::Error::from)?;
    }

    Ok(())
}

fn legacy_device_db_path(data_dir: &Path, user_id: &str, client_id: &str) -> PathBuf {
    data_dir
        .join("users")
        .join(user_id)
        .join("sync")
        .join(format!("{client_id}.sqlite"))
}
