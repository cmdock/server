mod common;

use std::process::Command;
use std::sync::Arc;

use axum::http::{header, HeaderValue};
use axum::Router;
use axum_test::TestServer;
use tempfile::TempDir;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

use cmdock_server::admin;
use cmdock_server::admin::cli::copy_dir_recursive;
use cmdock_server::admin::recovery::run_startup_recovery_assessment;
use cmdock_server::app_state::AppState;
use cmdock_server::devices;
use cmdock_server::health;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::tasks;
use cmdock_server::tc_sync;
use cmdock_server::tc_sync::crypto::SyncCryptor;

const ADMIN_TOKEN: &str = "recovery-admin-token";

fn admin_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cmdock-server")
}

fn master_key_hex() -> String {
    "2a".repeat(32)
}

fn master_key_bytes() -> [u8; 32] {
    [42u8; 32]
}

fn run_admin(data_dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(admin_bin())
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env("CMDOCK_MASTER_KEY", master_key_hex())
        .output()
        .unwrap()
}

fn stdout_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn extract_field(output: &str, prefix: &str) -> String {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix(prefix).map(str::trim))
        .unwrap()
        .to_string()
}

fn extract_indented_value(output: &str, prefix: &str) -> String {
    output
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(prefix).map(str::trim))
        .unwrap()
        .to_string()
}

fn auth_header(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
}

fn admin_auth_header() -> HeaderValue {
    auth_header(ADMIN_TOKEN)
}

fn client_id_header(client_id: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::HeaderName::from_static("x-client-id"),
        HeaderValue::from_str(client_id).unwrap(),
    )
}

async fn start_server(data_dir: std::path::PathBuf) -> TestServer {
    let db_path = data_dir.join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let mut config = common::test_server_config_with_admin_token(data_dir, ADMIN_TOKEN);
    config.server.public_base_url = None;
    config.master_key = Some(master_key_bytes());

    let state = AppState::new(store, &config);
    run_startup_recovery_assessment(&state).await.unwrap();
    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(devices::routes())
        .merge(admin::routes())
        .merge(tc_sync::routes())
        .with_state(state);
    TestServer::new(app)
}

fn create_backup_snapshot(data_dir: &std::path::Path, backup_dir: &std::path::Path) {
    std::fs::create_dir_all(backup_dir).unwrap();
    std::fs::copy(
        data_dir.join("config.sqlite"),
        backup_dir.join("config.sqlite"),
    )
    .unwrap();
    for suffix in ["-wal", "-shm"] {
        let src = data_dir.join(format!("config.sqlite{suffix}"));
        if src.exists() {
            std::fs::copy(src, backup_dir.join(format!("config.sqlite{suffix}"))).unwrap();
        }
    }

    let users_src = data_dir.join("users");
    let users_dst = backup_dir.join("users");
    std::fs::create_dir_all(&users_dst).unwrap();
    for entry in std::fs::read_dir(users_src).unwrap().flatten() {
        if entry.file_type().unwrap().is_dir() {
            copy_dir_recursive(&entry.path(), &users_dst.join(entry.file_name())).unwrap();
        }
    }
}

