//! Per-user TaskChampion replica management.
//!
//! Each user gets their own TaskChampion SQLite database in `data/users/{user_id}/`.
//! The `ReplicaManager` caches open replicas per user to eliminate connection churn
//! and provides retry-with-jitter for SQLite BUSY errors.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use taskchampion::storage::AccessMode;
use taskchampion::{Operations, Replica, SqliteStorage, Status, Tag};
use tokio::sync::Mutex;

use crate::metrics as m;
use crate::tasks::models::TaskItem;
use crate::tasks::parser::ParsedTask;

/// Manages per-user replica connections with caching.
///
/// Instead of opening a new SQLite connection per request, replicas are
/// cached in a concurrent map. Each replica is behind a Mutex to serialise
/// access per user (matching SQLite's single-writer model).
#[derive(Clone)]
pub struct ReplicaManager {
    replicas: Arc<DashMap<String, Arc<Mutex<Replica<SqliteStorage>>>>>,
    opening: Arc<DashMap<String, Arc<Mutex<()>>>>,
    data_dir: PathBuf,
}

impl ReplicaManager {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            replicas: Arc::new(DashMap::new()),
            opening: Arc::new(DashMap::new()),
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// Get a cached replica for the user, or open a new one.
    ///
    /// Uses per-key locking via DashMap::entry to prevent the race where
    /// two concurrent first-requests for the same user both open connections.
    pub async fn get_replica(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Arc<Mutex<Replica<SqliteStorage>>>> {
        // Validate user_id to prevent path traversal
        if user_id.contains('/')
            || user_id.contains('\\')
            || user_id.contains("..")
            || user_id.is_empty()
        {
            anyhow::bail!("Invalid user_id: {user_id}");
        }

        // Fast path: return cached replica (read-only DashMap lookup)
        if let Some(replica) = self.replicas.get(user_id) {
            return Ok(replica.value().clone());
        }

        // Slow path: open new replica. Serialize the first-open path per user
        // so SQLite schema creation cannot race under a cold-start stampede.
        let open_lock = self
            .opening
            .entry(user_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .value()
            .clone();
        let _open_guard = open_lock.lock().await;

        if let Some(replica) = self.replicas.get(user_id) {
            return Ok(replica.value().clone());
        }

        let user_dir = self.data_dir.join("users").join(user_id);
        let start = Instant::now();
        tokio::fs::create_dir_all(&user_dir).await?;
        let storage = SqliteStorage::new(&user_dir, AccessMode::ReadWrite, true).await?;
        let replica = Replica::new(storage);
        m::record_replica_open(start.elapsed().as_secs_f64());

        let arc = Arc::new(Mutex::new(replica));
        self.replicas.insert(user_id.to_string(), arc.clone());
        self.opening.remove(user_id);
        Ok(arc)
    }

    /// Number of cached replicas (for metrics).
    pub fn replica_count(&self) -> usize {
        self.replicas.len()
    }

    /// Get user IDs of currently cached replicas (for health checks).
    /// Returns a snapshot — safe to iterate without holding any locks.
    pub fn cached_user_ids(&self) -> Vec<String> {
        self.replicas
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Check if a user's replica is currently cached.
    pub fn is_cached(&self, user_id: &str) -> bool {
        self.replicas.contains_key(user_id)
    }

    /// Evict a cached replica (for restore or reload operations).
    /// Returns true if a replica was evicted, false if it wasn't in cache.
    /// The next request for this user will open a fresh connection.
    pub fn evict(&self, user_id: &str) -> bool {
        self.opening.remove(user_id);
        self.replicas.remove(user_id).is_some()
    }
}

/// Retry an async operation with exponential backoff and jitter on failure.
///
/// Used to handle transient SQLite BUSY errors. The operation is retried
/// up to `max_retries` times with increasing delays plus random jitter
/// to prevent thundering herd.
pub async fn retry_with_jitter<F, Fut, T>(
    operation: &'static str,
    max_retries: usize,
    mut f: F,
) -> Result<T, anyhow::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, anyhow::Error>>,
{
    let mut attempt = 0;
    loop {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                let is_busy = {
                    let err_str = e.to_string();
                    err_str.contains("database is locked") || err_str.contains("SQLITE_BUSY")
                };

                if !is_busy || attempt >= max_retries {
                    return Err(e);
                }

                m::record_sqlite_busy(operation);
                attempt += 1;

                // Exponential backoff: 20ms, 40ms, 80ms + random jitter 0-10ms
                // (attempt is 1-indexed after increment, so 10 * 2^1, 10 * 2^2, ...)
                let base_delay = Duration::from_millis(10 * (1 << attempt));
                let jitter = Duration::from_millis(rand::random::<u64>() % 10);
                let delay = base_delay + jitter;

                tracing::debug!(
                    "BUSY retry {attempt}/{max_retries} for {operation}, waiting {delay:?}"
                );
                m::record_busy_retry(operation, attempt);
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Check if an error looks like SQLite contention (SQLITE_BUSY / database locked).
pub fn is_sqlite_busy(err: &dyn std::fmt::Display) -> bool {
    let err_str = err.to_string();
    err_str.contains("database is locked") || err_str.contains("SQLITE_BUSY")
}

/// Check if an error indicates SQLite contention and record the metric.
///
/// Checks for "database is locked" and "SQLITE_BUSY" in the error's Display
/// output. This is necessarily string-based because TaskChampion wraps
/// SQLite errors in its own opaque error types — we can't downcast to
/// rusqlite::Error through the wrapper.
pub fn check_busy_error(err: &impl std::fmt::Display, operation: &'static str) {
    if is_sqlite_busy(err) {
        m::record_sqlite_busy(operation);
        tracing::debug!("SQLite BUSY on {operation}");
    }
}

/// Check if any error in an `anyhow::Error` chain indicates SQLite contention.
pub fn is_busy_in_chain(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| is_sqlite_busy(cause))
}

/// Check if an error is a SQLite corruption error (SQLITE_CORRUPT, SQLITE_NOTADB).
/// These indicate the database file is damaged and the connection should be evicted.
///
/// Uses string matching because TaskChampion wraps SQLite errors in its own
/// opaque error types — we can't always downcast to `rusqlite::Error`.
pub fn is_sqlite_corruption(err: &dyn std::fmt::Display) -> bool {
    let msg = err.to_string();
    msg.contains("database disk image is malformed")
        || msg.contains("file is not a database")
        || msg.contains("SQLITE_CORRUPT")
        || msg.contains("SQLITE_NOTADB")
        // TaskChampion wraps SQLite errors opaquely — these are TC-specific
        // error messages that indicate the underlying SQLite file is corrupt.
        // "Setting journal_mode=WAL" occurs when TC can't set WAL mode on a non-DB file.
        || msg.contains("Setting journal_mode=WAL")
}

/// Check if any error in an `anyhow::Error` chain indicates SQLite corruption.
///
/// Prefers structured matching via `rusqlite::Error::SqliteFailure` when
/// available, falling back to string matching for wrapped errors.
pub fn is_corruption_in_chain(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        if let Some(sqlite_err) = cause.downcast_ref::<rusqlite::Error>() {
            matches!(
                sqlite_err,
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ffi::ErrorCode::DatabaseCorrupt
                            | rusqlite::ffi::ErrorCode::NotADatabase,
                        ..
                    },
                    _
                )
            )
        } else {
            is_sqlite_corruption(cause)
        }
    })
}

