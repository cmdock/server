//! Server trait implementation that bridges TC's sync protocol with our storage.
//!
//! `SyncBridgeServer` implements `taskchampion::Server` backed by [`SyncStorage`]
//! for persistence and [`SyncCryptor`] for envelope encryption. This allows
//! `Replica::sync()` to work directly against our storage layer without going
//! through HTTP.
//!
//! Encryption semantics match TC's `SyncServer`:
//! - History segments use `parent_version_id` as the AAD version_id
//! - Snapshots use the snapshot's own `version_id` as the AAD version_id

use std::sync::Arc;

use async_trait::async_trait;

use super::crypto::SyncCryptor;
use super::storage::SyncStorage;

/// Threshold for requesting a snapshot (versions since last snapshot).
const SNAPSHOT_URGENCY_LOW: u64 = 100;
const SNAPSHOT_URGENCY_HIGH: u64 = 1000;

/// A `Server` implementation that bridges the TC sync protocol with our
/// [`SyncStorage`] + [`SyncCryptor`].
///
/// Encrypts plaintext history segments before storing, and decrypts on
/// retrieval, using the user's escrowed encryption key.
pub struct SyncBridgeServer {
    storage: SyncStorage,
    cryptor: Arc<SyncCryptor>,
}

impl SyncBridgeServer {
    /// Create a new bridge server.
    ///
    /// `storage` — a single device's sync storage (SQLite).
    /// `cryptor` — cryptor initialised with that device's client_id and encryption secret.
    pub fn new(storage: SyncStorage, cryptor: SyncCryptor) -> Self {
        Self {
            storage,
            cryptor: Arc::new(cryptor),
        }
    }

    /// Create a new bridge server with a shared (cached) cryptor.
    ///
    /// Used by the sync bridge to avoid re-deriving the PBKDF2 key.
    pub fn new_with_arc(storage: SyncStorage, cryptor: Arc<SyncCryptor>) -> Self {
        Self { storage, cryptor }
    }
}

#[async_trait(?Send)]
impl taskchampion::Server for SyncBridgeServer {
    async fn add_version(
        &mut self,
        parent_version_id: taskchampion::server::VersionId,
        history_segment: taskchampion::server::HistorySegment,
    ) -> std::result::Result<
        (
            taskchampion::server::AddVersionResult,
            taskchampion::server::SnapshotUrgency,
        ),
        taskchampion::Error,
    > {
        // Encrypt the plaintext history segment.
        // TC's SyncServer uses parent_version_id as the AAD version_id for history segments.
        let encrypted = self
            .cryptor
            .seal(parent_version_id, &history_segment)
            .map_err(|e| taskchampion::Error::Server(e.to_string()))?;

        // Store encrypted bytes
        match self
            .storage
            .add_version(parent_version_id, &encrypted)
            .map_err(|e| taskchampion::Error::Server(e.to_string()))?
        {
            Ok(version_id) => {
                let urgency = self.snapshot_urgency();
                Ok((
                    taskchampion::server::AddVersionResult::Ok(version_id),
                    urgency,
                ))
            }
            Err(expected_parent) => Ok((
                taskchampion::server::AddVersionResult::ExpectedParentVersion(expected_parent),
                taskchampion::server::SnapshotUrgency::None,
            )),
        }
    }

    async fn get_child_version(
        &mut self,
        parent_version_id: taskchampion::server::VersionId,
    ) -> std::result::Result<taskchampion::server::GetVersionResult, taskchampion::Error> {
        let (child, _parent_known, _has_versions) = self
            .storage
            .get_child_version_with_context(parent_version_id)
            .map_err(|e| taskchampion::Error::Server(e.to_string()))?;

        match child {
            Some((version_id, pvid, encrypted_segment)) => {
                // TC's SyncServer uses parent_version_id as the AAD version_id when unsealing.
                let plaintext = self
                    .cryptor
                    .unseal(pvid, &encrypted_segment)
                    .map_err(|e| taskchampion::Error::Server(e.to_string()))?;
                Ok(taskchampion::server::GetVersionResult::Version {
                    version_id,
                    parent_version_id: pvid,
                    history_segment: plaintext,
                })
            }
            None => Ok(taskchampion::server::GetVersionResult::NoSuchVersion),
        }
    }