async fn create_user(data_dir: &std::path::Path, username: &str) -> (String, String) {
    let output = run_admin(
        data_dir,
        &["admin", "user", "create", "--username", username],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    let user_id = extract_indented_value(&stdout, "ID:");
    let token = stdout.lines().last().map(str::trim).unwrap().to_string();
    (user_id, token)
}

async fn create_device(data_dir: &std::path::Path, user_id: &str, name: &str) -> (String, String) {
    let output = run_admin(data_dir, &["admin", "sync", "create", user_id]);
    assert!(output.status.success(), "{output:?}");
    let output = run_admin(
        data_dir,
        &[
            "admin",
            "device",
            "create",
            user_id,
            "--name",
            name,
            "--server-url",
            "https://sync.example.com",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    (
        extract_field(&stdout, "Client ID:"),
        extract_field(&stdout, "Encryption Secret:"),
    )
}

async fn wait_for_offline_transition() {
    sleep(Duration::from_millis(700)).await;
}

#[tokio::test]
async fn test_running_server_selective_restore_one_user_other_user_unaffected() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let (user_a, token_a) = create_user(data_dir, "restore-user-a").await;
    let (_user_b, token_b) = create_user(data_dir, "restore-user-b").await;

    let server = start_server(data_dir.to_path_buf()).await;
    let auth_a = auth_header(&token_a);
    let auth_b = auth_header(&token_b);

    server
        .post("/api/tasks")
        .add_header(header::AUTHORIZATION, auth_a.clone())
        .json(&serde_json::json!({"raw": "baseline-a"}))
        .await
        .assert_status_ok();
    server
        .post("/api/tasks")
        .add_header(header::AUTHORIZATION, auth_b.clone())
        .json(&serde_json::json!({"raw": "baseline-b"}))
        .await
        .assert_status_ok();

    let backup_tmp = TempDir::new().unwrap();
    create_backup_snapshot(data_dir, backup_tmp.path());

    server
        .post("/api/tasks")
        .add_header(header::AUTHORIZATION, auth_a.clone())
        .json(&serde_json::json!({"raw": "mutated-a"}))
        .await
        .assert_status_ok();
    server
        .post("/api/tasks")
        .add_header(header::AUTHORIZATION, auth_b.clone())
        .json(&serde_json::json!({"raw": "mutated-b"}))
        .await
        .assert_status_ok();

    let output = run_admin(data_dir, &["admin", "user", "offline", &user_a]);
    assert!(output.status.success(), "{output:?}");
    wait_for_offline_transition().await;

    server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth_a.clone())
        .await
        .assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
    server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth_b.clone())
        .await
        .assert_status_ok();

    let output = run_admin(
        data_dir,
        &[
            "admin",
            "restore",
            "--input",
            backup_tmp.path().to_str().unwrap(),
            "--user-id",
            &user_a,
            "-y",
        ],
    );
    assert!(output.status.success(), "{output:?}");

    let output = run_admin(data_dir, &["admin", "user", "assess", &user_a]);
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    assert!(stdout.contains("Status:               Healthy"), "{stdout}");

    let output = run_admin(data_dir, &["admin", "user", "online", &user_a]);
    assert!(output.status.success(), "{output:?}");
    wait_for_offline_transition().await;

    let body_a = server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth_a.clone())
        .await
        .json::<serde_json::Value>();
    let body_b = server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth_b.clone())
        .await
        .json::<serde_json::Value>();

    let tasks_a = body_a.as_array().unwrap();
    let tasks_b = body_b.as_array().unwrap();
    assert_eq!(tasks_a.len(), 1, "user A should be restored to baseline");
    assert_eq!(tasks_b.len(), 2, "user B should keep post-backup mutations");
}

#[tokio::test]
async fn test_startup_recovery_quarantines_user_needing_operator_attention() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let (user_id, token) = create_user(data_dir, "startup-broken-user").await;
    let (client_id, _secret) = create_device(data_dir, &user_id, "Broken At Startup").await;

    let db_path = data_dir.join("config.sqlite");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE devices SET encryption_secret_enc = NULL WHERE client_id = ?1",
        [&client_id],
    )
    .unwrap();

    let server = start_server(data_dir.to_path_buf()).await;
    let auth = auth_header(&token);

    server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .await
        .assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    let stats = server
        .get(&format!("/admin/user/{user_id}/stats"))
        .add_header(header::AUTHORIZATION, admin_auth_header())
        .await;
    stats.assert_status_ok();
    let body = stats.json::<serde_json::Value>();
    assert_eq!(body["quarantined"], true);
    assert_eq!(
        body["recovery_assessment"]["status"],
        serde_json::json!("needs_operator_attention")
    );

    assert!(
        data_dir
            .join("users")
            .join(&user_id)
            .join(".offline")
            .exists(),
        "startup recovery should persist the offline marker"
    );
}