/// Convert a TaskChampion Task to our API TaskItem model.
///
/// Reads each property once to avoid redundant iterations/lookups.
pub fn task_to_item(task: &taskchampion::Task) -> TaskItem {
    let project = task.get_value("project").map(|s| s.to_string());
    let has_project = project.is_some();
    let tags: Vec<String> = task
        .get_tags()
        .filter(|t| t.is_user())
        .map(|t| t.to_string())
        .collect();
    let tag_count = tags.len();
    let priority_str = task.get_priority();
    let priority = if priority_str.is_empty() {
        None
    } else {
        Some(priority_str.to_string())
    };
    let due = task.get_due();
    let blocked = task.is_blocked();
    let waiting = task.get_wait().is_some_and(|wait| wait > Utc::now());
    let status = match task.get_status() {
        Status::Pending => "pending",
        Status::Completed => "completed",
        Status::Deleted => "deleted",
        Status::Recurring => "recurring",
        _ => "pending",
    };

    TaskItem {
        uuid: task.get_uuid().to_string(),
        description: task.get_description().to_string(),
        project,
        tags,
        priority,
        due: due.map(format_tw_date),
        urgency: crate::tasks::urgency::calculate_urgency(
            if priority_str.is_empty() {
                None
            } else {
                Some(priority_str)
            },
            due,
            tag_count,
            has_project,
        ),
        blocked,
        waiting,
        status: status.to_string(),
    }
}

