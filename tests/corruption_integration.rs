//! Integration tests for SQLite corruption resilience.
//!
//! Tests quarantine behaviour: auto-quarantine on corruption detection,
//! manual offline/online cycling, cross-surface quarantine propagation,
//! integrity checks, and recovery after corruption.

mod common;

use std::path::Path;
use std::sync::Arc;

use axum::http::{header, HeaderValue, Method};
use axum::Router;
use axum_test::TestServer;
use serde_json::Value;
use tempfile::TempDir;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use cmdock_server::admin;
use cmdock_server::app_config;
use cmdock_server::app_state::AppState;
use cmdock_server::config_api;
use cmdock_server::health;
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::summary;
use cmdock_server::sync;
use cmdock_server::tasks;
use cmdock_server::tc_sync;
use cmdock_server::views;

// --- Setup types and helpers ---

struct UserInfo {
    user_id: String,
    token: String,
    client_id: String,
}

struct TestEnv {
    server: TestServer,
    _tmp: TempDir,
    user_a: UserInfo,
    user_b: UserInfo,
}

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

fn corrupt_file(path: &Path) {
    std::fs::write(path, b"THIS IS NOT A SQLITE DATABASE").unwrap();
}

fn delete_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

async fn create_user_with_token_and_client(
    store: &Arc<dyn ConfigStore>,
    username: &str,
    tmp: &TempDir,
) -> UserInfo {
    let user = store
        .create_user(&NewUser {
            username: username.to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();

    let client_id = Uuid::new_v4().to_string();
    store
        .create_replica(&user.id, &client_id, "test-enc-secret")
        .await
        .unwrap();
    store
        .create_device(&user.id, &client_id, "Test device", None)
        .await
        .unwrap();

    std::fs::create_dir_all(tmp.path().join("users").join(&user.id)).unwrap();

    UserInfo {
        user_id: user.id,
        token,
        client_id,
    }
}

async fn setup() -> TestEnv {
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

    let user_a = create_user_with_token_and_client(&store, "user_a", &tmp).await;
    let user_b = create_user_with_token_and_client(&store, "user_b", &tmp).await;

    let config =
        common::test_server_config_with_admin_token(data_dir.clone(), user_a.token.clone());

    let state = AppState::new(store, &config);

    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(admin::routes())
        .with_state(state.clone())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .merge(tc_sync::routes().with_state(state))
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);

    TestEnv {
        server,
        _tmp: tmp,
        user_a,
        user_b,
    }
}

// --- Tests ---

#[tokio::test]
async fn test_corrupt_replica_returns_error() {
    let env = setup().await;

    // Warm cache: add a task so the replica is opened and cached
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+test Warm cache task"}))
        .await;
    resp.assert_status_ok();

    // GET tasks should work
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();

    // Evict cached replica BEFORE corrupting, so the connection is dropped
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .post(&format!("/admin/user/{}/evict", env.user_a.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    // Corrupt the replica file (TC uses .sqlite3 extension)
    let replica_dir = env._tmp.path().join("users").join(&env.user_a.user_id);
    let tc_db = replica_dir.join("taskchampion.sqlite3");
    assert!(
        tc_db.exists(),
        "TC database should exist after adding a task"
    );
    corrupt_file(&tc_db);

    // Next GET should fail with 503 — our corruption detection recognises
    // TC's "Setting journal_mode=WAL" error on a non-DB file and triggers
    // auto-quarantine.
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // Verify auto-quarantine was set
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .get(&format!("/admin/user/{}/stats", env.user_a.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(
        body["quarantined"], true,
        "User should be auto-quarantined after corruption detection"
    );

    // Follow-on blocking: a second GET /api/tasks must also return 503,
    // proving caches were evicted and quarantine is actively blocking.
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_manual_quarantine_sets_quarantined_flag() {
    let env = setup().await;

    // Quarantine via admin endpoint
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post(&format!("/admin/user/{}/offline", env.user_a.user_id))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // Verify quarantine via admin stats
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .get(&format!("/admin/user/{}/stats", env.user_a.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["quarantined"], true);
}

#[tokio::test]
async fn test_quarantine_blocks_all_task_endpoints() {
    let env = setup().await;

    // Quarantine user_a via admin endpoint
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .post(&format!("/admin/user/{}/offline", env.user_a.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    // GET /api/tasks → 503
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // POST /api/tasks → 503
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+test Should be blocked"}))
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    let dummy_uuid = Uuid::nil();

    // POST /api/tasks/{uuid}/done → 503
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{dummy_uuid}/done"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // POST /api/tasks/{uuid}/delete → 503
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{dummy_uuid}/delete"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // POST /api/tasks/{uuid}/modify → 503
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{dummy_uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "should be blocked"}))
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // POST /v1/client/add-snapshot/{version} → 503 (quarantine checked in open_sync_storage)
    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let resp = env
        .server
        .post(&format!("/v1/client/add-snapshot/{dummy_uuid}"))
        .add_header(ch, cv)
        .content_type("application/vnd.taskchampion.snapshot")
        .bytes(b"snapshot-data".to_vec().into())
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // GET /v1/client/snapshot → 503
    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let resp = env
        .server
        .get("/v1/client/snapshot")
        .add_header(ch, cv)
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // user_b should still work
    let (h, v) = auth_header(&env.user_b.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();
}

#[tokio::test]
async fn test_quarantine_blocks_sync_endpoints() {
    let env = setup().await;

    // Quarantine user_a
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post(&format!("/admin/user/{}/offline", env.user_a.user_id))
        .add_header(h, v)
        .await
        .assert_status_ok();

    let nil = Uuid::nil();

    // Sync add-version for quarantined user → 503
    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let resp = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type("application/vnd.taskchampion.history-segment")
        .bytes(b"data".to_vec().into())
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // Sync get-child-version for quarantined user → 503
    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let resp = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_other_user_unaffected_during_quarantine() {
    let env = setup().await;

    // Quarantine user_a
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post(&format!("/admin/user/{}/offline", env.user_a.user_id))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // user_b REST still works
    let (h, v) = auth_header(&env.user_b.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();

    // user_b sync add-version still works
    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&env.user_b.client_id);
    let resp = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type("application/vnd.taskchampion.history-segment")
        .bytes(b"user-b-data".to_vec().into())
        .await;
    resp.assert_status_ok();
}

#[tokio::test]
async fn test_admin_offline_online_cycle() {
    let env = setup().await;

    // Offline
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post(&format!("/admin/user/{}/offline", env.user_a.user_id))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // Verify quarantined
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .get(&format!("/admin/user/{}/stats", env.user_a.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    assert_eq!(resp.json::<Value>()["quarantined"], true);

    // Online
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post(&format!("/admin/user/{}/online", env.user_a.user_id))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // Verify not quarantined
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .get(&format!("/admin/user/{}/stats", env.user_a.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    assert_eq!(resp.json::<Value>()["quarantined"], false);

    // GET /api/tasks works again
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();
}

#[tokio::test]
async fn test_admin_online_idempotent() {
    let env = setup().await;

    // Online when not quarantined
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .post(&format!("/admin/user/{}/online", env.user_a.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("was not quarantined"));
}

#[tokio::test]
async fn test_integrity_check_healthy_db() {
    let env = setup().await;

    // Seed a task so the replica DB exists
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+test Integrity test task"}))
        .await
        .assert_status_ok();

    // Run integrity check
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .get(&format!(
            "/admin/user/{}/stats?integrity=quick",
            env.user_a.user_id
        ))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();

    // The integrity_check field should be present when integrity=quick is requested
    assert!(
        body["integrity_check"].is_object(),
        "integrity_check should be present in response"
    );

    // Admin handler uses taskchampion.sqlite3 (matching TC's actual filename).
    let ic = &body["integrity_check"];
    assert_eq!(
        ic["replica"].as_str(),
        Some("ok"),
        "Healthy replica should report integrity_check.replica = 'ok', got: {}",
        ic["replica"]
    );

    // Create the shared sync DB by making a sync request, then verify its integrity
    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let _resp = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch, cv)
        .await;

    // Re-run integrity check — now the shared sync DB should exist and be healthy
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .get(&format!(
            "/admin/user/{}/stats?integrity=quick",
            env.user_a.user_id
        ))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let ic = &body["integrity_check"];
    let sync_results = ic["sync"]
        .as_array()
        .expect("integrity_check.sync should be an array");
    assert!(
        !sync_results.is_empty(),
        "Healthy sync integrity should report at least one sync DB result, got: {}",
        ic["sync"]
    );
    let sync_result = sync_results
        .iter()
        .filter_map(|entry| entry.as_str())
        .find(|entry| entry.starts_with("sync.sqlite:"))
        .expect("integrity_check.sync should include shared sync.sqlite");
    assert!(
        sync_result == "sync.sqlite: ok",
        "Healthy shared sync DB should report ok, got: {sync_result}"
    );
}

#[tokio::test]
async fn test_integrity_check_corrupt_db() {
    let env = setup().await;

    // Seed a task so the replica DB exists
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+test Corrupt check task"}))
        .await
        .assert_status_ok();

    // Evict cache before corrupting, to avoid stale connection issues
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post(&format!("/admin/user/{}/evict", env.user_a.user_id))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // Corrupt the replica file (admin handler checks taskchampion.sqlite3)
    let replica_dir = env._tmp.path().join("users").join(&env.user_a.user_id);
    corrupt_file(&replica_dir.join("taskchampion.sqlite3"));

    // Integrity check should report the corruption
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .get(&format!(
            "/admin/user/{}/stats?integrity=quick",
            env.user_a.user_id
        ))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();

    let ic = &body["integrity_check"];
    // The replica field must be a non-"ok" string since we corrupted the file
    let replica_result = ic["replica"]
        .as_str()
        .expect("integrity_check.replica should be a string");
    assert_ne!(
        replica_result, "ok",
        "Corrupted replica should not report 'ok', got: {replica_result}"
    );

    // Also corrupt the shared sync DB and verify integrity_check.sync reports an error.
    let sync_db = replica_dir.join("sync.sqlite");
    corrupt_file(&sync_db);

    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .get(&format!(
            "/admin/user/{}/stats?integrity=quick",
            env.user_a.user_id
        ))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let ic = &body["integrity_check"];
    let sync_results = ic["sync"]
        .as_array()
        .expect("integrity_check.sync should be an array");
    assert!(
        sync_results.iter().any(|entry| {
            entry
                .as_str()
                .is_some_and(|value| value.contains("sync.sqlite") && !value.ends_with(": ok"))
        }),
        "Corrupted shared sync DB should not report ok, got: {}",
        ic["sync"]
    );
}

#[tokio::test]
async fn test_recovery_after_quarantine() {
    let env = setup().await;

    // Warm cache with a task
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+test Recovery task"}))
        .await
        .assert_status_ok();

    // Quarantine the user (simulates corruption detection)
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post(&format!("/admin/user/{}/offline", env.user_a.user_id))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // Verify blocked
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // Delete the replica file (simulates a restore — TC creates fresh on next open)
    let replica_dir = env._tmp.path().join("users").join(&env.user_a.user_id);
    let tc_db = replica_dir.join("taskchampion.sqlite3");
    delete_file(&tc_db);
    delete_file(&tc_db.with_extension("sqlite3-wal"));
    delete_file(&tc_db.with_extension("sqlite3-shm"));

    // Bring user back online
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post(&format!("/admin/user/{}/online", env.user_a.user_id))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // Should recover — TC creates a fresh replica
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();
    let tasks: Vec<Value> = resp.json();
    assert!(tasks.is_empty(), "Fresh replica should have no tasks");
}

#[tokio::test]
async fn test_cross_surface_quarantine() {
    let env = setup().await;

    // Quarantine user_a via admin (simulates corruption detection on any surface)
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post(&format!("/admin/user/{}/offline", env.user_a.user_id))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // REST task endpoint should be blocked
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // Sync endpoint should also be blocked (quarantine is shared via AppState)
    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let resp = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type("application/vnd.taskchampion.history-segment")
        .bytes(b"should-be-blocked".to_vec().into())
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // Sync get-child-version should also be blocked
    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let resp = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_corruption_triggered_quarantine_blocks_cross_surface() {
    let env = setup().await;

    // Seed tasks for user_a to warm the replica cache
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+test Cross-surface corruption task"}))
        .await
        .assert_status_ok();

    // Also make a sync request to warm the sync storage cache
    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let _resp = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch, cv)
        .await;

    // Evict sync cache so the next sync request re-opens the file
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .post(&format!("/admin/user/{}/evict", env.user_a.user_id))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // Corrupt the shared sync file
    let replica_dir = env._tmp.path().join("users").join(&env.user_a.user_id);
    let sync_db = replica_dir.join("sync.sqlite");
    if sync_db.exists() {
        corrupt_file(&sync_db);
    } else {
        // If the shared sync DB doesn't exist yet, create it as corrupt
        std::fs::write(&sync_db, b"THIS IS NOT A SQLITE DATABASE").unwrap();
    }

    // Make a sync request — should trigger corruption detection and quarantine.
    // SyncStorage::open uses rusqlite::Connection::open directly, so corruption
    // errors (PRAGMA journal_mode=WAL on a non-DB file) flow through
    // is_corruption_in_chain in open_sync_storage, triggering auto-quarantine
    // and returning 503.
    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let resp = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // Verify auto-quarantine was set
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .get(&format!("/admin/user/{}/stats", env.user_a.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(
        body["quarantined"], true,
        "User should be auto-quarantined after sync corruption detection"
    );

    // Cross-surface proof: REST GET /api/tasks must also return 503
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
}
