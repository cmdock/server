use super::*;
use tempfile::TempDir;

fn open_temp() -> (TempDir, SyncStorage) {
    let tmp = TempDir::new().unwrap();
    let storage = SyncStorage::open(tmp.path()).unwrap();
    (tmp, storage)
}

#[test]
fn test_empty_latest_version() {
    let (_tmp, storage) = open_temp();
    assert_eq!(storage.get_latest_version_id().unwrap(), NIL_VERSION_ID);
}

#[test]
fn test_new_db_sets_schema_version_metadata() {
    let (_tmp, storage) = open_temp();
    let schema_version: i64 = storage
        .conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(schema_version, SyncStorage::current_schema_version());
}

#[test]
fn test_add_first_version() {
    let (_tmp, storage) = open_temp();
    let result = storage.add_version(NIL_VERSION_ID, b"hello").unwrap();
    assert!(result.is_ok());
    let vid = result.unwrap();
    assert_ne!(vid, NIL_VERSION_ID);
    assert_eq!(storage.get_latest_version_id().unwrap(), vid);
}

#[test]
fn test_add_version_chain() {
    let (_tmp, storage) = open_temp();
    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    let v2 = storage.add_version(v1, b"v2").unwrap().unwrap();
    assert_eq!(storage.get_latest_version_id().unwrap(), v2);
}

#[test]
fn test_add_version_conflict() {
    let (_tmp, storage) = open_temp();
    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    let wrong_parent = Uuid::new_v4();
    let result = storage.add_version(wrong_parent, b"v2").unwrap();
    assert_eq!(result, Err(v1));
}

#[test]
fn test_unique_parent_constraint() {
    let (_tmp, storage) = open_temp();
    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    let _v2 = storage.add_version(v1, b"v2").unwrap().unwrap();
    // parent_version_id has UNIQUE constraint — inserting another child of v1 would fail
    // (but add_version checks latest first, so it would return conflict instead)
}

#[test]
fn test_get_child_version() {
    let (_tmp, storage) = open_temp();
    let v1 = storage
        .add_version(NIL_VERSION_ID, b"data1")
        .unwrap()
        .unwrap();

    let (child, parent_known, has_versions) = storage
        .get_child_version_with_context(NIL_VERSION_ID)
        .unwrap();
    assert!(child.is_some());
    assert!(parent_known);
    assert!(has_versions);
    let (vid, pvid, data) = child.unwrap();
    assert_eq!(vid, v1);
    assert_eq!(pvid, NIL_VERSION_ID);
    assert_eq!(data, b"data1");
}

#[test]
fn test_get_child_version_not_found() {
    let (_tmp, storage) = open_temp();
    let (child, parent_known, has_versions) = storage
        .get_child_version_with_context(Uuid::new_v4())
        .unwrap();
    assert!(child.is_none());
    assert!(!parent_known);
    assert!(!has_versions);
}

#[test]
fn test_parent_is_known() {
    let (_tmp, storage) = open_temp();
    assert!(storage.parent_is_known(NIL_VERSION_ID).unwrap());
    assert!(!storage.parent_is_known(Uuid::new_v4()).unwrap());

    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    assert!(storage.parent_is_known(v1).unwrap());
}

#[test]
fn test_snapshot_validates_version() {
    let (_tmp, storage) = open_temp();
    // Can't add snapshot for nonexistent version
    assert!(!storage.add_snapshot(Uuid::new_v4(), b"snap").unwrap());

    // Can add for existing version
    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    assert!(storage.add_snapshot(v1, b"snap").unwrap());

    let (snap_vid, snap_data) = storage.get_snapshot().unwrap().unwrap();
    assert_eq!(snap_vid, v1);
    assert_eq!(snap_data, b"snap");
}

#[test]
fn test_snapshot_rollback_rejected() {
    let (_tmp, storage) = open_temp();
    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    let v2 = storage.add_version(v1, b"v2").unwrap().unwrap();

    // Snapshot at v2
    assert!(storage.add_snapshot(v2, b"snap-v2").unwrap());

    // Try to snapshot at v1 (older) — should be rejected
    assert!(!storage.add_snapshot(v1, b"snap-v1-rollback").unwrap());

    // Current snapshot should still be v2
    let (vid, _) = storage.get_snapshot().unwrap().unwrap();
    assert_eq!(vid, v2);
}

