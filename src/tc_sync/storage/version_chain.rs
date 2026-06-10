//! Version-chain operations for TaskChampion sync storage.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use super::{ops, SyncStorage, NIL_VERSION_ID};

impl SyncStorage {
    /// Get the latest version ID (tip of the version chain).
    /// Uses metadata first; falls back to scanning the versions table if metadata
    /// is missing or corrupt (self-healing).
    pub fn get_latest_version_id(&self) -> Result<Uuid> {
        ops::find_tip_on(&self.conn, NIL_VERSION_ID)
    }

    /// Check whether a version_id exists in the chain.
    pub fn version_exists(&self, version_id: Uuid) -> Result<bool> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM versions WHERE version_id = ?1)",
            params![version_id.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        Ok(exists)
    }

    /// Add a new version to the chain. Returns `Ok(version_id)` on success,
    /// or `Err` with the expected parent version ID on conflict.
    pub fn add_version(
        &self,
        parent_version_id: Uuid,
        history_segment: &[u8],
    ) -> Result<std::result::Result<Uuid, Uuid>> {
        let tx = self.conn.unchecked_transaction()?;

        // Check linearity — parent must match current tip
        let latest = ops::find_tip_on(&tx, NIL_VERSION_ID)?;

        if latest == NIL_VERSION_ID {
            // Empty chain — first version must be rooted at NIL
            if parent_version_id != NIL_VERSION_ID {
                return Ok(Err(NIL_VERSION_ID));
            }
        } else if parent_version_id != latest {
            return Ok(Err(latest));
        }

        let version_id = Uuid::new_v4();

        // Get next seq from metadata (O(1) instead of scanning MAX(seq))
        let current_seq: i64 = tx
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'latest_seq'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let next_seq = current_seq + 1;

        tx.execute(
            "INSERT INTO versions (version_id, parent_version_id, history_segment, seq) VALUES (?1, ?2, ?3, ?4)",
            params![
                version_id.as_bytes().as_slice(),
                parent_version_id.as_bytes().as_slice(),
                history_segment,
                next_seq,
            ],
        )?;

        tx.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES ('latest_version_id', ?1)",
            params![version_id.as_bytes().as_slice()],
        )?;

        // Store seq in metadata for O(1) next-seq lookup
        tx.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES ('latest_seq', ?1)",
            params![next_seq.to_string()],
        )?;

        tx.commit()?;
        Ok(Ok(version_id))
    }

    /// Get the child version of a given parent, with context for 404 vs 410.
    ///
    /// Returns a tuple: (child_option, parent_known, has_versions)
    /// All reads are done in a single transaction for consistency.
    #[allow(clippy::type_complexity)]
    pub fn get_child_version_with_context(
        &self,
        parent_version_id: Uuid,
    ) -> Result<(Option<(Uuid, Uuid, Vec<u8>)>, bool, bool)> {
        let tx = self.conn.unchecked_transaction()?;

        let child = tx
            .query_row(
                "SELECT version_id, parent_version_id, history_segment
                 FROM versions WHERE parent_version_id = ?1",
                params![parent_version_id.as_bytes().as_slice()],
                |row| {
                    let vid: Vec<u8> = row.get(0)?;
                    let pvid: Vec<u8> = row.get(1)?;
                    let data: Vec<u8> = row.get(2)?;
                    Ok((vid, pvid, data))
                },
            )
            .optional()?;

        let has_versions: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM versions)
             OR EXISTS(SELECT 1 FROM snapshots WHERE id = 1)
             OR EXISTS(SELECT 1 FROM metadata WHERE key = 'latest_seq' AND CAST(value AS INTEGER) > 0)",
            [],
            |row| row.get(0),
        )?;

        let parent_known = if parent_version_id == NIL_VERSION_ID {
            // NIL is a valid parent for an empty chain and for the retained
            // first child. If that child was GC'd under a snapshot, treating
            // NIL as unknown lets the protocol adapter return 410 so stale
            // clients reset via snapshot instead of thinking they are current.
            !has_versions || child.is_some()
        } else {
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM versions WHERE version_id = ?1)
                 OR EXISTS(SELECT 1 FROM snapshots WHERE id = 1 AND version_id = ?1)",
                params![parent_version_id.as_bytes().as_slice()],
                |row| row.get(0),
            )?
        };

        let child_parsed = child.map(|(vid, pvid, data)| {
            let version_id = Uuid::from_slice(&vid).unwrap_or(NIL_VERSION_ID);
            let parent_id = Uuid::from_slice(&pvid).unwrap_or(NIL_VERSION_ID);
            (version_id, parent_id, data)
        });

        Ok((child_parsed, parent_known, has_versions))
    }

    /// Check if the version chain has any data (independent of metadata).
    pub fn has_versions(&self) -> Result<bool> {
        let exists: bool =
            self.conn
                .query_row("SELECT EXISTS(SELECT 1 FROM versions)", [], |row| {
                    row.get(0)
                })?;
        Ok(exists)
    }

    /// Check if a parent_version_id is known (either NIL or exists as a version_id).
    pub(super) fn parent_is_known(&self, parent_version_id: Uuid) -> Result<bool> {
        if parent_version_id == NIL_VERSION_ID {
            return Ok(true);
        }
        self.version_exists(parent_version_id)
    }
}