/// Format a chrono DateTime to Taskwarrior date format: YYYYMMDDTHHmmssZ
fn format_tw_date(dt: DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Apply parsed task fields to a TaskChampion task.
pub fn apply_parsed_fields(
    task: &mut taskchampion::Task,
    parsed: &ParsedTask,
    ops: &mut Operations,
) -> anyhow::Result<()> {
    if !parsed.description.is_empty() {
        task.set_description(parsed.description.clone(), ops)?;
    }
    if let Some(ref project) = parsed.project {
        task.set_value("project", Some(project.clone()), ops)?;
    }
    for tag_str in &parsed.tags {
        if let Ok(tag) = Tag::try_from(tag_str.as_str()) {
            task.add_tag(&tag, ops)?;
        }
    }
    if let Some(ref priority) = parsed.priority {
        task.set_priority(priority.clone(), ops)?;
    }
    if let Some(ref due_str) = parsed.due {
        // Use the full date parser (handles named dates like "tomorrow",
        // TW format "20260401T090000Z", and ISO "2026-04-01")
        if let Some(dt) = crate::tasks::filter::dates::parse_date_value(due_str) {
            task.set_due(Some(dt), ops)?;
        }
    }
    Ok(())
}

/// Parse a Taskwarrior date string (YYYYMMDDTHHmmssZ) to a chrono DateTime.
pub fn parse_tw_date(s: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ")
        .ok()
        .map(|naive| naive.and_utc())
}

#[cfg(test)]
mod corruption_tests {
    use super::*;

    #[tokio::test]
    async fn test_tc_corruption_detection_on_garbage_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let user_dir = tmp.path().join("test-user");
        std::fs::create_dir_all(&user_dir).unwrap();

        // Write garbage to the TC database file
        std::fs::write(
            user_dir.join("taskchampion.sqlite3"),
            b"THIS IS NOT A SQLITE DATABASE FILE AT ALL",
        )
        .unwrap();

        // Try to open via TaskChampion
        let result = SqliteStorage::new(&user_dir, AccessMode::ReadWrite, true).await;
        match result {
            Ok(_) => panic!("Expected error opening corrupt TC database"),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    is_sqlite_corruption(&e),
                    "is_sqlite_corruption should detect TC corruption error: {msg}"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_tc_corruption_detection_on_truncated_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let user_dir = tmp.path().join("test-user");
        std::fs::create_dir_all(&user_dir).unwrap();

        // Create empty file (truncated)
        std::fs::write(user_dir.join("taskchampion.sqlite3"), b"").unwrap();

        let result = SqliteStorage::new(&user_dir, AccessMode::ReadWrite, true).await;
        match result {
            Ok(_) => {
                // Empty file treated as new DB by SQLite — valid outcome.
                // SQLite creates tables on first access when the file is empty.
            }
            Err(e) => {
                // If TC rejects the empty file, our detector should recognise it
                assert!(
                    is_sqlite_corruption(&e),
                    "Truncated file error should be detected as corruption: {e}"
                );
            }
        }
    }

    #[test]
    fn test_normal_error_not_detected_as_corruption() {
        // Transient/operational errors must NOT trigger corruption quarantine
        assert!(!is_sqlite_corruption(&"database is locked"));
        assert!(!is_sqlite_corruption(&"SQLITE_BUSY"));
        assert!(!is_sqlite_corruption(&"table not found"));
        assert!(!is_sqlite_corruption(&"constraint violation"));
    }

    #[test]
    fn test_is_corruption_in_chain_detects_rusqlite_error() {
        use rusqlite::ffi;
        let sqlite_err = rusqlite::Error::SqliteFailure(
            ffi::Error {
                code: ffi::ErrorCode::DatabaseCorrupt,
                extended_code: 11,
            },
            Some("database disk image is malformed".to_string()),
        );
        let anyhow_err = anyhow::Error::new(sqlite_err).context("opening database");
        assert!(is_corruption_in_chain(&anyhow_err));
    }

    #[test]
    fn test_is_corruption_in_chain_ignores_normal_errors() {
        let err = anyhow::anyhow!("database is locked");
        assert!(!is_corruption_in_chain(&err));
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn test_check_busy_error_detects_locked() {
        // Should not panic — just records a metric and logs.
        // We verify it doesn't crash on a "database is locked" message.
        let err = "database is locked";
        check_busy_error(&err, "test_op");
        // Also check SQLITE_BUSY variant
        let err2 = "SQLITE_BUSY (5)";
        check_busy_error(&err2, "test_op");
    }

    #[test]
    fn test_check_busy_error_ignores_other() {
        // Non-BUSY errors should pass through without recording busy metrics.
        // This just verifies no panic or unexpected side effect.
        let err = "table not found: tasks";
        check_busy_error(&err, "test_op");

        let err2 = "constraint violation";
        check_busy_error(&err2, "test_op");
    }

    #[test]
    fn test_format_tw_date_roundtrip() {
        let dt = chrono::Utc::now();
        let formatted = format_tw_date(dt);
        let parsed = parse_tw_date(&formatted);
        assert!(parsed.is_some(), "roundtrip should succeed");
        // Precision is seconds, so truncate the original
        let expected = dt.format("%Y%m%dT%H%M%SZ").to_string();
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_parse_tw_date_valid() {
        let result = parse_tw_date("20260329T143000Z");
        assert!(result.is_some());
        let dt = result.unwrap();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2026-03-29");
    }

    #[test]
    fn test_parse_tw_date_invalid() {
        assert!(parse_tw_date("not-a-date").is_none());
        assert!(parse_tw_date("").is_none());
    }
}