#[test]
fn test_snapshot_replaces_old() {
    let (_tmp, storage) = open_temp();
    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    let v2 = storage.add_version(v1, b"v2").unwrap().unwrap();

    storage.add_snapshot(v1, b"snap1").unwrap();
    storage.add_snapshot(v2, b"snap2").unwrap();

    let (vid, data) = storage.get_snapshot().unwrap().unwrap();
    assert_eq!(vid, v2);
    assert_eq!(data, b"snap2");
}

#[test]
fn test_versions_since_snapshot() {
    let (_tmp, storage) = open_temp();
    assert_eq!(storage.versions_since_snapshot().unwrap(), 0);

    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    assert_eq!(storage.versions_since_snapshot().unwrap(), 1);

    let v2 = storage.add_version(v1, b"v2").unwrap().unwrap();
    assert_eq!(storage.versions_since_snapshot().unwrap(), 2);

    // Snapshot at v1 — only v2 is after it
    storage.add_snapshot(v1, b"snap").unwrap();
    assert_eq!(storage.versions_since_snapshot().unwrap(), 1);

    // Snapshot at v2 — nothing after it
    storage.add_snapshot(v2, b"snap2").unwrap();
    assert_eq!(storage.versions_since_snapshot().unwrap(), 0);
}

#[test]
fn test_gc_retains_versions_after_latest_snapshot_for_fresh_clone_replay() {
    let (_tmp, storage) = open_temp();
    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    let v2 = storage.add_version(v1, b"v2").unwrap().unwrap();
    let v3 = storage.add_version(v2, b"v3").unwrap().unwrap();

    storage.add_snapshot(v2, b"snapshot-at-v2").unwrap();
    assert_eq!(storage.garbage_collect_older_than_snapshot(0).unwrap(), 2);

    assert!(!storage.version_exists(v1).unwrap());
    assert!(!storage.version_exists(v2).unwrap());
    assert!(storage.version_exists(v3).unwrap());
    assert_eq!(storage.get_snapshot().unwrap().unwrap().0, v2);

    // A fresh clone starts from the snapshot version and can still replay
    // the retained child even though the snapshot's own version row was GC'd.
    let (child, parent_known, has_versions) = storage.get_child_version_with_context(v2).unwrap();
    assert!(
        parent_known,
        "snapshot version remains a known protocol base"
    );
    assert!(has_versions);
    let (child_id, parent_id, body) = child.unwrap();
    assert_eq!(child_id, v3);
    assert_eq!(parent_id, v2);
    assert_eq!(body, b"v3");
}

#[test]
fn test_gc_snapshot_parent_without_child_is_up_to_date_not_gone() {
    let (_tmp, storage) = open_temp();
    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    let v2 = storage.add_version(v1, b"v2").unwrap().unwrap();

    storage.add_snapshot(v2, b"snapshot-at-tip").unwrap();
    assert_eq!(storage.garbage_collect_older_than_snapshot(0).unwrap(), 2);

    let (child, parent_known, has_versions) = storage.get_child_version_with_context(v2).unwrap();
    assert!(child.is_none());
    assert!(
        parent_known,
        "the snapshot version remains a known protocol base"
    );
    assert!(has_versions);
}

#[test]
fn test_gc_reports_gone_for_stale_parent_before_retained_history() {
    let (_tmp, storage) = open_temp();
    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    let v2 = storage.add_version(v1, b"v2").unwrap().unwrap();
    let _v3 = storage.add_version(v2, b"v3").unwrap().unwrap();

    storage.add_snapshot(v2, b"snapshot-at-v2").unwrap();
    storage.garbage_collect_older_than_snapshot(0).unwrap();

    let (child, parent_known, has_versions) = storage.get_child_version_with_context(v1).unwrap();
    assert!(child.is_none());
    assert!(!parent_known);
    assert!(
        has_versions,
        "snapshot metadata keeps the chain non-empty for 410 semantics"
    );
}