    async fn add_snapshot(
        &mut self,
        version_id: taskchampion::server::VersionId,
        snapshot: taskchampion::server::Snapshot,
    ) -> std::result::Result<(), taskchampion::Error> {
        // TC's SyncServer uses the snapshot's own version_id as the AAD version_id.
        let encrypted = self
            .cryptor
            .seal(version_id, &snapshot)
            .map_err(|e| taskchampion::Error::Server(e.to_string()))?;

        self.storage
            .add_snapshot(version_id, &encrypted)
            .map_err(|e| taskchampion::Error::Server(e.to_string()))?;
        Ok(())
    }

    async fn get_snapshot(
        &mut self,
    ) -> std::result::Result<
        Option<(
            taskchampion::server::VersionId,
            taskchampion::server::Snapshot,
        )>,
        taskchampion::Error,
    > {
        let snap = self
            .storage
            .get_snapshot()
            .map_err(|e| taskchampion::Error::Server(e.to_string()))?;

        match snap {
            Some((version_id, encrypted)) => {
                // TC's SyncServer uses version_id as the AAD version_id for snapshots.
                let plaintext = self
                    .cryptor
                    .unseal(version_id, &encrypted)
                    .map_err(|e| taskchampion::Error::Server(e.to_string()))?;
                Ok(Some((version_id, plaintext)))
            }
            None => Ok(None),
        }
    }
}

