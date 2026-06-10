use std::process::Command;
use std::sync::Arc;

use tempfile::TempDir;

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

#[tokio::test]
async fn cli_single_user_restore_nulls_cross_scope_task_key_allocation_fk() {
    let backup_tmp = TempDir::new().unwrap();
    let backup_db = backup_tmp.path().join("config.sqlite");
    let backup_store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&backup_db.to_string_lossy())
            .await
            .unwrap(),
    );
    backup_store.run_migrations().await.unwrap();

    let restored_user = backup_store
        .create_user(&NewUser {
            username: "restore-fk-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();
    let foreign_scope_user = backup_store
        .create_user(&NewUser {
            username: "foreign-scope-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();
    cmdock_server::admin::prefix::backfill_missing_user_prefixes(backup_store.as_ref())
        .await
        .unwrap();
    {
        let conn = rusqlite::Connection::open(&backup_db).unwrap();
        let restored_prefix: String = conn
            .query_row(
                "SELECT prefix FROM users WHERE id = ?1",
                [&restored_user.id],
                |row| row.get(0),
            )
            .unwrap();
        let foreign_scope_id: String = conn
            .query_row(
                "SELECT id FROM task_scopes WHERE owner_runtime_user_id = ?1",
                [&foreign_scope_user.id],
                |row| row.get(0),
            )
            .unwrap();
        let task_uuid = uuid::Uuid::new_v4().to_string();
        let attempt_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO task_key_allocations
                 (user_id, prefix, n, task_uuid, state, attempt_id, task_scope_id)
             VALUES (?1, ?2, 888, ?3, 'committed', ?4, ?5)",
            [
                restored_user.id.as_str(),
                restored_prefix.as_str(),
                task_uuid.as_str(),
                attempt_id.as_str(),
                foreign_scope_id.as_str(),
            ],
        )
        .unwrap();
        let backup_scope_before_restore: Option<String> = conn
            .query_row(
                "SELECT task_scope_id FROM task_key_allocations WHERE user_id = ?1 AND n = 888",
                [&restored_user.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            backup_scope_before_restore.as_deref(),
            Some(foreign_scope_id.as_str())
        );
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }

    let target_tmp = TempDir::new().unwrap();
    std::fs::copy(&backup_db, target_tmp.path().join("config.sqlite")).unwrap();
    {
        let target_db = target_tmp.path().join("config.sqlite");
        let conn = rusqlite::Connection::open(&target_db).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute(
            "DELETE FROM task_key_allocations WHERE user_id = ?1 AND n = 888",
            [&restored_user.id],
        )
        .unwrap();
        conn.execute("DELETE FROM users WHERE id = ?1", [&foreign_scope_user.id])
            .unwrap();
        let foreign_scope_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM task_scopes WHERE owner_runtime_user_id = ?1)",
                [&foreign_scope_user.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !foreign_scope_exists,
            "target DB must not contain the foreign scope before selective restore"
        );
    }

    let output = run_admin(
        target_tmp.path(),
        &[
            "admin",
            "restore",
            "--input",
            backup_tmp.path().to_str().unwrap(),
            "--user-id",
            &restored_user.id,
            "-y",
        ],
    );
    assert!(output.status.success(), "{output:?}");

    let conn = rusqlite::Connection::open(target_tmp.path().join("config.sqlite")).unwrap();
    let restored_allocation_scope: Option<String> = conn
        .query_row(
            "SELECT task_scope_id FROM task_key_allocations WHERE user_id = ?1 AND n = 888",
            [&restored_user.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        restored_allocation_scope.is_none(),
        "single-user restore must NULL task_scope_id values that point at scopes it did not restore"
    );
    let fk_violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(fk_violations, 0);
}

#[tokio::test]
async fn test_admin_restore_user_replaces_only_selected_user() {
    let backup_tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(backup_tmp.path().join("users")).unwrap();
    let backup_db = backup_tmp.path().join("config.sqlite");
    let backup_store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&backup_db.to_string_lossy())
            .await
            .unwrap(),
    );
    backup_store.run_migrations().await.unwrap();

    let restored_user = backup_store
        .create_user(&NewUser {
            username: "backup-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();
    cmdock_server::admin::prefix::backfill_missing_user_prefixes(backup_store.as_ref())
        .await
        .unwrap();
    backup_store
        .create_api_token(&restored_user.id, Some("backup-token"))
        .await
        .unwrap();
    backup_store
        .create_replica(&restored_user.id, &uuid::Uuid::new_v4().to_string(), "enc")
        .await
        .unwrap();
    let restored_client_id = uuid::Uuid::new_v4().to_string();
    backup_store
        .create_device(
            &restored_user.id,
            &restored_client_id,
            "Backup Laptop",
            Some("enc"),
        )
        .await
        .unwrap();
    let foreign_scope_user = backup_store
        .create_user(&NewUser {
            username: "foreign-scope-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();
    cmdock_server::admin::prefix::backfill_missing_user_prefixes(backup_store.as_ref())
        .await
        .unwrap();
    {
        let conn = rusqlite::Connection::open(&backup_db).unwrap();
        let restored_prefix: String = conn
            .query_row(
                "SELECT prefix FROM users WHERE id = ?1",
                [&restored_user.id],
                |row| row.get(0),
            )
            .unwrap();
        let foreign_scope_id: String = conn
            .query_row(
                "SELECT id FROM task_scopes WHERE owner_runtime_user_id = ?1",
                [&foreign_scope_user.id],
                |row| row.get(0),
            )
            .unwrap();
        let task_uuid = uuid::Uuid::new_v4().to_string();
        let attempt_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO task_key_allocations
                 (user_id, prefix, n, task_uuid, state, attempt_id, task_scope_id)
             VALUES (?1, ?2, 777, ?3, 'committed', ?4, ?5)",
            [
                restored_user.id.as_str(),
                restored_prefix.as_str(),
                task_uuid.as_str(),
                attempt_id.as_str(),
                foreign_scope_id.as_str(),
            ],
        )
        .unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }
    let backup_user_dir = backup_tmp.path().join("users").join(&restored_user.id);
    std::fs::create_dir_all(&backup_user_dir).unwrap();
    std::fs::write(
        backup_user_dir.join("taskchampion.sqlite3"),
        b"backup-user-data",
    )
    .unwrap();

    let target_tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(target_tmp.path().join("users")).unwrap();
    std::fs::copy(&backup_db, target_tmp.path().join("config.sqlite")).unwrap();
    cmdock_server::admin::cli::copy_dir_recursive(
        &backup_user_dir,
        &target_tmp.path().join("users").join(&restored_user.id),
    )
    .unwrap();

    let target_db = target_tmp.path().join("config.sqlite");
    let target_store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&target_db.to_string_lossy())
            .await
            .unwrap(),
    );
    target_store.run_migrations().await.unwrap();
    {
        let conn = rusqlite::Connection::open(&target_db).unwrap();
        conn.execute(
            "DELETE FROM task_key_allocations WHERE user_id = ?1",
            [&restored_user.id],
        )
        .unwrap();
        conn.execute("DELETE FROM users WHERE id = ?1", [&foreign_scope_user.id])
            .unwrap();
    }
    let extra_user = target_store
        .create_user(&NewUser {
            username: "keep-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();
    cmdock_server::admin::prefix::backfill_missing_user_prefixes(target_store.as_ref())
        .await
        .unwrap();
    std::fs::create_dir_all(target_tmp.path().join("users").join(&extra_user.id)).unwrap();
    std::fs::write(
        target_tmp
            .path()
            .join("users")
            .join(&extra_user.id)
            .join("taskchampion.sqlite3"),
        b"keep-user-data",
    )
    .unwrap();

    let conn = rusqlite::Connection::open(&target_db).unwrap();
    conn.execute(
        "UPDATE users SET username = 'mutated-user' WHERE id = ?1",
        [&restored_user.id],
    )
    .unwrap();
    conn.execute(
        "DELETE FROM devices WHERE user_id = ?1",
        [&restored_user.id],
    )
    .unwrap();
    std::fs::write(
        target_tmp
            .path()
            .join("users")
            .join(&restored_user.id)
            .join("taskchampion.sqlite3"),
        b"mutated-user-data",
    )
    .unwrap();

    let output = run_admin(
        target_tmp.path(),
        &[
            "admin",
            "restore",
            "--input",
            backup_tmp.path().to_str().unwrap(),
            "--user-id",
            &restored_user.id,
            "-y",
        ],
    );
    assert!(output.status.success(), "{output:?}");

    let restored_store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&target_db.to_string_lossy())
            .await
            .unwrap(),
    );
    restored_store.run_migrations().await.unwrap();

    let restored_user_row = restored_store
        .get_user_by_id(&restored_user.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored_user_row.username, "backup-user");
    assert_eq!(
        restored_store
            .list_devices(&restored_user.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(restored_store
        .get_replica_by_user(&restored_user.id)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        std::fs::read(
            target_tmp
                .path()
                .join("users")
                .join(&restored_user.id)
                .join("taskchampion.sqlite3")
        )
        .unwrap(),
        b"backup-user-data"
    );
    {
        let conn = rusqlite::Connection::open(&target_db).unwrap();
        let restored_allocation_scope: Option<String> = conn
            .query_row(
                "SELECT task_scope_id FROM task_key_allocations WHERE user_id = ?1 AND n = 777",
                [&restored_user.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            restored_allocation_scope.is_none(),
            "selective restore must not preserve task_scope_id references to scopes it did not restore"
        );
    }

    let extra_user_row = restored_store
        .get_user_by_id(&extra_user.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(extra_user_row.username, "keep-user");
    assert_eq!(
        std::fs::read(
            target_tmp
                .path()
                .join("users")
                .join(&extra_user.id)
                .join("taskchampion.sqlite3")
        )
        .unwrap(),
        b"keep-user-data"
    );
}