#[test]
fn test_gc_retains_delete_or_corrective_versions_until_post_change_snapshot() {
    let (_tmp, storage) = open_temp();
    let v1 = storage
        .add_version(NIL_VERSION_ID, b"create")
        .unwrap()
        .unwrap();
    let v2 = storage
        .add_version(v1, b"delete-or-corrective")
        .unwrap()
        .unwrap();

    storage.add_snapshot(v1, b"pre-delete-snapshot").unwrap();
    assert_eq!(storage.garbage_collect_older_than_snapshot(0).unwrap(), 1);
    assert!(!storage.version_exists(v1).unwrap());
    assert!(
        storage.version_exists(v2).unwrap(),
        "post-snapshot delete/corrective version must be retained for replay"
    );

    storage.add_snapshot(v2, b"post-delete-snapshot").unwrap();
    assert_eq!(storage.garbage_collect_older_than_snapshot(0).unwrap(), 1);
    assert!(
        !storage.version_exists(v2).unwrap(),
        "once a post-delete/correction snapshot exists, that version may be pruned"
    );
}

#[test]
fn test_default_gc_keeps_ten_thousand_version_tail() {
    let (_tmp, storage) = open_temp();
    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    let v2 = storage.add_version(v1, b"v2").unwrap().unwrap();
    storage.add_snapshot(v2, b"snapshot").unwrap();

    assert_eq!(storage.garbage_collect_with_default_retention().unwrap(), 0);
    assert!(storage.version_exists(v1).unwrap());
    assert!(storage.version_exists(v2).unwrap());
}

#[test]
fn test_latest_seq_metadata_tracks_correctly() {
    let (_tmp, storage) = open_temp();

    // After first version, latest_seq metadata should be 1
    let _v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    let seq: i64 = storage
        .conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'latest_seq'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(seq, 1);

    // After second version, latest_seq should be 2
    let v1 = storage.get_latest_version_id().unwrap();
    let _v2 = storage.add_version(v1, b"v2").unwrap().unwrap();
    let seq: i64 = storage
        .conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'latest_seq'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(seq, 2);
}

#[test]
fn test_reopen_preserves_seq_metadata() {
    let tmp = TempDir::new().unwrap();

    // Create some versions
    {
        let storage = SyncStorage::open(tmp.path()).unwrap();
        let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
        let _v2 = storage.add_version(v1, b"v2").unwrap().unwrap();
    }

    // Reopen — seq should continue from where it left off
    {
        let storage = SyncStorage::open(tmp.path()).unwrap();
        let v2 = storage.get_latest_version_id().unwrap();
        let _v3 = storage.add_version(v2, b"v3").unwrap().unwrap();
        let seq: i64 = storage
            .conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'latest_seq'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(seq, 3);
    }
}

#[test]
fn test_migration_runs_independently_per_table() {
    // Simulate a partially migrated DB: versions has seq but snapshots doesn't.
    // This can happen if a crash occurs between the two ALTER TABLE statements.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sync.sqlite");

    // Create DB with seq on versions but NOT on snapshots
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
                 CREATE TABLE versions (
                     version_id BLOB PRIMARY KEY,
                     parent_version_id BLOB NOT NULL UNIQUE,
                     history_segment BLOB NOT NULL,
                     seq INTEGER NOT NULL
                 );
                 CREATE TABLE snapshots (
                     id INTEGER PRIMARY KEY CHECK(id = 1),
                     version_id BLOB NOT NULL,
                     snapshot BLOB NOT NULL
                 );
                 CREATE TABLE metadata (
                     key TEXT PRIMARY KEY,
                     value BLOB NOT NULL
                 );",
        )
        .unwrap();
    }

    // Opening should detect and fix the missing snapshots.seq column
    let storage = SyncStorage::open(tmp.path()).unwrap();

    // Verify we can add a version and snapshot (snapshot uses seq column)
    let v1 = storage.add_version(NIL_VERSION_ID, b"v1").unwrap().unwrap();
    assert!(storage.add_snapshot(v1, b"snap").unwrap());

    let (vid, data) = storage.get_snapshot().unwrap().unwrap();
    assert_eq!(vid, v1);
    assert_eq!(data, b"snap");
}

