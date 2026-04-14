//! Integration tests for the admin CLI backup/restore roundtrip.
//!
//! Tests that backup creates the expected files and restore copies them
//! back with data integrity preserved (users, tokens, replica directories).

use std::sync::Arc;

use tempfile::TempDir;
use uuid::Uuid;

use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;

/// Test 6: Backup/restore roundtrip preserves users, tokens, and replica directories.
///
/// This test exercises `run_backup` and `run_restore` from admin/cli.rs.
/// Those functions are `async fn` (not pub) — they're module-private.
/// Instead, we replicate the core logic: create data, copy it (simulating backup),
/// and verify the restored data is intact.
///
/// For a full end-to-end backup/restore test via the CLI binary, use:
///   cargo run -- admin backup --output /tmp/backups
///   cargo run -- admin restore --input /tmp/backups/backup-XXXXX
#[tokio::test]
async fn test_backup_restore_roundtrip() {
    // --- Setup: create a "live" data_dir with users, tokens, and replica dirs ---
    let live_tmp = TempDir::new().unwrap();
    let live_dir = live_tmp.path();
    std::fs::create_dir_all(live_dir.join("users")).unwrap();

    let db_path = live_dir.join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    // Create a user + token
    let user = store
        .create_user(&NewUser {
            username: "backup-test-user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let token = store
        .create_api_token(&user.id, Some("backup-test"))
        .await
        .unwrap();

    // Create a sync client
    let client_id = Uuid::new_v4().to_string();
    store
        .create_replica(&user.id, &client_id, "backup-test-enc-secret")
        .await
        .unwrap();
    store
        .create_device(&user.id, &client_id, "Test device", None)
        .await
        .unwrap();

    // Create the user's replica directory with a dummy file (simulating TC data)
    let user_replica_dir = live_dir.join("users").join(&user.id);
    std::fs::create_dir_all(&user_replica_dir).unwrap();
    std::fs::write(
        user_replica_dir.join("taskchampion.sqlite3"),
        b"dummy-tc-data",
    )
    .unwrap();

    // Checkpoint WAL for a clean copy (same as run_backup does)
    {
        let db_str = db_path.to_string_lossy().to_string();
        tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open(&db_str).unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
        })
        .await
        .unwrap();
    }

    // --- Backup: copy config.sqlite and users/ to a backup directory ---
    let backup_tmp = TempDir::new().unwrap();
    let backup_dir = backup_tmp.path().join("backup-test");
    std::fs::create_dir_all(&backup_dir).unwrap();

    // Copy config database
    std::fs::copy(
        live_dir.join("config.sqlite"),
        backup_dir.join("config.sqlite"),
    )
    .unwrap();

    // Copy WAL/SHM if they exist
    for suffix in &["-wal", "-shm"] {
        let src = live_dir.join(format!("config.sqlite{suffix}"));
        if src.exists() {
            std::fs::copy(&src, backup_dir.join(format!("config.sqlite{suffix}"))).unwrap();
        }
    }

    // Copy user replica directories
    let backup_users = backup_dir.join("users");
    std::fs::create_dir_all(&backup_users).unwrap();
    for entry in std::fs::read_dir(live_dir.join("users")).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            let dest = backup_users.join(entry.file_name());
            copy_dir_recursive(&entry.path(), &dest);
        }
    }

    // Verify backup contains expected files
    assert!(
        backup_dir.join("config.sqlite").exists(),
        "Backup should contain config.sqlite"
    );
    assert!(
        backup_dir.join("users").exists(),
        "Backup should contain users/ directory"
    );
    assert!(
        backup_dir
            .join("users")
            .join(&user.id)
            .join("taskchampion.sqlite3")
            .exists(),
        "Backup should contain user's replica data"
    );

    // --- Restore: copy backup to a fresh data_dir ---
    let restore_tmp = TempDir::new().unwrap();
    let restore_dir = restore_tmp.path();
    std::fs::create_dir_all(restore_dir.join("users")).unwrap();

    // Copy config database
    std::fs::copy(
        backup_dir.join("config.sqlite"),
        restore_dir.join("config.sqlite"),
    )
    .unwrap();

    // Copy WAL/SHM if they exist
    for suffix in &["-wal", "-shm"] {
        let src = backup_dir.join(format!("config.sqlite{suffix}"));
        if src.exists() {
            std::fs::copy(&src, restore_dir.join(format!("config.sqlite{suffix}"))).unwrap();
        }
    }

    // Copy user replica directories
    for entry in std::fs::read_dir(backup_dir.join("users")).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            let dest = restore_dir.join("users").join(entry.file_name());
            copy_dir_recursive(&entry.path(), &dest);
        }
    }

    // --- Verification: open restored config DB and check data integrity ---
    let restored_db_path = restore_dir.join("config.sqlite");
    let restored_store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&restored_db_path.to_string_lossy())
            .await
            .unwrap(),
    );

    // User still exists
    let restored_user = restored_store
        .get_user_by_id(&user.id)
        .await
        .unwrap()
        .expect("User should exist in restored database");
    assert_eq!(restored_user.username, "backup-test-user");

    // Token still resolves to the user
    let resolved_user = restored_store
        .get_user_by_token(&token)
        .await
        .unwrap()
        .expect("Token should resolve to user in restored database");
    assert_eq!(resolved_user.id, user.id);

    // Sync client still exists
    let resolved_sync_user = restored_store
        .get_user_by_client_id(&client_id)
        .await
        .unwrap()
        .expect("Sync client should resolve to user in restored database");
    assert_eq!(resolved_sync_user.id, user.id);

    // User's replica directory exists with data
    let restored_replica_dir = restore_dir.join("users").join(&user.id);
    assert!(
        restored_replica_dir.exists(),
        "Restored user replica directory should exist"
    );
    assert!(
        restored_replica_dir.join("taskchampion.sqlite3").exists(),
        "Restored user should have taskchampion.sqlite3"
    );
    let restored_data = std::fs::read(restored_replica_dir.join("taskchampion.sqlite3")).unwrap();
    assert_eq!(
        restored_data, b"dummy-tc-data",
        "Restored replica data should match original"
    );
}

/// Recursively copy a directory (simplified version for tests).
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let dest_path = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path);
        } else {
            std::fs::copy(entry.path(), &dest_path).unwrap();
        }
    }
}
