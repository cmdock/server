//! Integration tests for admin CLI operations.
//!
//! Tests copy_dir_recursive (symlink skipping, empty dirs, nested dirs),
//! token revoke prefix matching (exact, prefix, ambiguous, no match),
//! backup file creation, and restore with orphan cleanup.

use std::process::Command;
use std::sync::Arc;

use cmdock_server::connect_config::decode_connect_url;
use tempfile::TempDir;

use cmdock_server::admin::cli::copy_dir_recursive;
use cmdock_server::runtime_policy::{RuntimeAccessMode, RuntimeDeleteAction, RuntimePolicy};
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;

fn admin_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cmdock-server")
}

fn run_admin(data_dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(admin_bin())
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .output()
        .unwrap()
}

fn run_admin_with_config(config_path: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(admin_bin())
        .arg("--config")
        .arg(config_path)
        .args(args)
        .output()
        .unwrap()
}

fn extract_connect_url(stdout: &str) -> String {
    stdout
        .lines()
        .find(|line| line.starts_with("cmdock://connect?payload="))
        .unwrap_or_else(|| panic!("connect URL not found in stdout:\n{stdout}"))
        .trim()
        .to_string()
}

// ============================================================================
// copy_dir_recursive tests
// ============================================================================

#[test]
fn test_copy_dir_recursive_copies_files_and_subdirs() {
    let src_tmp = TempDir::new().unwrap();
    let dst_tmp = TempDir::new().unwrap();
    let src = src_tmp.path();
    let dst = dst_tmp.path().join("dest");

    // Create nested structure: src/a.txt, src/sub/b.txt, src/sub/deep/c.txt
    std::fs::write(src.join("a.txt"), b"file-a").unwrap();
    std::fs::create_dir_all(src.join("sub").join("deep")).unwrap();
    std::fs::write(src.join("sub").join("b.txt"), b"file-b").unwrap();
    std::fs::write(src.join("sub").join("deep").join("c.txt"), b"file-c").unwrap();

    copy_dir_recursive(src, &dst).unwrap();

    assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"file-a");
    assert_eq!(
        std::fs::read(dst.join("sub").join("b.txt")).unwrap(),
        b"file-b"
    );
    assert_eq!(
        std::fs::read(dst.join("sub").join("deep").join("c.txt")).unwrap(),
        b"file-c"
    );
}

#[cfg(unix)]
#[test]
fn test_copy_dir_recursive_skips_symlinks() {
    use std::os::unix::fs::symlink;

    let src_tmp = TempDir::new().unwrap();
    let dst_tmp = TempDir::new().unwrap();
    let src = src_tmp.path();
    let dst = dst_tmp.path().join("dest");

    // Create a regular file and a symlink to it
    std::fs::write(src.join("real.txt"), b"real-data").unwrap();
    symlink(src.join("real.txt"), src.join("link.txt")).unwrap();

    // Also create a symlink to a directory
    std::fs::create_dir(src.join("realdir")).unwrap();
    std::fs::write(src.join("realdir").join("inside.txt"), b"inside").unwrap();
    symlink(src.join("realdir"), src.join("linkdir")).unwrap();

    copy_dir_recursive(src, &dst).unwrap();

    // Real file should be copied
    assert!(
        dst.join("real.txt").exists(),
        "regular file should be copied"
    );
    assert_eq!(std::fs::read(dst.join("real.txt")).unwrap(), b"real-data");

    // Symlink to file should NOT be copied
    assert!(
        !dst.join("link.txt").exists(),
        "symlink to file should be skipped"
    );

    // Real directory and its contents should be copied
    assert!(
        dst.join("realdir").join("inside.txt").exists(),
        "real directory contents should be copied"
    );

    // Symlink to directory should NOT be copied
    assert!(
        !dst.join("linkdir").exists(),
        "symlink to directory should be skipped"
    );
}