#[test]
fn test_exists_queries_on_empty_db() {
    let (_tmp, storage) = open_temp();
    // All existence checks should work on empty DB without errors
    assert!(!storage.has_versions().unwrap());
    assert!(!storage.version_exists(Uuid::new_v4()).unwrap());
    assert!(storage.parent_is_known(NIL_VERSION_ID).unwrap());
    assert!(!storage.parent_is_known(Uuid::new_v4()).unwrap());
}

// --- Dirty/corrupt DB migration tests ---

/// Helper: create a DB with the pre-seq schema (no seq column, no metadata table)
fn create_legacy_db(path: &std::path::Path) {
    let db_path = path.join("sync.sqlite");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
             CREATE TABLE versions (
                 version_id BLOB PRIMARY KEY CHECK(length(version_id) = 16),
                 parent_version_id BLOB NOT NULL UNIQUE CHECK(length(parent_version_id) = 16),
                 history_segment BLOB NOT NULL,
                 CHECK(version_id != parent_version_id)
             );
             CREATE TABLE snapshots (
                 id INTEGER PRIMARY KEY CHECK(id = 1),
                 version_id BLOB NOT NULL CHECK(length(version_id) = 16),
                 snapshot BLOB NOT NULL
             );
             CREATE TABLE metadata (
                 key TEXT PRIMARY KEY,
                 value BLOB NOT NULL
             );",
    )
    .unwrap();
}

/// Insert a version row into a legacy DB (no seq column yet).
fn insert_legacy_version(
    conn: &rusqlite::Connection,
    version_id: Uuid,
    parent_id: Uuid,
    data: &[u8],
) {
    conn.execute(
        "INSERT INTO versions (version_id, parent_version_id, history_segment) VALUES (?1, ?2, ?3)",
        params![
            version_id.as_bytes().as_slice(),
            parent_id.as_bytes().as_slice(),
            data
        ],
    )
    .unwrap();
}

