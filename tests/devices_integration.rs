//! Integration tests for the device registry (POST/GET/DELETE/PATCH /api/devices)
//! and sync endpoint device validation (registered → OK, unregistered → 403, revoked → 403).

mod common;

use std::sync::Arc;

use axum::http::{header, HeaderValue};
use axum::Router;
use axum_test::TestServer;
use base64::Engine as _;
use serde_json::{json, Value};
use uuid::Uuid;

use cmdock_server::app_state::AppState;
use cmdock_server::runtime_policy::{RuntimeAccessMode, RuntimeDeleteAction, RuntimePolicy};
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::tc_sync;
use cmdock_server::tc_sync::crypto::SyncCryptor;

fn auth_header(token: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

fn client_id_header(client_id: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::HeaderName::from_static("x-client-id"),
        HeaderValue::from_str(client_id).unwrap(),
    )
}

const HS_CT: &str = "application/vnd.taskchampion.history-segment";
const SNAP_CT: &str = "application/vnd.taskchampion.snapshot";

/// Returns (server, store, user_id, token, tmp_dir)
async fn setup() -> (
    TestServer,
    Arc<dyn ConfigStore>,
    String,
    String,
    tempfile::TempDir,
) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "deviceuser".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();
    let token = store.create_api_token(&user.id, None).await.unwrap();

    std::fs::create_dir_all(tmp.path().join("users").join(&user.id)).unwrap();

    // Create a master key and replica (required for device registration with HKDF)
    let master_key: [u8; 32] = [42u8; 32];
    let raw_secret = b"test-master-encryption-secret!!!";
    let encrypted = cmdock_server::crypto::encrypt_secret(raw_secret, &master_key).unwrap();
    let enc_b64 = base64::engine::general_purpose::STANDARD.encode(&encrypted);
    let replica_client_id = Uuid::new_v4().to_string();
    store
        .create_replica(&user.id, &replica_client_id, &enc_b64)
        .await
        .unwrap();

    let config = common::test_server_config_with_master_key(tmp.path().to_path_buf(), master_key);
    let state = AppState::new(store.clone(), &config);

    let app = Router::new()
        .merge(cmdock_server::devices::routes())
        .merge(tc_sync::routes())
        .with_state(state);
    let server = TestServer::new(app);

    (server, store, user.id, token, tmp)
}

/// Transport-only setup for exercising per-device sync storage without the bridge.
async fn setup_transport_only_device() -> (TestServer, String, String, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "transport-only-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();

    let client_id = Uuid::new_v4().to_string();
    store
        .create_device(&user.id, &client_id, "Transport device", None)
        .await
        .unwrap();
    std::fs::create_dir_all(tmp.path().join("users").join(&user.id)).unwrap();

    let config = common::test_server_config(tmp.path().to_path_buf());
    let state = AppState::new(store, &config);
    let app = Router::new().merge(tc_sync::routes()).with_state(state);
    let server = TestServer::new(app);

    (server, client_id, "transport-secret".to_string(), tmp)
}

/// Helper: register a device and return (client_id, encryption_secret)
async fn register_device(
    server: &TestServer,
    h: &header::HeaderName,
    v: &HeaderValue,
    name: &str,
) -> (String, String) {
    let resp = server
        .post("/api/devices")
        .add_header(h.clone(), v.clone())
        .json(&json!({ "name": name }))
        .await;
    resp.assert_status(axum::http::StatusCode::CREATED);
    let body: Value = resp.json();
    let client_id = body["clientId"].as_str().unwrap().to_string();
    let secret = body["encryptionSecret"].as_str().unwrap().to_string();
    assert!(!client_id.is_empty());
    assert!(!secret.is_empty());
    // Verify taskrcLines are present
    assert!(body["taskrcLines"].as_array().unwrap().len() >= 3);
    (client_id, secret)
}

#[tokio::test]
async fn test_register_and_list_device() {
    let (server, _store, user_id, token, tmp) = setup().await;
    let (h, v) = auth_header(&token);

    let (client_id, _secret) = register_device(&server, &h, &v, "Test iPhone").await;

    let legacy_path = tmp
        .path()
        .join("users")
        .join(&user_id)
        .join("sync")
        .join(format!("{client_id}.sqlite"));
    assert!(
        !legacy_path.exists(),
        "normal device registration should not create legacy per-device sync DBs"
    );

    // List devices
    let resp = server
        .get("/api/devices")
        .add_header(h.clone(), v.clone())
        .await;
    resp.assert_status_ok();
    let devices: Vec<Value> = resp.json();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["clientId"], client_id);
    assert_eq!(devices[0]["name"], "Test iPhone");
    assert_eq!(devices[0]["status"], "active");
}