#[tokio::test]
async fn test_migration_backfills_legacy_migrated_device_secret() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    conn.execute_batch(
        "
        CREATE TABLE users (
            id TEXT PRIMARY KEY,
            username TEXT UNIQUE NOT NULL,
            password_hash TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE replicas (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL REFERENCES users(id),
            encryption_secret_enc TEXT NOT NULL,
            label TEXT NOT NULL DEFAULT 'Personal',
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(user_id)
        );
        CREATE TABLE devices (
            client_id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL REFERENCES users(id),
            name TEXT NOT NULL,
            registered_at TEXT NOT NULL DEFAULT (datetime('now')),
            last_sync_at TEXT,
            last_sync_ip TEXT,
            status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'revoked')),
            encryption_secret_enc TEXT,
            bootstrap_request_id TEXT UNIQUE,
            bootstrap_status TEXT,
            bootstrap_requested_username TEXT,
            bootstrap_create_user_if_missing INTEGER,
            bootstrap_expires_at TEXT
        );
        CREATE TABLE IF NOT EXISTS views (
            id TEXT NOT NULL,
            user_id TEXT NOT NULL REFERENCES users(id),
            label TEXT NOT NULL,
            icon TEXT NOT NULL DEFAULT '',
            filter TEXT NOT NULL DEFAULT '',
            group_by TEXT,
            context_filtered INTEGER NOT NULL DEFAULT 0,
            display_mode TEXT NOT NULL DEFAULT 'list',
            sort_order INTEGER NOT NULL DEFAULT 0,
            origin TEXT NOT NULL DEFAULT 'user' CHECK(origin IN ('builtin', 'user')),
            user_modified INTEGER NOT NULL DEFAULT 0,
            hidden INTEGER NOT NULL DEFAULT 0,
            template_version INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (user_id, id)
        );
        CREATE TABLE _migrations (
            name TEXT PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        ",
    )
    .unwrap();

    let applied: Vec<String> = std::fs::read_dir("migrations")
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.as_str() < "015_backfill_migrated_device_secrets.sql")
        .collect();
    for name in applied {
        conn.execute("INSERT INTO _migrations (name) VALUES (?1)", [&name])
            .unwrap();
    }

    let user_id = uuid::Uuid::new_v4().to_string();
    let client_id = uuid::Uuid::new_v4().to_string();
    let replica_secret = "encrypted-canonical-secret";

    conn.execute(
        "INSERT INTO users (id, username, password_hash) VALUES (?1, 'legacy-user', 'hash')",
        [&user_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO replicas (id, user_id, encryption_secret_enc) VALUES (?1, ?2, ?3)",
        rusqlite::params![client_id, user_id, replica_secret],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO devices (client_id, user_id, name, status, encryption_secret_enc)
         VALUES (?1, ?2, 'Migrated device', 'active', NULL)",
        rusqlite::params![client_id, user_id],
    )
    .unwrap();

    drop(conn);

    let store = cmdock_server::store::sqlite::SqliteConfigStore::new(&db_path.to_string_lossy())
        .await
        .unwrap();
    store.run_migrations().await.unwrap();

    let device = store
        .get_device(&client_id)
        .await
        .unwrap()
        .expect("legacy migrated device should still exist");
    assert_eq!(
        device.encryption_secret_enc.as_deref(),
        Some(replica_secret),
        "migration should backfill the legacy migrated device with the canonical secret"
    );
}

#[tokio::test]
async fn test_startup_recovery_leaves_rebuildable_user_online() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let (user_id, token) = create_user(data_dir, "startup-rebuildable-user").await;
    let (_client_id, _secret) = create_device(data_dir, &user_id, "Rebuildable At Startup").await;
    let auth = auth_header(&token);

    let server = start_server(data_dir.to_path_buf()).await;
    server
        .post("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .json(&serde_json::json!({"raw": "canonical-seeded-before-restart"}))
        .await
        .assert_status_ok();
    drop(server);

    let sync_db = data_dir.join("users").join(&user_id).join("sync.sqlite");
    std::fs::remove_file(&sync_db).unwrap();

    let server = start_server(data_dir.to_path_buf()).await;
    server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .await
        .assert_status_ok();

    let stats = server
        .get(&format!("/admin/user/{user_id}/stats"))
        .add_header(header::AUTHORIZATION, admin_auth_header())
        .await;
    stats.assert_status_ok();
    let body = stats.json::<serde_json::Value>();
    assert_eq!(body["quarantined"], false);
    assert_eq!(
        body["recovery_assessment"]["status"],
        serde_json::json!("rebuildable")
    );
    assert!(
        !data_dir
            .join("users")
            .join(&user_id)
            .join(".offline")
            .exists(),
        "rebuildable users should stay online at startup"
    );
}

#[tokio::test]
async fn test_running_server_restore_reports_rebuildable_and_device_recovers() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let (user_id, token) = create_user(data_dir, "device-restore-user").await;
    let (client_id, secret) = create_device(data_dir, &user_id, "Recovery iPhone").await;

    let server = start_server(data_dir.to_path_buf()).await;
    let auth = auth_header(&token);

    server
        .post("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .json(&serde_json::json!({"raw": "canonical-only-task"}))
        .await
        .assert_status_ok();

    let output = run_admin(data_dir, &["admin", "user", "offline", &user_id]);
    assert!(output.status.success(), "{output:?}");
    wait_for_offline_transition().await;

    let user_dir = data_dir.join("users").join(&user_id);
    let sync_db = user_dir.join("sync.sqlite");
    std::fs::remove_file(&sync_db).ok();

    let output = run_admin(data_dir, &["admin", "user", "assess", &user_id]);
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    assert!(
        stdout.contains("Status:               Rebuildable"),
        "{stdout}"
    );
    assert!(stdout.contains("Shared sync DB:       false"), "{stdout}");

    let output = run_admin(data_dir, &["admin", "user", "online", &user_id]);
    assert!(output.status.success(), "{output:?}");
    wait_for_offline_transition().await;

    let client_uuid = Uuid::parse_str(&client_id).unwrap();
    let cryptor = SyncCryptor::new(client_uuid, secret.as_bytes()).unwrap();
    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{}", Uuid::nil()))
        .add_header(ch, cv)
        .await;
    let status = resp.status_code();
    assert!(
        status == axum::http::StatusCode::OK || status == axum::http::StatusCode::NOT_FOUND,
        "expected rebuilt device read to return 200 or 404, got {status}"
    );
    if status == axum::http::StatusCode::OK {
        let encrypted_body = resp.into_bytes();
        let _ = cryptor
            .unseal(Uuid::nil(), encrypted_body.as_ref())
            .unwrap();
    }
    assert!(
        user_dir.join("sync.sqlite").exists(),
        "shared sync DB should exist after the device recovery path runs"
    );
}