#[test]
fn test_migration_backfills_seq_for_nil_rooted_chain() {
    let tmp = TempDir::new().unwrap();
    create_legacy_db(tmp.path());

    let db_path = tmp.path().join("sync.sqlite");
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    // Add seq column with DEFAULT 0 (simulates ALTER TABLE migration)
    conn.execute_batch("ALTER TABLE versions ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;")
        .unwrap();

    // Insert a 3-version chain: NIL → v1 → v2 → v3, all with seq=0
    let v1 = Uuid::new_v4();
    let v2 = Uuid::new_v4();
    let v3 = Uuid::new_v4();
    conn.execute(
            "INSERT INTO versions (version_id, parent_version_id, history_segment, seq) VALUES (?1, ?2, ?3, 0)",
            params![v1.as_bytes().as_slice(), NIL_VERSION_ID.as_bytes().as_slice(), b"d1"],
        ).unwrap();
    conn.execute(
            "INSERT INTO versions (version_id, parent_version_id, history_segment, seq) VALUES (?1, ?2, ?3, 0)",
            params![v2.as_bytes().as_slice(), v1.as_bytes().as_slice(), b"d2"],
        ).unwrap();
    conn.execute(
            "INSERT INTO versions (version_id, parent_version_id, history_segment, seq) VALUES (?1, ?2, ?3, 0)",
            params![v3.as_bytes().as_slice(), v2.as_bytes().as_slice(), b"d3"],
        ).unwrap();
    drop(conn);

    // Open with SyncStorage — triggers migration
    let storage = SyncStorage::open(tmp.path()).unwrap();

    // Verify seq values are sequential
    let seqs: Vec<i64> = vec![
        storage
            .conn
            .query_row(
                "SELECT seq FROM versions WHERE version_id = ?1",
                params![v1.as_bytes().as_slice()],
                |r| r.get(0),
            )
            .unwrap(),
        storage
            .conn
            .query_row(
                "SELECT seq FROM versions WHERE version_id = ?1",
                params![v2.as_bytes().as_slice()],
                |r| r.get(0),
            )
            .unwrap(),
        storage
            .conn
            .query_row(
                "SELECT seq FROM versions WHERE version_id = ?1",
                params![v3.as_bytes().as_slice()],
                |r| r.get(0),
            )
            .unwrap(),
    ];
    assert_eq!(seqs, vec![1, 2, 3]);

    // latest_seq metadata should be backfilled
    let latest_seq: i64 = storage
        .conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'latest_seq'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(latest_seq, 3);

    let schema_version: i64 = storage
        .conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(schema_version, SyncStorage::current_schema_version());

    // New version should get seq=4
    let v4 = storage.add_version(v3, b"d4").unwrap().unwrap();
    let v4_seq: i64 = storage
        .conn
        .query_row(
            "SELECT seq FROM versions WHERE version_id = ?1",
            params![v4.as_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v4_seq, 4);
}

#[test]
fn test_migration_snapshot_seq_backfilled_independently() {
    // Scenario: versions already have proper seq, but snapshots.seq = 0
    // (crash between version backfill and snapshot backfill on previous open)
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sync.sqlite");

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
                 CREATE TABLE versions (
                     version_id BLOB PRIMARY KEY CHECK(length(version_id) = 16),
                     parent_version_id BLOB NOT NULL UNIQUE CHECK(length(parent_version_id) = 16),
                     history_segment BLOB NOT NULL,
                     seq INTEGER NOT NULL
                 );
                 CREATE TABLE snapshots (
                     id INTEGER PRIMARY KEY CHECK(id = 1),
                     version_id BLOB NOT NULL CHECK(length(version_id) = 16),
                     snapshot BLOB NOT NULL,
                     seq INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE metadata (
                     key TEXT PRIMARY KEY,
                     value BLOB NOT NULL
                 );",
        )
        .unwrap();

        // Insert versions with proper seq
        let v1 = Uuid::new_v4();
        let v2 = Uuid::new_v4();
        conn.execute(
            "INSERT INTO versions VALUES (?1, ?2, ?3, 1)",
            params![
                v1.as_bytes().as_slice(),
                NIL_VERSION_ID.as_bytes().as_slice(),
                b"d1"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO versions VALUES (?1, ?2, ?3, 2)",
            params![v2.as_bytes().as_slice(), v1.as_bytes().as_slice(), b"d2"],
        )
        .unwrap();

        // Insert snapshot referencing v2 but with seq=0 (simulates crash)
        conn.execute(
            "INSERT INTO snapshots (id, version_id, snapshot, seq) VALUES (1, ?1, ?2, 0)",
            params![v2.as_bytes().as_slice(), b"snap"],
        )
        .unwrap();

        // Metadata already has latest_seq
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('latest_seq', '2')",
            [],
        )
        .unwrap();
    }

    // Open — should backfill snapshot seq even though versions are fine
    let storage = SyncStorage::open(tmp.path()).unwrap();
    let snap_seq: i64 = storage
        .conn
        .query_row("SELECT seq FROM snapshots WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(snap_seq, 2); // Should match v2's seq

    // versions_since_snapshot should be 0 (latest_seq=2, snap_seq=2)
    assert_eq!(storage.versions_since_snapshot().unwrap(), 0);
}

#[test]
fn test_migration_snapshot_missing_version_does_not_crash() {
    // Scenario: snapshot references a version that doesn't exist (corruption).
    // Migration should not crash — should leave seq at 0 and warn.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sync.sqlite");

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
                 CREATE TABLE versions (
                     version_id BLOB PRIMARY KEY CHECK(length(version_id) = 16),
                     parent_version_id BLOB NOT NULL UNIQUE CHECK(length(parent_version_id) = 16),
                     history_segment BLOB NOT NULL,
                     seq INTEGER NOT NULL
                 );
                 CREATE TABLE snapshots (
                     id INTEGER PRIMARY KEY CHECK(id = 1),
                     version_id BLOB NOT NULL CHECK(length(version_id) = 16),
                     snapshot BLOB NOT NULL,
                     seq INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE metadata (
                     key TEXT PRIMARY KEY,
                     value BLOB NOT NULL
                 );",
        )
        .unwrap();

        // Snapshot references a version that doesn't exist
        let missing_vid = Uuid::new_v4();
        conn.execute(
            "INSERT INTO snapshots (id, version_id, snapshot, seq) VALUES (1, ?1, ?2, 0)",
            params![missing_vid.as_bytes().as_slice(), b"orphan-snap"],
        )
        .unwrap();
    }

    // Should NOT panic — open succeeds despite corrupt snapshot reference
    let storage = SyncStorage::open(tmp.path()).unwrap();

    // Snapshot seq should remain 0 (COALESCE fallback)
    let snap_seq: i64 = storage
        .conn
        .query_row("SELECT seq FROM snapshots WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(snap_seq, 0);
}

#[test]
fn test_migration_orphaned_root_chain() {
    // Scenario: chain root has a non-NIL parent that doesn't exist (corruption)
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sync.sqlite");

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
                 CREATE TABLE versions (
                     version_id BLOB PRIMARY KEY CHECK(length(version_id) = 16),
                     parent_version_id BLOB NOT NULL UNIQUE CHECK(length(parent_version_id) = 16),
                     history_segment BLOB NOT NULL,
                     seq INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE snapshots (
                     id INTEGER PRIMARY KEY CHECK(id = 1),
                     version_id BLOB NOT NULL CHECK(length(version_id) = 16),
                     snapshot BLOB NOT NULL,
                     seq INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE metadata (
                     key TEXT PRIMARY KEY,
                     value BLOB NOT NULL
                 );",
        )
        .unwrap();

        // Chain: missing_parent → v1 → v2 (orphaned root)
        let missing_parent = Uuid::new_v4();
        let v1 = Uuid::new_v4();
        let v2 = Uuid::new_v4();
        conn.execute(
            "INSERT INTO versions VALUES (?1, ?2, ?3, 0)",
            params![
                v1.as_bytes().as_slice(),
                missing_parent.as_bytes().as_slice(),
                b"d1"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO versions VALUES (?1, ?2, ?3, 0)",
            params![v2.as_bytes().as_slice(), v1.as_bytes().as_slice(), b"d2"],
        )
        .unwrap();
    }

    // Should open successfully and backfill the orphaned chain
    let storage = SyncStorage::open(tmp.path()).unwrap();

    // Both should have non-zero seq values
    let zero_count: i64 = storage
        .conn
        .query_row("SELECT COUNT(*) FROM versions WHERE seq = 0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(zero_count, 0, "All versions should have been backfilled");

    // Seq should be sequential
    let max_seq: i64 = storage
        .conn
        .query_row("SELECT MAX(seq) FROM versions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(max_seq, 2);
}

#[test]
fn test_open_rejects_newer_unsupported_schema_version() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sync.sqlite");

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
                 CREATE TABLE versions (
                     version_id BLOB PRIMARY KEY CHECK(length(version_id) = 16),
                     parent_version_id BLOB NOT NULL UNIQUE CHECK(length(parent_version_id) = 16),
                     history_segment BLOB NOT NULL,
                     seq INTEGER NOT NULL
                 );
                 CREATE TABLE snapshots (
                     id INTEGER PRIMARY KEY CHECK(id = 1),
                     version_id BLOB NOT NULL CHECK(length(version_id) = 16),
                     snapshot BLOB NOT NULL,
                     seq INTEGER NOT NULL
                 );
                 CREATE TABLE metadata (
                     key TEXT PRIMARY KEY,
                     value BLOB NOT NULL
                 );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('schema_version', ?1)",
            params![(SyncStorage::current_schema_version() + 1).to_string()],
        )
        .unwrap();
    }

    let err = SyncStorage::open(tmp.path()).err().unwrap();
    assert!(err.to_string().contains("unsupported schema_version"));
}