#[test]
fn test_copy_dir_recursive_handles_empty_dirs() {
    let src_tmp = TempDir::new().unwrap();
    let dst_tmp = TempDir::new().unwrap();
    let src = src_tmp.path();
    let dst = dst_tmp.path().join("dest");

    // Create empty subdirectories
    std::fs::create_dir_all(src.join("empty1")).unwrap();
    std::fs::create_dir_all(src.join("empty2").join("nested_empty")).unwrap();

    copy_dir_recursive(src, &dst).unwrap();

    assert!(dst.join("empty1").is_dir(), "empty dir should be created");
    assert!(
        dst.join("empty2").join("nested_empty").is_dir(),
        "nested empty dir should be created"
    );
}

// ============================================================================
// Token revoke prefix matching tests
// ============================================================================
//
// These test the matching logic used by run_token's Revoke branch.
// Since run_token is private and calls process::exit, we replicate
// the prefix matching algorithm against a real ConfigStore.

async fn setup_store_with_tokens() -> (Arc<dyn ConfigStore>, TempDir, String, Vec<String>) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "token-test-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();

    // Create 3 tokens and collect their hashes
    let mut token_hashes = Vec::new();
    for i in 0..3 {
        let raw_token = store
            .create_api_token(&user.id, Some(&format!("tok-{i}")))
            .await
            .unwrap();
        // We need the hash, not the raw token. List tokens to get hashes.
        let _ = raw_token;
    }

    let tokens = store.list_api_tokens(&user.id).await.unwrap();
    for t in &tokens {
        token_hashes.push(t.token_hash.clone());
    }

    (store, tmp, user.id, token_hashes)
}

/// Replicate the prefix matching logic from run_token Revoke branch.
async fn revoke_by_prefix(store: &dyn ConfigStore, prefix: &str) -> Result<bool, String> {
    if prefix.len() >= 64 {
        // Exact match
        return store
            .revoke_api_token(prefix)
            .await
            .map_err(|e| e.to_string());
    }

    let users = store.list_users().await.map_err(|e| e.to_string())?;
    let mut found_hash = None;
    for user in &users {
        let tokens = store
            .list_api_tokens(&user.id)
            .await
            .map_err(|e| e.to_string())?;
        for t in &tokens {
            if t.token_hash.starts_with(prefix) {
                if found_hash.is_some() {
                    return Err(format!(
                        "Ambiguous prefix '{prefix}' matches multiple tokens. Use a longer prefix."
                    ));
                }
                found_hash = Some(t.token_hash.clone());
            }
        }
    }

    match found_hash {
        Some(hash) => store
            .revoke_api_token(&hash)
            .await
            .map_err(|e| e.to_string()),
        None => Ok(false),
    }
}

#[tokio::test]
async fn test_token_revoke_exact_hash_match() {
    let (store, _tmp, _user_id, hashes) = setup_store_with_tokens().await;

    let target_hash = &hashes[0];
    let result = revoke_by_prefix(store.as_ref(), target_hash).await;
    assert!(result.is_ok(), "exact hash revoke should succeed");
    assert!(
        result.unwrap(),
        "exact hash should find and revoke the token"
    );

    // Verify it's actually gone: revoking again should return false
    let result2 = revoke_by_prefix(store.as_ref(), target_hash).await;
    assert!(result2.is_ok());
    assert!(
        !result2.unwrap(),
        "revoking already-revoked token should return false"
    );
}

#[tokio::test]
async fn test_token_revoke_unique_prefix_match() {
    let (store, _tmp, _user_id, hashes) = setup_store_with_tokens().await;

    // Find a prefix length that uniquely identifies the first token.
    // SHA-256 hashes are hex — 8 chars should almost always be unique among 3 tokens.
    let target_hash = &hashes[0];
    let mut prefix_len = 8;

    // Ensure uniqueness: extend prefix if needed
    loop {
        let prefix = &target_hash[..prefix_len];
        let matches: Vec<_> = hashes.iter().filter(|h| h.starts_with(prefix)).collect();
        if matches.len() == 1 {
            break;
        }
        prefix_len += 4;
        assert!(
            prefix_len <= 64,
            "could not find unique prefix (extremely unlikely)"
        );
    }

    let prefix = &target_hash[..prefix_len];
    let result = revoke_by_prefix(store.as_ref(), prefix).await;
    assert!(result.is_ok(), "unique prefix revoke should succeed");
    assert!(
        result.unwrap(),
        "unique prefix should find and revoke the token"
    );
}

