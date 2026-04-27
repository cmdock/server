use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Duration, Utc};
use thiserror::Error;
use uuid::Uuid;

use crate::admin::services::sync_identity::SyncIdentityService;
use crate::devices::service::{
    decrypt_device_secret_hex, provision_device_with_metadata, BootstrapDeviceMetadata,
    ProvisionDeviceError,
};
use crate::runtime_policy::{self, RuntimeAccessDecision};
use crate::store::models::{DeviceRecord, NewUser, UserRecord};
use crate::store::ConfigStore;

const BOOTSTRAP_TTL_HOURS: i64 = 24;

#[derive(Debug, Clone)]
pub struct BootstrapUserDeviceRequest {
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub create_user_if_missing: bool,
    pub device_name: String,
    pub bootstrap_request_id: String,
}

#[derive(Debug, Clone)]
pub struct BootstrapUserDeviceResult {
    pub user: UserRecord,
    pub canonical_client_id: String,
    pub device_client_id: String,
    pub encryption_secret_hex: String,
    pub bootstrap_status: String,
    pub created_user: bool,
}

#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("bootstrapRequestId must be a valid UUID")]
    InvalidBootstrapRequestId,
    #[error("deviceName must be 1-255 characters")]
    InvalidDeviceName,
    #[error("Either userId or username is required")]
    MissingUserSelector,
    #[error("createUserIfMissing=true requires username")]
    CreateRequiresUsername,
    #[error("User not found")]
    UserNotFound,
    #[error("bootstrapRequestId payload does not match the original bootstrap request")]
    BootstrapRequestConflict,
    #[error("Server has no master key configured (CMDOCK_MASTER_KEY)")]
    MissingMasterKey,
    #[error(transparent)]
    Provision(#[from] ProvisionDeviceError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Clone)]
pub struct BootstrapService {
    store: Arc<dyn ConfigStore>,
    data_dir: PathBuf,
    sync_identity: SyncIdentityService,
}

impl BootstrapService {
    pub fn new(store: Arc<dyn ConfigStore>, data_dir: PathBuf) -> Self {
        let sync_identity = SyncIdentityService::new(store.clone());
        Self {
            store,
            data_dir,
            sync_identity,
        }
    }

    pub async fn bootstrap_user_device(
        &self,
        req: BootstrapUserDeviceRequest,
        master_key: Option<[u8; 32]>,
    ) -> Result<BootstrapUserDeviceResult, BootstrapError> {
        let device_name = req.device_name.trim().to_string();
        if device_name.is_empty() || device_name.len() > 255 {
            return Err(BootstrapError::InvalidDeviceName);
        }

        let bootstrap_request_id = Uuid::parse_str(req.bootstrap_request_id.trim())
            .map_err(|_| BootstrapError::InvalidBootstrapRequestId)?
            .to_string();
        let requested_user_id = req
            .user_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let requested_username = req
            .username
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        if requested_user_id.is_none() && requested_username.is_none() {
            return Err(BootstrapError::MissingUserSelector);
        }
        if req.create_user_if_missing && requested_username.is_none() {
            return Err(BootstrapError::CreateRequiresUsername);
        }

        if let Some(existing) = self
            .store
            .get_device_by_bootstrap_request(&bootstrap_request_id)
            .await?
        {
            let user = self
                .store
                .get_user_by_id(&existing.user_id)
                .await?
                .ok_or(BootstrapError::UserNotFound)?;
            self.ensure_device_provisioning_allowed(&user.id).await?;
            self.validate_existing_request(
                &existing,
                &user,
                requested_user_id,
                requested_username.as_deref(),
                req.create_user_if_missing,
                &device_name,
            )?;

            if self.bootstrap_device_expired(&existing) {
                let _ = self
                    .store
                    .delete_device(&existing.user_id, &existing.client_id)
                    .await?;
            } else {
                let canonical = self
                    .sync_identity
                    .ensure_for_user(
                        &user.id,
                        master_key.ok_or(BootstrapError::MissingMasterKey)?,
                    )
                    .await
                    .map_err(map_sync_identity_err)?;
                let encryption_secret_hex = decrypt_device_secret_hex(&existing, master_key)?;
                return Ok(BootstrapUserDeviceResult {
                    user,
                    canonical_client_id: canonical.client_id,
                    device_client_id: existing.client_id,
                    encryption_secret_hex,
                    bootstrap_status: existing
                        .bootstrap_status
                        .unwrap_or_else(|| "acknowledged".to_string()),
                    created_user: false,
                });
            }
        }

        let (user, created_user) = self
            .resolve_user(
                requested_user_id,
                requested_username.as_deref(),
                req.create_user_if_missing,
            )
            .await?;
        self.ensure_device_provisioning_allowed(&user.id).await?;

        self.sweep_expired_bootstrap_devices(&user.id).await?;

        let canonical = self
            .sync_identity
            .ensure_for_user(
                &user.id,
                master_key.ok_or(BootstrapError::MissingMasterKey)?,
            )
            .await
            .map_err(map_sync_identity_err)?;

        let expires_at = (Utc::now() + Duration::hours(BOOTSTRAP_TTL_HOURS))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        let provisioned = match provision_device_with_metadata(
            self.store.as_ref(),
            &self.data_dir,
            &user.id,
            &device_name,
            master_key,
            Some(BootstrapDeviceMetadata {
                bootstrap_request_id: bootstrap_request_id.clone(),
                bootstrap_requested_username: requested_username.clone(),
                bootstrap_create_user_if_missing: req.create_user_if_missing,
                bootstrap_expires_at: expires_at,
            }),
        )
        .await
        {
            Ok(provisioned) => provisioned,
            Err(ProvisionDeviceError::Internal(err))
                if is_bootstrap_request_unique_violation(&err) =>
            {
                let existing = self
                    .store
                    .get_device_by_bootstrap_request(&bootstrap_request_id)
                    .await?
                    .ok_or(err)?;
                self.validate_existing_request(
                    &existing,
                    &user,
                    requested_user_id,
                    requested_username.as_deref(),
                    req.create_user_if_missing,
                    &device_name,
                )?;
                let encryption_secret_hex = decrypt_device_secret_hex(&existing, master_key)?;
                return Ok(BootstrapUserDeviceResult {
                    user,
                    canonical_client_id: canonical.client_id,
                    device_client_id: existing.client_id,
                    encryption_secret_hex,
                    bootstrap_status: existing
                        .bootstrap_status
                        .unwrap_or_else(|| "acknowledged".to_string()),
                    created_user,
                });
            }
            Err(err) => return Err(err.into()),
        };

        Ok(BootstrapUserDeviceResult {
            user,
            canonical_client_id: canonical.client_id,
            device_client_id: provisioned.client_id,
            encryption_secret_hex: provisioned.encryption_secret_hex,
            bootstrap_status: "pending_delivery".to_string(),
            created_user,
        })
    }

    pub async fn acknowledge_bootstrap_request(
        &self,
        bootstrap_request_id: &str,
    ) -> Result<bool, BootstrapError> {
        let bootstrap_request_id = Uuid::parse_str(bootstrap_request_id.trim())
            .map_err(|_| BootstrapError::InvalidBootstrapRequestId)?
            .to_string();
        self.store
            .acknowledge_bootstrap_device(&bootstrap_request_id)
            .await
            .map_err(BootstrapError::Internal)
    }

    async fn resolve_user(
        &self,
        requested_user_id: Option<&str>,
        requested_username: Option<&str>,
        create_user_if_missing: bool,
    ) -> Result<(UserRecord, bool), BootstrapError> {
        match (requested_user_id, requested_username) {
            (Some(user_id), None) => {
                if create_user_if_missing {
                    return Err(BootstrapError::CreateRequiresUsername);
                }
                let user = self
                    .store
                    .get_user_by_id(user_id)
                    .await?
                    .ok_or(BootstrapError::UserNotFound)?;
                Ok((user, false))
            }
            (None, Some(username)) => {
                if let Some(user) = self.store.get_user_by_username(username).await? {
                    return Ok((user, false));
                }
                if !create_user_if_missing {
                    return Err(BootstrapError::UserNotFound);
                }
                match self
                    .store
                    .create_user(&NewUser {
                        username: username.to_string(),
                        password_hash: String::new(),
                    })
                    .await
                {
                    Ok(user) => {
                        crate::views::defaults::reconcile_default_views(
                            self.store.as_ref(),
                            &user.id,
                        )
                        .await?;
                        Ok((user, true))
                    }
                    Err(err) if is_username_unique_violation(&err) => {
                        let user = self
                            .store
                            .get_user_by_username(username)
                            .await?
                            .ok_or(err)?;
                        Ok((user, false))
                    }
                    Err(err) => Err(BootstrapError::Internal(err)),
                }
            }
            (Some(user_id), Some(username)) => {
                let user_by_id = self
                    .store
                    .get_user_by_id(user_id)
                    .await?
                    .ok_or(BootstrapError::UserNotFound)?;
                if user_by_id.username != username {
                    return Err(BootstrapError::BootstrapRequestConflict);
                }
                Ok((user_by_id, false))
            }
            (None, None) => Err(BootstrapError::MissingUserSelector),
        }
    }

    async fn sweep_expired_bootstrap_devices(&self, user_id: &str) -> Result<(), BootstrapError> {
        let now = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let devices = self.store.list_devices(user_id).await?;
        for device in devices {
            let expired = matches!(device.bootstrap_status.as_deref(), Some("pending_delivery"))
                && device
                    .bootstrap_expires_at
                    .as_deref()
                    .is_some_and(|expires_at| expires_at <= now.as_str());
            if expired {
                let _ = self.store.delete_device(user_id, &device.client_id).await?;
            }
        }
        Ok(())
    }

    async fn ensure_device_provisioning_allowed(
        &self,
        user_id: &str,
    ) -> Result<(), BootstrapError> {
        match runtime_policy::runtime_access_for_user(self.store.as_ref(), user_id).await? {
            RuntimeAccessDecision::Allow => Ok(()),
            RuntimeAccessDecision::Blocked => Err(BootstrapError::Provision(
                ProvisionDeviceError::RuntimePolicyBlocked,
            )),
            RuntimeAccessDecision::NotCurrent => Err(BootstrapError::Provision(
                ProvisionDeviceError::RuntimePolicyNotCurrent,
            )),
        }
    }

    fn validate_existing_request(
        &self,
        existing: &DeviceRecord,
        user: &UserRecord,
        requested_user_id: Option<&str>,
        requested_username: Option<&str>,
        create_user_if_missing: bool,
        device_name: &str,
    ) -> Result<(), BootstrapError> {
        if requested_user_id.is_some_and(|id| id != user.id) {
            return Err(BootstrapError::BootstrapRequestConflict);
        }
        if requested_username != existing.bootstrap_requested_username.as_deref() {
            return Err(BootstrapError::BootstrapRequestConflict);
        }
        if existing.user_id != user.id
            || existing.name != device_name
            || existing.bootstrap_create_user_if_missing != Some(create_user_if_missing)
        {
            return Err(BootstrapError::BootstrapRequestConflict);
        }
        Ok(())
    }

    fn bootstrap_device_expired(&self, device: &DeviceRecord) -> bool {
        let now = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        matches!(device.bootstrap_status.as_deref(), Some("pending_delivery"))
            && device
                .bootstrap_expires_at
                .as_deref()
                .is_some_and(|expires_at| expires_at <= now.as_str())
    }
}

fn is_username_unique_violation(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains("UNIQUE constraint failed: users.username")
}

fn is_bootstrap_request_unique_violation(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains("UNIQUE constraint failed: devices.bootstrap_request_id")
}

fn map_sync_identity_err(err: crate::sync_identity::SyncIdentityError) -> BootstrapError {
    match err {
        crate::sync_identity::SyncIdentityError::Internal(inner) => BootstrapError::Internal(inner),
        crate::sync_identity::SyncIdentityError::MissingIdentity(_) => {
            BootstrapError::Provision(ProvisionDeviceError::MissingCanonicalSync)
        }
        crate::sync_identity::SyncIdentityError::AlreadyExists(_) => BootstrapError::Internal(
            anyhow::anyhow!("unexpected existing sync identity conflict"),
        ),
    }
}
