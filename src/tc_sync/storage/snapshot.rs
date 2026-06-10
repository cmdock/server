//! Snapshot and garbage-collection operations for TaskChampion sync storage.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use super::{SyncStorage, MIN_RETAINED_VERSIONS_AFTER_GC, NIL_VERSION_ID};

impl SyncStorage {
    /// Store a snapshot for a given version. The version_id must exist in the chain,
    /// and must be at or after the current snapshot's version (no rollback).
    pub fn add_snapshot(&self, version_id: Uuid, snapshot: &[u8]) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;

        // Get the seq of the new snapshot's version
        let new_seq: Option<i64> = tx
            .query_row(
                "SELECT seq FROM versions WHERE version_id = ?1",
                params![version_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        let new_seq = match new_seq {
            Some(s) => s,
            None => return Ok(false), // version doesn't exist
        };

        // Monotonic: reject if new seq is less than current snapshot's seq
        let current_seq: Option<i64> = tx
            .query_row("SELECT seq FROM snapshots WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        // Reject rollback (older seq), allow equal (idempotent retry) and newer
        if let Some(cur) = current_seq {
            if new_seq < cur {
                return Ok(false);
            }
        }

        tx.execute(
            "INSERT OR REPLACE INTO snapshots (id, version_id, snapshot, seq) VALUES (1, ?1, ?2, ?3)",
            params![version_id.as_bytes().as_slice(), snapshot, new_seq],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Get the latest snapshot. Returns `None` if no snapshot exists.
    pub fn get_snapshot(&self) -> Result<Option<(Uuid, Vec<u8>)>> {
        let row = self
            .conn
            .query_row(
                "SELECT version_id, snapshot FROM snapshots LIMIT 1",
                [],
                |row| {
                    let vid: Vec<u8> = row.get(0)?;
                    let data: Vec<u8> = row.get(1)?;
                    Ok((vid, data))
                },
            )
            .optional()?;

        match row {
            Some((vid, data)) => {
                let version_id = Uuid::from_slice(&vid).unwrap_or(NIL_VERSION_ID);
                Ok(Some((version_id, data)))
            }
            None => Ok(None),
        }
    }

    /// Count versions added since the latest snapshot (for snapshot urgency).
    /// Uses metadata for O(1) computation: latest_seq - snapshot_seq.
    pub fn versions_since_snapshot(&self) -> Result<u64> {
        let latest_seq: i64 = self
            .conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'latest_seq'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);

        let snap_seq: i64 = self
            .conn
            .query_row("SELECT seq FROM snapshots WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?
            .unwrap_or(0);

        Ok(latest_seq.saturating_sub(snap_seq) as u64)
    }

    /// Garbage-collect history older than the latest snapshot while preserving
    /// a tail of retained versions.
    ///
    /// This is the Phase-7 merged-chain retention primitive. The latest
    /// snapshot is the authority for fresh clones; versions strictly after that
    /// snapshot must remain replayable. Therefore GC only deletes rows with
    /// `seq <= latest_snapshot_seq`, and only when those rows also fall outside
    /// the retained latest-version tail. Production callers should pass
    /// [`MIN_RETAINED_VERSIONS_AFTER_GC`]. Tests may pass a smaller value to
    /// exercise stale-client behaviour without creating thousands of versions.
    pub fn garbage_collect_older_than_snapshot(&self, min_retained_versions: u64) -> Result<u64> {
        let tx = self.conn.unchecked_transaction()?;

        let latest_seq: i64 = tx
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'latest_seq'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let snapshot_seq: Option<i64> = tx
            .query_row("SELECT seq FROM snapshots WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(snapshot_seq) = snapshot_seq else {
            return Ok(0);
        };

        let retained_floor = latest_seq.saturating_sub(min_retained_versions as i64);
        let cutoff_seq = snapshot_seq.min(retained_floor);
        if cutoff_seq <= 0 {
            return Ok(0);
        }

        let deleted = tx.execute("DELETE FROM versions WHERE seq <= ?1", params![cutoff_seq])?;
        tx.commit()?;
        Ok(deleted as u64)
    }

    /// Apply the production conservative retention policy.
    pub(super) fn garbage_collect_with_default_retention(&self) -> Result<u64> {
        self.garbage_collect_older_than_snapshot(MIN_RETAINED_VERSIONS_AFTER_GC)
    }
}
