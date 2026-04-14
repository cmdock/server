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
    let extra_user = target_store
        .create_user(&NewUser {
            username: "keep-user".to_string(),
            password_hash: String::new(),
        })
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
