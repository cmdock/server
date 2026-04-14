mod common;

use std::process::Command;
use std::sync::Arc;

use axum::http::{header, HeaderValue};
use axum::Router;
use axum_test::TestServer;
use tempfile::TempDir;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

use cmdock_server::app_state::AppState;
use cmdock_server::health;
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::tasks;
use cmdock_server::tc_sync;
use cmdock_server::tc_sync::crypto::SyncCryptor;

fn admin_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cmdock-server")
}

fn master_key_hex() -> String {
    "2a".repeat(32)
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

fn version_id(resp: &axum_test::TestResponse) -> Uuid {
    Uuid::parse_str(resp.header("X-Version-Id").to_str().unwrap()).unwrap()
}

async fn start_sync_server(data_dir: std::path::PathBuf, db_path: &std::path::Path) -> TestServer {
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let config = common::test_server_config(data_dir);
    let state = AppState::new(store, &config);
    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(cmdock_server::devices::routes())
        .merge(tc_sync::routes())
        .with_state(state);
    TestServer::new(app)
}

fn client_id_header(client_id: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::HeaderName::from_static("x-client-id"),
        HeaderValue::from_str(client_id).unwrap(),
    )
}

const HS_CT: &str = "application/vnd.taskchampion.history-segment";

#[tokio::test]
async fn test_self_hosted_offline_cli_device_onboarding_flow() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "offline-onboarding-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();
    std::fs::create_dir_all(data_dir.join("users").join(&user.id)).unwrap();

    // Provision while the server is not running.
    let output = run_admin(tmp.path(), &["admin", "sync", "create", &user.id]);
    assert!(output.status.success(), "{output:?}");

    let output = run_admin(
        tmp.path(),
        &[
            "admin",
            "device",
            "create",
            &user.id,
            "--name",
            "Offline Provisioned iPhone",
            "--server-url",
            "https://sync.example.com",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    let client_id = extract_field(&stdout, "Client ID:");
    let secret = extract_field(&stdout, "Encryption Secret:");

    // Now start the server against that already-provisioned data dir.
    let server = start_sync_server(data_dir.clone(), &db_path).await;

    let client_uuid = Uuid::parse_str(&client_id).unwrap();
    let cryptor = SyncCryptor::new(client_uuid, secret.as_bytes()).unwrap();

    let parent = Uuid::nil();
    let encrypted = cryptor.seal(parent, b"offline-onboarding-history").unwrap();
    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{parent}"))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(encrypted.into())
        .await;
    resp.assert_status_ok();
    let first_version = version_id(&resp);

    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{parent}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status_ok();
    let encrypted_body = resp.into_bytes();
    let decrypted = cryptor.unseal(parent, encrypted_body.as_ref()).unwrap();
    assert_eq!(decrypted, b"offline-onboarding-history");

    // Revoke while the server is running via the local/break-glass admin CLI.
    let output = run_admin(
        tmp.path(),
        &["admin", "device", "revoke", &user.id, &client_id, "-y"],
    );
    assert!(output.status.success(), "{output:?}");

    let encrypted = cryptor
        .seal(first_version, b"blocked-after-revoke")
        .unwrap();
    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{first_version}"))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(encrypted.into())
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);

    // Unrevoke and confirm the same device identity works again.
    let output = run_admin(
        tmp.path(),
        &["admin", "device", "unrevoke", &user.id, &client_id, "-y"],
    );
    assert!(output.status.success(), "{output:?}");

    let encrypted = cryptor
        .seal(first_version, b"works-after-unrevoke")
        .unwrap();
    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{first_version}"))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(encrypted.into())
        .await;
    resp.assert_status_ok();

    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{first_version}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status_ok();
    let encrypted_body = resp.into_bytes();
    let decrypted = cryptor
        .unseal(first_version, encrypted_body.as_ref())
        .unwrap();
    assert_eq!(decrypted, b"works-after-unrevoke");

    // Delete after revoking again, and confirm the device can no longer authenticate.
    let output = run_admin(
        tmp.path(),
        &["admin", "device", "revoke", &user.id, &client_id, "-y"],
    );
    assert!(output.status.success(), "{output:?}");

    let output = run_admin(
        tmp.path(),
        &["admin", "device", "delete", &user.id, &client_id, "-y"],
    );
    assert!(output.status.success(), "{output:?}");

    let encrypted = cryptor
        .seal(first_version, b"blocked-after-delete")
        .unwrap();
    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{first_version}"))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(encrypted.into())
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_self_hosted_cli_user_to_tw_device_flow() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();
    let db_path = data_dir.join("config.sqlite");

    let output = run_admin(
        tmp.path(),
        &["admin", "user", "create", "--username", "tw-e2e-user"],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    let user_id = extract_indented_value(&stdout, "ID:");
    let api_token = stdout.lines().last().map(str::trim).unwrap().to_string();
    assert!(!user_id.is_empty());
    assert!(!api_token.is_empty());

    let output = run_admin(tmp.path(), &["admin", "sync", "create", &user_id]);
    assert!(output.status.success(), "{output:?}");

    let output = run_admin(
        tmp.path(),
        &[
            "admin",
            "device",
            "create",
            &user_id,
            "--name",
            "Taskwarrior Laptop",
            "--server-url",
            "https://sync.example.com",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    let client_id = extract_field(&stdout, "Client ID:");
    let secret = extract_field(&stdout, "Encryption Secret:");

    let server = start_sync_server(data_dir.clone(), &db_path).await;

    let client_uuid = Uuid::parse_str(&client_id).unwrap();
    let cryptor = SyncCryptor::new(client_uuid, secret.as_bytes()).unwrap();
    let parent = Uuid::nil();

    let encrypted = cryptor.seal(parent, b"tw-e2e-history").unwrap();
    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{parent}"))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(encrypted.into())
        .await;
    resp.assert_status_ok();

    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{parent}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status_ok();
    let encrypted_body = resp.into_bytes();
    let decrypted = cryptor.unseal(parent, encrypted_body.as_ref()).unwrap();
    assert_eq!(decrypted, b"tw-e2e-history");

    let auth = HeaderValue::from_str(&format!("Bearer {api_token}")).unwrap();
    let resp = server
        .get("/api/devices")
        .add_header(header::AUTHORIZATION, auth)
        .await;
    resp.assert_status_ok();
    let devices: serde_json::Value = resp.json();
    assert_eq!(devices.as_array().unwrap().len(), 1);
    assert_eq!(devices[0]["clientId"], client_id);
    assert_eq!(devices[0]["name"], "Taskwarrior Laptop");
}

#[tokio::test]
async fn test_self_hosted_cli_offline_online_cycle_against_running_server() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();
    let db_path = data_dir.join("config.sqlite");

    let output = run_admin(
        tmp.path(),
        &["admin", "user", "create", "--username", "recovery-user"],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    let user_id = extract_indented_value(&stdout, "ID:");
    let api_token = stdout.lines().last().map(str::trim).unwrap().to_string();

    let server = start_sync_server(data_dir.clone(), &db_path).await;
    let auth = HeaderValue::from_str(&format!("Bearer {api_token}")).unwrap();

    let resp = server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .await;
    resp.assert_status_ok();

    let output = run_admin(tmp.path(), &["admin", "user", "offline", &user_id]);
    assert!(output.status.success(), "{output:?}");
    sleep(Duration::from_millis(600)).await;

    let resp = server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    let output = run_admin(tmp.path(), &["admin", "user", "online", &user_id]);
    assert!(output.status.success(), "{output:?}");
    sleep(Duration::from_millis(600)).await;

    let resp = server
        .get("/api/tasks")
        .add_header(header::AUTHORIZATION, auth)
        .await;
    resp.assert_status_ok();
}

#[tokio::test]
async fn test_device_registration_and_revocation_survive_restart() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "restart-persistence-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();
    std::fs::create_dir_all(data_dir.join("users").join(&user.id)).unwrap();

    let output = run_admin(tmp.path(), &["admin", "sync", "create", &user.id]);
    assert!(output.status.success(), "{output:?}");

    let output = run_admin(
        tmp.path(),
        &[
            "admin",
            "device",
            "create",
            &user.id,
            "--name",
            "Persistent MacBook",
            "--server-url",
            "https://sync.example.com",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    let active_client_id = extract_field(&stdout, "Client ID:");
    let active_secret = extract_field(&stdout, "Encryption Secret:");

    let output = run_admin(
        tmp.path(),
        &[
            "admin",
            "device",
            "create",
            &user.id,
            "--name",
            "Revoked iPhone",
            "--server-url",
            "https://sync.example.com",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    let revoked_client_id = extract_field(&stdout, "Client ID:");

    let output = run_admin(
        tmp.path(),
        &[
            "admin",
            "device",
            "revoke",
            &user.id,
            &revoked_client_id,
            "-y",
        ],
    );
    assert!(output.status.success(), "{output:?}");

    let server = start_sync_server(data_dir.clone(), &db_path).await;

    let active_uuid = Uuid::parse_str(&active_client_id).unwrap();
    let active_cryptor = SyncCryptor::new(active_uuid, active_secret.as_bytes()).unwrap();
    let parent = Uuid::nil();
    let encrypted = active_cryptor
        .seal(parent, b"persists-across-restart")
        .unwrap();
    let (ch, cv) = client_id_header(&active_client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{parent}"))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(encrypted.into())
        .await;
    resp.assert_status_ok();

    let (ch, cv) = client_id_header(&revoked_client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{parent}"))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(vec![0u8; 8].into())
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);

    drop(server);

    let restarted = start_sync_server(data_dir.clone(), &db_path).await;

    let (ch, cv) = client_id_header(&active_client_id);
    let resp = restarted
        .get(&format!("/v1/client/get-child-version/{parent}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status_ok();
    let encrypted_body = resp.into_bytes();
    let decrypted = active_cryptor
        .unseal(parent, encrypted_body.as_ref())
        .unwrap();
    assert_eq!(decrypted, b"persists-across-restart");

    let (ch, cv) = client_id_header(&revoked_client_id);
    let resp = restarted
        .post(&format!("/v1/client/add-version/{parent}"))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(vec![0u8; 8].into())
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
}