impl SyncBridgeServer {
    /// Compute snapshot urgency based on versions since last snapshot.
    fn snapshot_urgency(&self) -> taskchampion::server::SnapshotUrgency {
        match self.storage.versions_since_snapshot() {
            Ok(n) if n >= SNAPSHOT_URGENCY_HIGH => taskchampion::server::SnapshotUrgency::High,
            Ok(n) if n >= SNAPSHOT_URGENCY_LOW => taskchampion::server::SnapshotUrgency::Low,
            _ => taskchampion::server::SnapshotUrgency::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taskchampion::server::{
        AddVersionResult, GetVersionResult, Server, SnapshotUrgency, NIL_VERSION_ID,
    };
    use tempfile::TempDir;
    use uuid::Uuid;

    fn make_bridge(tmp: &TempDir) -> SyncBridgeServer {
        let storage = SyncStorage::open(tmp.path()).unwrap();
        let client_id = Uuid::new_v4();
        let cryptor = SyncCryptor::new(client_id, b"test-secret").unwrap();
        SyncBridgeServer::new(storage, cryptor)
    }

    #[tokio::test]
    async fn test_bridge_add_and_get_version() {
        let tmp = TempDir::new().unwrap();
        let mut bridge = make_bridge(&tmp);

        let plaintext = b"some history operations".to_vec();
        let (result, _urgency) = bridge
            .add_version(NIL_VERSION_ID, plaintext.clone())
            .await
            .unwrap();

        let version_id = match result {
            AddVersionResult::Ok(vid) => vid,
            AddVersionResult::ExpectedParentVersion(_) => panic!("should have accepted version"),
        };

        // Retrieve through the bridge — should get plaintext back
        let child = bridge.get_child_version(NIL_VERSION_ID).await.unwrap();
        match child {
            GetVersionResult::Version {
                version_id: vid,
                parent_version_id: pvid,
                history_segment,
            } => {
                assert_eq!(vid, version_id);
                assert_eq!(pvid, NIL_VERSION_ID);
                assert_eq!(history_segment, plaintext);
            }
            GetVersionResult::NoSuchVersion => panic!("expected version, got NoSuchVersion"),
        }
    }

    #[tokio::test]
    async fn test_bridge_version_conflict() {
        let tmp = TempDir::new().unwrap();
        let mut bridge = make_bridge(&tmp);

        // Add first version
        let (result, _) = bridge
            .add_version(NIL_VERSION_ID, b"v1".to_vec())
            .await
            .unwrap();
        let v1 = match result {
            AddVersionResult::Ok(vid) => vid,
            _ => panic!("first version should succeed"),
        };

        // Try adding with same parent (NIL) — should conflict
        let (result, _) = bridge
            .add_version(NIL_VERSION_ID, b"v2-conflict".to_vec())
            .await
            .unwrap();
        match result {
            AddVersionResult::ExpectedParentVersion(expected) => {
                assert_eq!(expected, v1, "should expect parent to be v1");
            }
            AddVersionResult::Ok(_) => panic!("should have been a conflict"),
        }
    }

    #[tokio::test]
    async fn test_bridge_snapshot_round_trip() {
        let tmp = TempDir::new().unwrap();
        let mut bridge = make_bridge(&tmp);

        // Add a version first (snapshot needs a valid version_id)
        let (result, _) = bridge
            .add_version(NIL_VERSION_ID, b"history".to_vec())
            .await
            .unwrap();
        let version_id = match result {
            AddVersionResult::Ok(vid) => vid,
            _ => panic!("version add should succeed"),
        };

        let snapshot_data = b"full task database snapshot".to_vec();
        bridge
            .add_snapshot(version_id, snapshot_data.clone())
            .await
            .unwrap();

        let snap = bridge.get_snapshot().await.unwrap();
        assert!(snap.is_some());
        let (vid, data) = snap.unwrap();
        assert_eq!(vid, version_id);
        assert_eq!(data, snapshot_data);
    }

    #[tokio::test]
    async fn test_bridge_encrypted_storage() {
        let tmp = TempDir::new().unwrap();
        let mut bridge = make_bridge(&tmp);

        let plaintext = b"this should be encrypted in storage".to_vec();
        bridge
            .add_version(NIL_VERSION_ID, plaintext.clone())
            .await
            .unwrap();

        // Read raw storage directly (bypassing bridge decryption)
        let raw_storage = SyncStorage::open(tmp.path()).unwrap();
        let (child, _, _) = raw_storage
            .get_child_version_with_context(NIL_VERSION_ID)
            .unwrap();
        let (_, _, raw_data) = child.expect("should have a child version");

        // Raw data should NOT equal plaintext (it's encrypted)
        assert_ne!(raw_data, plaintext, "stored data should be encrypted");

        // Raw data should start with envelope version byte
        assert_eq!(raw_data[0], 1, "envelope should start with version 1");

        // Raw data should be larger than plaintext (version + nonce + tag overhead)
        assert!(
            raw_data.len() > plaintext.len(),
            "encrypted data should be larger than plaintext"
        );
    }

    #[tokio::test]
    async fn test_bridge_empty_returns_no_version() {
        let tmp = TempDir::new().unwrap();
        let mut bridge = make_bridge(&tmp);

        let result = bridge.get_child_version(NIL_VERSION_ID).await.unwrap();
        assert_eq!(result, GetVersionResult::NoSuchVersion);
    }

    #[tokio::test]
    async fn test_bridge_no_snapshot_returns_none() {
        let tmp = TempDir::new().unwrap();
        let mut bridge = make_bridge(&tmp);

        let snap = bridge.get_snapshot().await.unwrap();
        assert!(snap.is_none());
    }

    #[tokio::test]
    async fn test_bridge_snapshot_urgency() {
        let tmp = TempDir::new().unwrap();
        let mut bridge = make_bridge(&tmp);

        // First version — urgency should be None (only 1 version, no snapshot)
        let (result, urgency) = bridge
            .add_version(NIL_VERSION_ID, b"v1".to_vec())
            .await
            .unwrap();
        assert_eq!(urgency, SnapshotUrgency::None);
        let mut parent = match result {
            AddVersionResult::Ok(vid) => vid,
            _ => panic!("should succeed"),
        };

        // Add versions up to LOW threshold
        for i in 1..SNAPSHOT_URGENCY_LOW {
            let (result, _) = bridge
                .add_version(parent, format!("v{}", i + 1).into_bytes())
                .await
                .unwrap();
            parent = match result {
                AddVersionResult::Ok(vid) => vid,
                _ => panic!("should succeed at version {}", i + 1),
            };
        }

        // Next version should trigger Low urgency
        let (result, urgency) = bridge
            .add_version(parent, b"trigger-low".to_vec())
            .await
            .unwrap();
        assert_eq!(urgency, SnapshotUrgency::Low);
        let _ = match result {
            AddVersionResult::Ok(vid) => vid,
            _ => panic!("should succeed"),
        };
    }
}
