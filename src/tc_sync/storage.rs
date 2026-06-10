//! Sync storage for the TaskChampion sync protocol.
//!
//! The server stores TaskChampion protocol state in SQLite.
//!
//! The Phase-6 runtime path uses the merged gateway DB at
//! `data/users/{user_id}/merged/sync.sqlite`. The older per-user DB at
//! `data/users/{user_id}/sync.sqlite` and per-device files under
//! `data/users/{user_id}/sync/{client_id}.sqlite` are retained as lower-level
//! primitives/tests only and are not served by `/v1/client/*`.
//! The storage schema is the same regardless of where the database lives.

#[cfg(test)]
use rusqlite::params;
use uuid::Uuid;

#[path = "storage/migration.rs"]
mod migration;
#[path = "storage/open.rs"]
mod open;
#[path = "storage/ops.rs"]
mod ops;
#[path = "storage/snapshot.rs"]
mod snapshot;
#[path = "storage/version_chain.rs"]
mod version_chain;

/// NIL version ID — used as the parent of the first version in a chain.
pub const NIL_VERSION_ID: Uuid = Uuid::nil();

/// Production merged-chain GC keeps at least this many latest versions.
///
/// Phase-7 retention is intentionally conservative: there is no age-based
/// deletion in the current runtime, so history is retained indefinitely unless
/// an explicit snapshot-bounded GC pass runs. When GC does run, it keeps at
/// least 10,000 latest versions and never deletes versions after the latest
/// snapshot. This is stronger than the beta gate's 30-day-or-10,000-version
/// minimum because the implementation has no clock-based expiry path.
pub const MIN_RETAINED_VERSIONS_AFTER_GC: u64 = 10_000;

/// Sync storage backed by SQLite.
pub struct SyncStorage {
    conn: rusqlite::Connection,
}

#[cfg(test)]
#[path = "storage/tests.rs"]
mod tests;
