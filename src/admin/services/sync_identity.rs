use std::sync::Arc;

use crate::store::models::ReplicaRecord;
use crate::store::ConfigStore;
use crate::sync_identity::{self, CreatedCanonicalSyncIdentity, DecryptedCanonicalSyncIdentity};

#[derive(Clone)]
pub struct SyncIdentityService {
    store: Arc<dyn ConfigStore>,
}

impl SyncIdentityService {
    pub fn new(store: Arc<dyn ConfigStore>) -> Self {
        Self { store }
    }

    pub async fn get_for_user(&self, user_id: &str) -> anyhow::Result<Option<ReplicaRecord>> {
        self.store.get_replica_by_user(user_id).await
    }

    pub async fn create_for_user(
        &self,
        user_id: &str,
        master_key: [u8; 32],
    ) -> Result<CreatedCanonicalSyncIdentity, sync_identity::SyncIdentityError> {
        sync_identity::create_canonical_sync_identity(self.store.as_ref(), user_id, master_key)
            .await
    }

    pub async fn decrypt_secret_for_user(
        &self,
        user_id: &str,
        master_key: [u8; 32],
    ) -> Result<DecryptedCanonicalSyncIdentity, sync_identity::SyncIdentityError> {
        sync_identity::decrypt_canonical_sync_secret(self.store.as_ref(), user_id, master_key).await
    }

    pub async fn delete_for_user(
        &self,
        user_id: &str,
    ) -> Result<bool, sync_identity::SyncIdentityError> {
        sync_identity::delete_canonical_sync_identity(self.store.as_ref(), user_id).await
    }

    pub async fn ensure_record_for_user(
        &self,
        user_id: &str,
        master_key: [u8; 32],
    ) -> Result<(ReplicaRecord, bool), sync_identity::SyncIdentityError> {
        if let Some(existing) = self.get_for_user(user_id).await? {
            return Ok((existing, false));
        }

        let created = match self.create_for_user(user_id, master_key).await {
            Ok(created) => created,
            Err(sync_identity::SyncIdentityError::AlreadyExists(_)) => {
                let existing = self.get_for_user(user_id).await?.ok_or_else(|| {
                    sync_identity::SyncIdentityError::Internal(anyhow::anyhow!(
                        "sync identity already exists but could not be reloaded"
                    ))
                })?;
                return Ok((existing, false));
            }
            Err(sync_identity::SyncIdentityError::Internal(err))
                if is_replica_user_unique_violation(&err) =>
            {
                let existing = self.get_for_user(user_id).await?.ok_or_else(|| {
                    sync_identity::SyncIdentityError::Internal(anyhow::anyhow!(
                        "sync identity create raced but could not be reloaded"
                    ))
                })?;
                return Ok((existing, false));
            }
            Err(other) => return Err(other),
        };

        let replica = self.get_for_user(user_id).await?.ok_or_else(|| {
            sync_identity::SyncIdentityError::Internal(anyhow::anyhow!(
                "created sync identity but could not reload it"
            ))
        })?;

        if replica.id != created.client_id {
            return Err(sync_identity::SyncIdentityError::Internal(anyhow::anyhow!(
                "reloaded sync identity did not match created client_id"
            )));
        }

        Ok((replica, true))
    }

    pub async fn ensure_for_user(
        &self,
        user_id: &str,
        master_key: [u8; 32],
    ) -> Result<CreatedCanonicalSyncIdentity, sync_identity::SyncIdentityError> {
        let (replica, _created) = self.ensure_record_for_user(user_id, master_key).await?;
        Ok(CreatedCanonicalSyncIdentity {
            client_id: replica.id,
        })
    }
}

fn is_replica_user_unique_violation(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains("UNIQUE constraint failed: replicas.user_id")
}