#[tokio::test]
async fn test_offline_marker_persists_across_restart() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let (user_id, token) = create_user(data_dir, "restart-offline-user").await;
    let auth = auth_header(&token);

    let server = start_server(data_dir.to_path_buf()).await;
    server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .await
        .assert_status_ok();

    let output = run_admin(data_dir, &["admin", "user", "offline", &user_id]);
    assert!(output.status.success(), "{output:?}");
    wait_for_offline_transition().await;
    server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .await
        .assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    drop(server);

    let server = start_server(data_dir.to_path_buf()).await;
    wait_for_offline_transition().await;
    server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .await
        .assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    let output = run_admin(data_dir, &["admin", "user", "online", &user_id]);
    assert!(output.status.success(), "{output:?}");
    wait_for_offline_transition().await;
    server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth)
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn test_mixed_surface_recovery_blocks_and_restores_rest_and_tc() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let (user_id, token) = create_user(data_dir, "mixed-surface-user").await;
    let (client_id, secret) = create_device(data_dir, &user_id, "Mixed Surface MacBook").await;

    let server = start_server(data_dir.to_path_buf()).await;
    let auth = auth_header(&token);
    server
        .post("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .json(&serde_json::json!({"raw": "mixed-surface-baseline"}))
        .await
        .assert_status_ok();

    let backup_tmp = TempDir::new().unwrap();
    create_backup_snapshot(data_dir, backup_tmp.path());

    let client_uuid = Uuid::parse_str(&client_id).unwrap();
    let _cryptor = SyncCryptor::new(client_uuid, secret.as_bytes()).unwrap();
    let (ch, cv) = client_id_header(&client_id);
    server
        .get(&format!("/v1/client/get-child-version/{}", Uuid::nil()))
        .add_header(ch, cv)
        .await
        .assert_status_ok();

    server
        .post("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .json(&serde_json::json!({"raw": "mixed-surface-mutated"}))
        .await
        .assert_status_ok();

    let output = run_admin(data_dir, &["admin", "user", "offline", &user_id]);
    assert!(output.status.success(), "{output:?}");
    wait_for_offline_transition().await;

    server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .await
        .assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let (ch, cv) = client_id_header(&client_id);
    server
        .get(&format!("/v1/client/get-child-version/{}", Uuid::nil()))
        .add_header(ch, cv)
        .await
        .assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    let output = run_admin(
        data_dir,
        &[
            "admin",
            "restore",
            "--input",
            backup_tmp.path().to_str().unwrap(),
            "--user-id",
            &user_id,
            "-y",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let output = run_admin(data_dir, &["admin", "user", "online", &user_id]);
    assert!(output.status.success(), "{output:?}");
    wait_for_offline_transition().await;

    let tasks = server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .await
        .json::<serde_json::Value>();
    assert_eq!(tasks.as_array().unwrap().len(), 1);

    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{}", Uuid::nil()))
        .add_header(ch, cv)
        .await;
    resp.assert_status_ok();
}

#[tokio::test]
async fn test_negative_operator_paths_keep_user_offline_until_resolved() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let (user_id, token) = create_user(data_dir, "negative-operator-user").await;
    let (client_id, _secret) = create_device(data_dir, &user_id, "Broken Device").await;

    let server = start_server(data_dir.to_path_buf()).await;
    let auth = auth_header(&token);

    let backup_tmp = TempDir::new().unwrap();
    create_backup_snapshot(data_dir, backup_tmp.path());

    let output = run_admin(data_dir, &["admin", "user", "offline", &user_id]);
    assert!(output.status.success(), "{output:?}");
    wait_for_offline_transition().await;

    let missing_user = "does-not-exist";
    let output = run_admin(
        data_dir,
        &[
            "admin",
            "restore",
            "--input",
            backup_tmp.path().to_str().unwrap(),
            "--user-id",
            missing_user,
            "-y",
        ],
    );
    assert!(
        !output.status.success(),
        "restore should fail for missing user in backup: {output:?}"
    );

    let db_path = data_dir.join("config.sqlite");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE devices SET encryption_secret_enc = NULL WHERE client_id = ?1",
        [&client_id],
    )
    .unwrap();

    let output = run_admin(data_dir, &["admin", "user", "assess", &user_id]);
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    assert!(
        stdout.contains("needs_operator_attention") || stdout.contains("NeedsOperatorAttention"),
        "{stdout}"
    );

    server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth)
        .await
        .assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    let _ = stderr_string(&output);
}