#[tokio::test]
async fn test_token_revoke_ambiguous_prefix() {
    let (store, _tmp, user_id, hashes) = setup_store_with_tokens().await;

    // Find a prefix that matches at least 2 tokens. Start with 1 char and extend.
    // With 3 tokens and hex hashes (16 possible first chars), there's a good chance
    // two share a 1-char prefix. If not, we manufacture the scenario.
    let mut ambiguous_prefix = None;
    for len in 1..=4 {
        for h in &hashes {
            let prefix = &h[..len];
            let matches: Vec<_> = hashes
                .iter()
                .filter(|other| other.starts_with(prefix))
                .collect();
            if matches.len() >= 2 {
                ambiguous_prefix = Some(prefix.to_string());
                break;
            }
        }
        if ambiguous_prefix.is_some() {
            break;
        }
    }

    // If we couldn't find a natural collision (very unlikely with 3 tokens and 1-4 char prefixes),
    // create more tokens to force one.
    let ambiguous_prefix = if let Some(p) = ambiguous_prefix {
        p
    } else {
        // Create many more tokens to guarantee a prefix collision
        for i in 3..20 {
            store
                .create_api_token(&user_id, Some(&format!("extra-{i}")))
                .await
                .unwrap();
        }
        let all_tokens = store.list_api_tokens(&user_id).await.unwrap();
        let all_hashes: Vec<_> = all_tokens.iter().map(|t| t.token_hash.clone()).collect();

        let mut found = None;
        for len in 1..=4 {
            for h in &all_hashes {
                let prefix = &h[..len];
                let matches: Vec<_> = all_hashes
                    .iter()
                    .filter(|other| other.starts_with(prefix))
                    .collect();
                if matches.len() >= 2 {
                    found = Some(prefix.to_string());
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        found.expect("with 20 tokens, a 1-4 char prefix collision is virtually certain")
    };

    let result = revoke_by_prefix(store.as_ref(), &ambiguous_prefix).await;
    assert!(result.is_err(), "ambiguous prefix should return an error");
    assert!(
        result.unwrap_err().contains("Ambiguous"),
        "error should mention ambiguity"
    );
}

#[tokio::test]
async fn test_token_revoke_no_match() {
    let (store, _tmp, _user_id, _hashes) = setup_store_with_tokens().await;

    // Use a prefix that won't match any real SHA-256 hash
    let result = revoke_by_prefix(store.as_ref(), "zzzzzzzz").await;
    assert!(result.is_ok(), "no-match should not error");
    assert!(!result.unwrap(), "no-match should return false");
}

// ============================================================================
// Backup tests
// ============================================================================

#[tokio::test]
async fn test_backup_creates_expected_files() {
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

    // Create two users with replica dirs
    let user1 = store
        .create_user(&NewUser {
            username: "backup-user1".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();
    let user2 = store
        .create_user(&NewUser {
            username: "backup-user2".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();

    // Create replica dirs with dummy data
    for uid in [&user1.id, &user2.id] {
        let replica_dir = live_dir.join("users").join(uid);
        std::fs::create_dir_all(&replica_dir).unwrap();
        std::fs::write(
            replica_dir.join("taskchampion.sqlite3"),
            format!("data-{uid}"),
        )
        .unwrap();
    }

    // Checkpoint WAL for clean copy
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

    // Simulate backup (same logic as run_backup)
    let backup_tmp = TempDir::new().unwrap();
    let backup_dir = backup_tmp.path().join("backup-test");
    std::fs::create_dir_all(&backup_dir).unwrap();

    // Copy config DB
    std::fs::copy(
        live_dir.join("config.sqlite"),
        backup_dir.join("config.sqlite"),
    )
    .unwrap();

    // Copy WAL/SHM if present
    for suffix in &["-wal", "-shm"] {
        let src = live_dir.join(format!("config.sqlite{suffix}"));
        if src.exists() {
            std::fs::copy(&src, backup_dir.join(format!("config.sqlite{suffix}"))).unwrap();
        }
    }

    // Copy user replicas using copy_dir_recursive
    let backup_users = backup_dir.join("users");
    std::fs::create_dir_all(&backup_users).unwrap();
    let mut user_count = 0u32;
    for entry in std::fs::read_dir(live_dir.join("users")).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            let dest = backup_users.join(entry.file_name());
            copy_dir_recursive(&entry.path(), &dest).unwrap();
            user_count += 1;
        }
    }

    // Verify backup structure
    assert!(
        backup_dir.join("config.sqlite").exists(),
        "backup must contain config.sqlite"
    );
    assert_eq!(user_count, 2, "backup should copy 2 user replica dirs");
    assert!(
        backup_dir
            .join("users")
            .join(&user1.id)
            .join("taskchampion.sqlite3")
            .exists(),
        "user1 replica should be in backup"
    );
    assert!(
        backup_dir
            .join("users")
            .join(&user2.id)
            .join("taskchampion.sqlite3")
            .exists(),
        "user2 replica should be in backup"
    );

    // Verify data integrity
    let data1 = std::fs::read_to_string(
        backup_dir
            .join("users")
            .join(&user1.id)
            .join("taskchampion.sqlite3"),
    )
    .unwrap();
    assert_eq!(data1, format!("data-{}", user1.id));
}

#[tokio::test]
async fn test_backup_copies_wal_shm_files() {
    let live_tmp = TempDir::new().unwrap();
    let live_dir = live_tmp.path();

    // Create config.sqlite and synthetic WAL/SHM files
    std::fs::write(live_dir.join("config.sqlite"), b"main-db").unwrap();
    std::fs::write(live_dir.join("config.sqlite-wal"), b"wal-data").unwrap();
    std::fs::write(live_dir.join("config.sqlite-shm"), b"shm-data").unwrap();

    let backup_tmp = TempDir::new().unwrap();
    let backup_dir = backup_tmp.path().join("backup");
    std::fs::create_dir_all(&backup_dir).unwrap();

    // Replicate backup logic for WAL/SHM
    std::fs::copy(
        live_dir.join("config.sqlite"),
        backup_dir.join("config.sqlite"),
    )
    .unwrap();
    for suffix in &["-wal", "-shm"] {
        let src = live_dir.join(format!("config.sqlite{suffix}"));
        if src.exists() {
            std::fs::copy(&src, backup_dir.join(format!("config.sqlite{suffix}"))).unwrap();
        }
    }

    assert!(
        backup_dir.join("config.sqlite-wal").exists(),
        "WAL file should be copied"
    );
    assert!(
        backup_dir.join("config.sqlite-shm").exists(),
        "SHM file should be copied"
    );
    assert_eq!(
        std::fs::read(backup_dir.join("config.sqlite-wal")).unwrap(),
        b"wal-data"
    );
    assert_eq!(
        std::fs::read(backup_dir.join("config.sqlite-shm")).unwrap(),
        b"shm-data"
    );
}

// ============================================================================
// Restore tests
// ============================================================================

#[tokio::test]
async fn test_restore_copies_files_correctly() {
    // Create a backup directory with known contents
    let backup_tmp = TempDir::new().unwrap();
    let backup_dir = backup_tmp.path();

    let db_path = backup_dir.join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "restore-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();

    // Create user replica in backup
    let backup_users = backup_dir.join("users");
    std::fs::create_dir_all(backup_users.join(&user.id)).unwrap();
    std::fs::write(
        backup_users.join(&user.id).join("taskchampion.sqlite3"),
        b"restore-data",
    )
    .unwrap();

    // Checkpoint for clean copy
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

    // Restore to a fresh directory (replicate run_restore logic)
    let restore_tmp = TempDir::new().unwrap();
    let restore_dir = restore_tmp.path();
    std::fs::create_dir_all(restore_dir.join("users")).unwrap();

    // Copy config DB
    std::fs::copy(
        backup_dir.join("config.sqlite"),
        restore_dir.join("config.sqlite"),
    )
    .unwrap();

    // Copy WAL/SHM
    for suffix in &["-wal", "-shm"] {
        let src = backup_dir.join(format!("config.sqlite{suffix}"));
        if src.exists() {
            std::fs::copy(&src, restore_dir.join(format!("config.sqlite{suffix}"))).unwrap();
        }
    }

    // Copy user replicas
    for entry in std::fs::read_dir(backup_dir.join("users")).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            let dest = restore_dir.join("users").join(entry.file_name());
            if dest.exists() {
                std::fs::remove_dir_all(&dest).unwrap();
            }
            copy_dir_recursive(&entry.path(), &dest).unwrap();
        }
    }

    // Verify restored data
    let restored_store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&restore_dir.join("config.sqlite").to_string_lossy())
            .await
            .unwrap(),
    );

    let restored_user = restored_store
        .get_user_by_id(&user.id)
        .await
        .unwrap()
        .expect("user should exist in restored DB");
    assert_eq!(restored_user.username, "restore-user");

    let replica_data = std::fs::read(
        restore_dir
            .join("users")
            .join(&user.id)
            .join("taskchampion.sqlite3"),
    )
    .unwrap();
    assert_eq!(replica_data, b"restore-data");
}

#[tokio::test]
async fn test_restore_cleans_up_orphan_user_dirs() {
    // Create a backup with one user
    let backup_tmp = TempDir::new().unwrap();
    let backup_dir = backup_tmp.path();
    std::fs::create_dir_all(backup_dir.join("users").join("kept-user")).unwrap();
    std::fs::write(
        backup_dir.join("users").join("kept-user").join("data.db"),
        b"kept",
    )
    .unwrap();
    // Create a minimal config.sqlite so restore doesn't bail
    std::fs::write(backup_dir.join("config.sqlite"), b"dummy").unwrap();

    // Create a restore target with an orphan user dir
    let restore_tmp = TempDir::new().unwrap();
    let restore_dir = restore_tmp.path();
    std::fs::create_dir_all(restore_dir.join("users").join("kept-user")).unwrap();
    std::fs::create_dir_all(restore_dir.join("users").join("orphan-user")).unwrap();
    std::fs::write(
        restore_dir
            .join("users")
            .join("orphan-user")
            .join("stale.db"),
        b"stale",
    )
    .unwrap();

    // Replicate restore's orphan cleanup logic
    let users_backup = backup_dir.join("users");
    let users_dir = restore_dir.join("users");

    let backup_ids: std::collections::HashSet<_> = std::fs::read_dir(&users_backup)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name())
        .collect();

    let mut orphan_count = 0u32;
    for entry in std::fs::read_dir(&users_dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() && !backup_ids.contains(&entry.file_name()) {
            std::fs::remove_dir_all(entry.path()).unwrap();
            orphan_count += 1;
        }
    }

    // Copy backed-up replicas
    for entry in std::fs::read_dir(&users_backup).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            let dest = users_dir.join(entry.file_name());
            if dest.exists() {
                std::fs::remove_dir_all(&dest).unwrap();
            }
            copy_dir_recursive(&entry.path(), &dest).unwrap();
        }
    }

    // Verify orphan was removed
    assert_eq!(orphan_count, 1, "one orphan dir should have been removed");
    assert!(
        !restore_dir.join("users").join("orphan-user").exists(),
        "orphan-user dir should be gone"
    );

    // Verify kept user was restored
    assert!(
        restore_dir
            .join("users")
            .join("kept-user")
            .join("data.db")
            .exists(),
        "kept-user data should be restored"
    );
    assert_eq!(
        std::fs::read(restore_dir.join("users").join("kept-user").join("data.db")).unwrap(),
        b"kept"
    );
}