#[tokio::test]
async fn test_register_multiple_devices() {
    let (server, _store, _user_id, token, _tmp) = setup().await;
    let (h, v) = auth_header(&token);

    let (id1, secret1) = register_device(&server, &h, &v, "iPhone").await;
    let (id2, secret2) = register_device(&server, &h, &v, "MacBook").await;

    // Different client_ids and secrets
    assert_ne!(id1, id2);
    assert_ne!(secret1, secret2);

    let resp = server
        .get("/api/devices")
        .add_header(h.clone(), v.clone())
        .await;
    let devices: Vec<Value> = resp.json();
    assert_eq!(devices.len(), 2);
}

#[tokio::test]
async fn test_register_invalid_name() {
    let (server, _store, _user_id, token, _tmp) = setup().await;
    let (h, v) = auth_header(&token);

    // Empty name
    let resp = server
        .post("/api/devices")
        .add_header(h.clone(), v.clone())
        .json(&json!({ "name": "" }))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

    // Whitespace-only name
    let resp = server
        .post("/api/devices")
        .add_header(h.clone(), v.clone())
        .json(&json!({ "name": "   " }))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

    // Control characters
    let resp = server
        .post("/api/devices")
        .add_header(h.clone(), v.clone())
        .json(&json!({ "name": "bad\nname" }))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

    // Overlong name
    let resp = server
        .post("/api/devices")
        .add_header(h.clone(), v.clone())
        .json(&json!({ "name": "x".repeat(256) }))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_register_device_blocked_by_runtime_policy() {
    let (server, store, user_id, token, _tmp) = setup().await;
    let policy = RuntimePolicy {
        runtime_access: RuntimeAccessMode::Block,
        delete_action: RuntimeDeleteAction::Allow,
    };
    store
        .upsert_runtime_policy(
            &user_id,
            "policy-v1",
            &policy,
            Some("policy-v1"),
            Some(&policy),
            Some("2026-04-03 12:00:00"),
        )
        .await
        .unwrap();

    let (h, v) = auth_header(&token);
    let resp = server
        .post("/api/devices")
        .add_header(h, v)
        .json(&json!({ "name": "Blocked Device" }))
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
    assert!(resp.text().contains("Runtime access blocked by policy"));
}

#[tokio::test]
async fn test_revoke_device() {
    let (server, _store, _user_id, token, _tmp) = setup().await;
    let (h, v) = auth_header(&token);

    let (client_id, _) = register_device(&server, &h, &v, "To Revoke").await;

    // Revoke
    let resp = server
        .delete(&format!("/api/devices/{client_id}"))
        .add_header(h.clone(), v.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::NO_CONTENT);

    // Verify status is revoked
    let resp = server
        .get("/api/devices")
        .add_header(h.clone(), v.clone())
        .await;
    let devices: Vec<Value> = resp.json();
    let revoked = devices.iter().find(|d| d["clientId"] == client_id).unwrap();
    assert_eq!(revoked["status"], "revoked");
}

#[tokio::test]
async fn test_revoke_nonexistent_device() {
    let (server, _store, _user_id, token, _tmp) = setup().await;
    let (h, v) = auth_header(&token);

    let resp = server
        .delete(&format!("/api/devices/{}", Uuid::new_v4()))
        .add_header(h.clone(), v.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_rename_device() {
    let (server, _store, _user_id, token, _tmp) = setup().await;
    let (h, v) = auth_header(&token);

    let (client_id, _) = register_device(&server, &h, &v, "Old Name").await;

    let resp = server
        .patch(&format!("/api/devices/{client_id}"))
        .add_header(h.clone(), v.clone())
        .json(&json!({ "name": "New Name" }))
        .await;
    resp.assert_status_ok();

    let resp = server
        .get("/api/devices")
        .add_header(h.clone(), v.clone())
        .await;
    let devices: Vec<Value> = resp.json();
    let device = devices.iter().find(|d| d["clientId"] == client_id).unwrap();
    assert_eq!(device["name"], "New Name");
}

#[tokio::test]
async fn test_rename_device_rejects_control_chars() {
    let (server, _store, _user_id, token, _tmp) = setup().await;
    let (h, v) = auth_header(&token);
    let (client_id, _secret) = register_device(&server, &h, &v, "Laptop").await;

    let resp = server
        .patch(&format!("/api/devices/{client_id}"))
        .add_header(h.clone(), v.clone())
        .json(&json!({ "name": "bad\tname" }))
        .await;

    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_unregistered_device_sync_forbidden() {
    let (server, _store, _user_id, _token, _tmp) = setup().await;

    // Random client_id with no device registration → 403
    let client_id = Uuid::new_v4().to_string();
    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{}", Uuid::nil()))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(vec![0u8; 10].into())
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_revoked_device_sync_forbidden() {
    let (server, _store, _user_id, token, _tmp) = setup().await;
    let (h, v) = auth_header(&token);

    let (client_id, _) = register_device(&server, &h, &v, "To Revoke").await;

    // Revoke the device
    server
        .delete(&format!("/api/devices/{client_id}"))
        .add_header(h.clone(), v.clone())
        .await;

    // Attempt sync → 403
    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{}", Uuid::nil()))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(vec![0u8; 10].into())
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_registered_device_sync_rejects_invalid_plaintext_payload() {
    let (server, _store, _user_id, token, _tmp) = setup().await;
    let (h, v) = auth_header(&token);

    let (client_id, _) = register_device(&server, &h, &v, "Sync Device").await;

    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{}", Uuid::nil()))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(vec![0u8; 10].into())
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_registered_device_encrypted_sync_round_trip() {
    let (server, client_id, secret, _tmp) = setup_transport_only_device().await;
    let client_uuid = Uuid::parse_str(&client_id).unwrap();
    let cryptor = SyncCryptor::new(client_uuid, secret.as_bytes()).unwrap();

    let parent = Uuid::nil();
    let encrypted = cryptor.seal(parent, b"history-v1").unwrap();

    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{parent}"))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(encrypted.into())
        .await;
    resp.assert_status_ok();
    let version_id = Uuid::parse_str(resp.header("X-Version-Id").to_str().unwrap()).unwrap();

    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{parent}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status_ok();
    let encrypted_body = resp.into_bytes();
    let decrypted = cryptor.unseal(parent, encrypted_body.as_ref()).unwrap();
    assert_eq!(decrypted, b"history-v1");

    // Verify the handler returns the server-generated version id.
    assert_eq!(
        resp_header_uuid(&server, &client_id, parent).await,
        version_id,
        "child lookup should expose the stored version id"
    );
}

#[tokio::test]
async fn test_registered_device_encrypted_snapshot_round_trip() {
    let (server, client_id, secret, _tmp) = setup_transport_only_device().await;
    let client_uuid = Uuid::parse_str(&client_id).unwrap();
    let cryptor = SyncCryptor::new(client_uuid, secret.as_bytes()).unwrap();

    let parent = Uuid::nil();
    let encrypted_history = cryptor.seal(parent, b"history-v1").unwrap();

    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{parent}"))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(encrypted_history.into())
        .await;
    resp.assert_status_ok();
    let version_id = Uuid::parse_str(resp.header("X-Version-Id").to_str().unwrap()).unwrap();

    let encrypted_snapshot = cryptor.seal(version_id, b"snapshot-v1").unwrap();
    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-snapshot/{version_id}"))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(SNAP_CT))
        .bytes(encrypted_snapshot.into())
        .await;
    resp.assert_status_ok();

    let (ch, cv) = client_id_header(&client_id);
    let resp = server.get("/v1/client/snapshot").add_header(ch, cv).await;
    resp.assert_status_ok();
    let encrypted_body = resp.into_bytes();
    let decrypted = cryptor.unseal(version_id, encrypted_body.as_ref()).unwrap();
    assert_eq!(decrypted, b"snapshot-v1");
}

#[tokio::test]
async fn test_registered_device_without_stored_secret_fails_loudly() {
    let (server, store, user_id, _token, _tmp) = setup().await;

    let client_id = Uuid::new_v4().to_string();
    store
        .create_device(&user_id, &client_id, "Broken Device", None)
        .await
        .unwrap();

    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{}", Uuid::nil()))
        .add_header(ch, cv)
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static(HS_CT))
        .bytes(vec![0u8; 8].into())
        .await;
    resp.assert_status(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
}

async fn resp_header_uuid(server: &TestServer, client_id: &str, parent: Uuid) -> Uuid {
    let (ch, cv) = client_id_header(client_id);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{parent}"))
        .add_header(ch, cv)
        .await;
    Uuid::parse_str(resp.header("X-Version-Id").to_str().unwrap()).unwrap()
}

#[tokio::test]
async fn test_devices_require_auth() {
    let (server, _store, _user_id, _token, _tmp) = setup().await;

    let resp = server.get("/api/devices").await;
    resp.assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_device_secrets_are_unique() {
    let (server, _store, _user_id, token, _tmp) = setup().await;
    let (h, v) = auth_header(&token);

    let (_, secret1) = register_device(&server, &h, &v, "Device 1").await;
    let (_, secret2) = register_device(&server, &h, &v, "Device 2").await;
    let (_, secret3) = register_device(&server, &h, &v, "Device 3").await;

    assert_ne!(secret1, secret2);
    assert_ne!(secret2, secret3);
    assert_ne!(secret1, secret3);
}
