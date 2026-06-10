//! Opening/path/schema setup helpers for TaskChampion sync storage.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{migration, SyncStorage};

pub(super) const SYNC_SCHEMA_VERSION: i64 = 1;

impl SyncStorage {
    pub fn current_schema_version() -> i64 {
        SYNC_SCHEMA_VERSION
    }

    /// Open the legacy default sync storage at `<base_dir>/sync.sqlite`.
    ///
    /// Phase 6 TC handlers do not serve this path; use `open_merged` for the
    /// gateway-backed runtime chain.
    pub fn open(user_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(user_dir)
            .with_context(|| format!("Creating user dir at {}", user_dir.display()))?;
        Self::open_at(&Self::user_db_path(user_dir))
    }

    /// Return the primary per-user sync DB path.
    fn user_db_path(user_dir: &Path) -> PathBuf {
        user_dir.join("sync.sqlite")
    }

    /// Open the merged gateway sync storage at `<user_dir>/merged/sync.sqlite`.
    pub fn open_merged(user_dir: &Path) -> Result<Self> {
        let merged_dir = user_dir.join("merged");
        std::fs::create_dir_all(&merged_dir)
            .with_context(|| format!("Creating merged sync dir at {}", merged_dir.display()))?;
        Self::open_at(&merged_dir.join("sync.sqlite"))
    }

    /// Return the merged gateway sync DB path for a user directory.
    pub fn merged_db_path(user_dir: &Path) -> PathBuf {
        user_dir.join("merged").join("sync.sqlite")
    }

    /// Open (or create) per-device sync storage for a user/device pair.
    pub(super) fn open_device(user_dir: &Path, client_id: &str) -> Result<Self> {
        let sync_dir = user_dir.join("sync");
        std::fs::create_dir_all(&sync_dir)
            .with_context(|| format!("Creating sync dir at {}", sync_dir.display()))?;
        Self::open_at(&sync_dir.join(format!("{client_id}.sqlite")))
    }

    /// Return the per-device sync DB path for a user/device pair.
    fn device_db_path(user_dir: &Path, client_id: &str) -> PathBuf {
        user_dir.join("sync").join(format!("{client_id}.sqlite"))
    }

    pub fn inspect_schema_version(db_path: &Path) -> Result<Option<i64>> {
        let conn = rusqlite::Connection::open(db_path).with_context(|| {
            format!(
                "Opening sync DB at {} for schema inspection",
                db_path.display()
            )
        })?;
        migration::read_schema_version_on(&conn)
    }

    fn open_at(db_path: &Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(db_path)
            .with_context(|| format!("Opening sync DB at {}", db_path.display()))?;

        // Busy timeout for concurrent access (5 seconds)
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS versions (
                 version_id BLOB PRIMARY KEY CHECK(length(version_id) = 16),
                 parent_version_id BLOB NOT NULL UNIQUE CHECK(length(parent_version_id) = 16),
                 history_segment BLOB NOT NULL,
                 seq INTEGER NOT NULL,
                 CHECK(version_id != parent_version_id)
             );
             CREATE TABLE IF NOT EXISTS snapshots (
                 id INTEGER PRIMARY KEY CHECK(id = 1),
                 version_id BLOB NOT NULL CHECK(length(version_id) = 16),
                 snapshot BLOB NOT NULL,
                 seq INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY,
                 value BLOB NOT NULL
             );",
        )?;

        if let Some(schema_version) = migration::read_schema_version_on(&conn)? {
            if schema_version > SYNC_SCHEMA_VERSION {
                anyhow::bail!(
                    "Sync DB at {} uses unsupported schema_version={} (current binary supports up to {})",
                    db_path.display(),
                    schema_version,
                    SYNC_SCHEMA_VERSION
                );
            }
        }

        migration::upgrade_to_v1(&conn, SYNC_SCHEMA_VERSION)?;

        Ok(Self { conn })
    }
}