#[tokio::test]
async fn test_admin_user_delete_rejects_forbidden_runtime_policy() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "policy-delete-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();
    let policy = RuntimePolicy {
        runtime_access: RuntimeAccessMode::Block,
        delete_action: RuntimeDeleteAction::Forbid,
    };
    store
        .upsert_runtime_policy(
            &user.id,
            "policy-v1",
            &policy,
            Some("policy-v1"),
            Some(&policy),
            Some("2026-04-03 12:00:00"),
        )
        .await
        .unwrap();

    let output = run_admin(tmp.path(), &["admin", "user", "delete", &user.id, "-y"]);
    assert!(
        !output.status.success(),
        "user delete should be rejected when policy forbids it: {output:?}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("applied runtime policy does not allow deletion"));
    assert!(store.get_user_by_id(&user.id).await.unwrap().is_some());
}

#[tokio::test]
async fn test_admin_connect_config_create_outputs_valid_url_without_qr() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "connect-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();

    let output = run_admin(
        tmp.path(),
        &[
            "admin",
            "connect-config",
            "create",
            &user.id,
            "--server-url",
            "https://tasks.example.com",
            "--name",
            "Dogfood",
            "--no-qr",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Token ID:"));
    assert!(stdout.contains("Token Hash:"));
    let connect_url = extract_connect_url(&stdout);
    assert!(connect_url.len() <= 250, "connect URL must fit QR budget");

    let payload = decode_connect_url(&connect_url).unwrap();
    assert_eq!(payload.server_url, "https://tasks.example.com");
    assert!(payload.token_id.is_some());
    assert!(payload.token_id.as_deref().unwrap().starts_with("cc_"));
    assert_eq!(payload.name.as_deref(), Some("Dogfood"));
    assert!(payload.credential.len() <= 24);

    let looked_up = store.get_user_by_token(&payload.credential).await.unwrap();
    assert_eq!(looked_up.unwrap().id, user.id);
}

#[tokio::test]
async fn test_admin_connect_config_create_uses_public_base_url_from_config() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();
    let user = store
        .create_user(&NewUser {
            username: "config-default-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();

    let config_path = tmp.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[server]\nhost = \"127.0.0.1\"\nport = 8080\ndata_dir = \"{}\"\npublic_base_url = \"https://cmdock.example.com\"\n",
            data_dir.display()
        ),
    )
    .unwrap();

    let output = run_admin_with_config(
        &config_path,
        &["admin", "connect-config", "create", &user.id, "--no-qr"],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let connect_url = extract_connect_url(&stdout);
    let payload = decode_connect_url(&connect_url).unwrap();
    assert_eq!(payload.server_url, "https://cmdock.example.com");
    assert!(payload.token_id.is_some());
}

#[tokio::test]
async fn test_admin_token_list_shows_connect_config_usage_fields() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "connect-token-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();

    let token = store
        .create_connect_config_token(&user.id, "2099-01-01 00:00:00", 18)
        .await
        .unwrap()
        .token;
    store
        .record_connect_config_token_use(&token, "203.0.113.10")
        .await
        .unwrap();

    let output = run_admin(tmp.path(), &["admin", "token", "list", &user.id]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(stdout.contains("FIRST_USED"));
    assert!(stdout.contains("LAST_USED"));
    assert!(stdout.contains("LAST_IP"));
    assert!(stdout.contains("connect-config"));
    assert!(stdout.contains("203.0.113.10"));
}
