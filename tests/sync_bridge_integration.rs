//! Integration tests for the sync bridge (Phase 3).
//!
//! Verifies that REST API mutations appear in the TC sync version chain,
//! and that TC sync pushes appear in the REST API task list.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, HeaderValue};
use axum::Router;
use axum_test::TestServer;
use serde_json::Value;
use uuid::Uuid;

use cmdock_server::app_state::AppState;
use cmdock_server::health;
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::tasks;
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

/// Generate a random 32-byte master key for testing.
fn test_master_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    use ring::rand::{SecureRandom, SystemRandom};
    SystemRandom::new().fill(&mut key).unwrap();
    key
}

struct TestEnv {
    server: TestServer,
    state: AppState,
    store: Arc<dyn ConfigStore>,
    bearer_token: String,
    client_id: String,
    #[allow(dead_code)]
    user_id: String,
    master_key: [u8; 32],
    replica_secret: [u8; 32],
    /// Hex-encoded encryption secret (matching TW CLI's PBKDF2 input format)
    encryption_secret: Vec<u8>,
    _tmp: tempfile::TempDir,
}

async fn count_sync_versions(env: &TestEnv) -> usize {
    let (ch, cv) = client_id_header(&env.client_id);
    let mut version_count = 0;
    let mut parent = Uuid::nil();

    loop {
        let resp = env
            .server
            .get(&format!("/v1/client/get-child-version/{parent}"))
            .add_header(ch.clone(), cv.clone())
            .await;
        if resp.status_code() == axum::http::StatusCode::NOT_FOUND {
            break;
        }
        resp.assert_status_ok();
        parent = Uuid::parse_str(resp.header("X-Version-Id").to_str().unwrap()).unwrap();
        version_count += 1;
    }

    version_count
}

async fn wait_for_sync_versions(env: &TestEnv, expected: usize) -> usize {
    let mut last = 0;
    for _ in 0..30 {
        last = count_sync_versions(env).await;
        if last >= expected {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    last
}

/// Set up a test server with:
/// - A user with bearer token (for REST API) AND replica with encrypted secret (for sync)
/// - master_key configured (enables sync bridge)
/// - Both task routes and TC sync routes
async fn setup() -> TestEnv {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().to_path_buf();

    let db_path = data_dir.join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let master_key = test_master_key();

    // Create user
    let user = store
        .create_user(&NewUser {
            username: "bridge_user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    // Create bearer token for REST API auth
    let bearer_token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();

    // Generate encryption secret and encrypt with master key
    let mut secret_bytes = [0u8; 32];
    use ring::rand::{SecureRandom, SystemRandom};
    SystemRandom::new().fill(&mut secret_bytes).unwrap();

    let encrypted = cmdock_server::crypto::encrypt_secret(&secret_bytes, &master_key).unwrap();
    let encrypted_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &encrypted);

    // Register canonical sync identity
    let client_id = Uuid::new_v4().to_string();
    store
        .create_replica(&user.id, &client_id, &encrypted_b64)
        .await
        .unwrap();
    let device_secret_raw =
        cmdock_server::crypto::derive_device_secret(&secret_bytes, client_id.as_bytes()).unwrap();
    let device_secret_enc =
        cmdock_server::crypto::encrypt_secret(&device_secret_raw, &master_key).unwrap();
    let device_secret_enc_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &device_secret_enc,
    );
    store
        .create_device(
            &user.id,
            &client_id,
            "Test device",
            Some(&device_secret_enc_b64),
        )
        .await
        .unwrap();

    // Create user data directory
    std::fs::create_dir_all(data_dir.join("users").join(&user.id)).unwrap();

    let config = common::test_server_config_with_master_key(data_dir, master_key);

    let state = AppState::new(store.clone(), &config);

    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(tc_sync::routes())
        .with_state(state.clone());

    let server = TestServer::new(app);

    // Store the per-device secret, matching what TW CLI receives from
    // `admin device create` and what the Phase 2B bridge uses.
    let secret_hex = hex::encode(device_secret_raw);

    TestEnv {
        server,
        state,
        store,
        bearer_token,
        client_id,
        user_id: user.id,
        master_key,
        replica_secret: secret_bytes,
        encryption_secret: secret_hex.into_bytes(),
        _tmp: tmp,
    }
}

async fn register_additional_device(env: &TestEnv, name: &str) -> (String, Vec<u8>) {
    let client_id = Uuid::new_v4().to_string();
    let device_secret_raw =
        cmdock_server::crypto::derive_device_secret(&env.replica_secret, client_id.as_bytes())
            .unwrap();
    let device_secret_enc =
        cmdock_server::crypto::encrypt_secret(&device_secret_raw, &env.master_key).unwrap();
    let device_secret_enc_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &device_secret_enc,
    );

    env.store
        .create_device(&env.user_id, &client_id, name, Some(&device_secret_enc_b64))
        .await
        .unwrap();

    (client_id, hex::encode(device_secret_raw).into_bytes())
}

async fn push_tc_task_from_device(
    server: &TestServer,
    client_id: &str,
    encryption_secret: &[u8],
    description: &str,
) -> Uuid {
    let client_uuid = Uuid::parse_str(client_id).unwrap();
    let cryptor = SyncCryptor::new(client_uuid, encryption_secret).unwrap();

    let task_uuid = Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
    let version_json = serde_json::json!({
        "operations": [
            { "Create": { "uuid": task_uuid.to_string() } },
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "status",
                "value": "pending",
                "timestamp": now
            }},
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "description",
                "value": description,
                "timestamp": now
            }},
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "entry",
                "value": now,
                "timestamp": now
            }},
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "modified",
                "value": now,
                "timestamp": now
            }}
        ]
    });
    let plaintext = serde_json::to_vec(&version_json).unwrap();
    let nil = Uuid::nil();
    let encrypted = cryptor.seal(nil, &plaintext).unwrap();
    let (ch, cv) = client_id_header(client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .add_header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.taskchampion.history-segment"),
        )
        .bytes(encrypted.into())
        .await;
    resp.assert_status_ok();
    task_uuid
}

/// Test that creating a task via REST API produces a version in the sync chain.
///
/// Flow: POST /api/tasks → sync bridge pushes → GET /v1/client/get-child-version/{nil}
#[tokio::test]
async fn test_rest_create_appears_in_sync_chain() {
    let env = setup().await;

    let (ah, av) = auth_header(&env.bearer_token);

    // Create a task via REST API
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(ah, av)
        .json(&serde_json::json!({ "raw": "Buy milk project:shopping" }))
        .await;
    resp.assert_status_ok();

    // Now read the sync version chain — there should be at least one version
    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&env.client_id);
    let resp = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch, cv)
        .await;

    // Should return 200 with a version (the bridge pushed the REST change)
    resp.assert_status_ok();
    let vid_hdr = resp.header("X-Version-Id");
    Uuid::parse_str(vid_hdr.to_str().unwrap()).expect("X-Version-Id should be a valid UUID");
}

/// Test that completing and deleting tasks also appear in the sync chain.
#[tokio::test]
async fn test_rest_mutations_appear_in_sync_chain() {
    let env = setup().await;

    let (ah, av) = auth_header(&env.bearer_token);

    // Create a task
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(ah.clone(), av.clone())
        .json(&serde_json::json!({ "raw": "Test task" }))
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    // Extract UUID from "Created task {uuid}."
    let output = body["output"].as_str().unwrap();
    let uuid = output
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap();

    // Complete the task
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/done"))
        .add_header(ah.clone(), av.clone())
        .await;
    resp.assert_status_ok();

    let version_count = wait_for_sync_versions(&env, 1).await;
    assert!(
        version_count >= 1,
        "expected at least 1 sync version after queued create + complete, got {version_count}"
    );
}

/// Test that the sync bridge is a no-op when master_key is not configured.
#[tokio::test]
async fn test_no_master_key_skips_sync() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().to_path_buf();

    let db_path = data_dir.join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "nokey_user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let bearer_token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();

    let client_id = Uuid::new_v4().to_string();
    store
        .create_replica(&user.id, &client_id, "dummy-enc-secret")
        .await
        .unwrap();
    store
        .create_device(&user.id, &client_id, "Test device", None)
        .await
        .unwrap();

    std::fs::create_dir_all(data_dir.join("users").join(&user.id)).unwrap();

    // No master_key
    let config = common::test_server_config(data_dir);

    let state = AppState::new(store, &config);
    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(tc_sync::routes())
        .with_state(state);
    let server = TestServer::new(app);

    let (ah, av) = auth_header(&bearer_token);

    // Create a task — should succeed without errors even though sync bridge can't run
    let resp = server
        .post("/api/tasks")
        .add_header(ah.clone(), av.clone())
        .json(&serde_json::json!({ "raw": "Task without sync" }))
        .await;
    resp.assert_status_ok();

    // Sync chain should be empty (no bridge pushing)
    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch, cv)
        .await;
    // 404 = no versions in the chain
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
}

/// Test that reading tasks uses the canonical replica directly.
///
/// REST reads no longer trigger a bridge pull first. This verifies that
/// canonical state remains readable without a read-time bridge step.
#[tokio::test]
async fn test_list_tasks_reads_canonical_replica() {
    let env = setup().await;

    let (ah, av) = auth_header(&env.bearer_token);

    // Create two tasks via REST
    for desc in &["Task A", "Task B"] {
        let resp = env
            .server
            .post("/api/tasks")
            .add_header(ah.clone(), av.clone())
            .json(&serde_json::json!({ "raw": desc }))
            .await;
        resp.assert_status_ok();
    }

    // List tasks — reads come straight from the canonical replica.
    let resp = env
        .server
        .get("/api/tasks")
        .add_header(ah.clone(), av.clone())
        .await;
    resp.assert_status_ok();
    let tasks: Vec<Value> = resp.json();
    assert_eq!(
        tasks.len(),
        2,
        "should see both tasks from canonical storage"
    );
}

/// Test that modify_task pushes changes to sync chain.
#[tokio::test]
async fn test_modify_pushes_to_sync_chain() {
    let env = setup().await;

    let (ah, av) = auth_header(&env.bearer_token);

    // Create a task
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(ah.clone(), av.clone())
        .json(&serde_json::json!({ "raw": "Original description" }))
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let output = body["output"].as_str().unwrap();
    let uuid = output
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap();

    // Modify the task
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(ah.clone(), av.clone())
        .json(&serde_json::json!({ "description": "Updated description" }))
        .await;
    resp.assert_status_ok();

    // REST writes now enqueue bridge work and may be coalesced into a single
    // device-chain sync. Poll briefly for eventual convergence.
    let version_count = wait_for_sync_versions(&env, 1).await;

    assert!(
        version_count >= 1,
        "should have at least 1 sync version after queued create + modify, got {version_count}"
    );
}

/// Test TC→REST flow: encrypt operations, POST to sync endpoint, then
/// verify the task appears via GET /api/tasks.
///
/// This simulates a TC CLI client pushing encrypted operations through
/// the sync protocol, then reading back through the REST API.
#[tokio::test]
async fn test_tc_push_appears_in_rest_api() {
    let env = setup().await;

    // Create a SyncCryptor with the same credentials as the server has
    let client_uuid = Uuid::parse_str(&env.client_id).unwrap();
    let cryptor = SyncCryptor::new(client_uuid, &env.encryption_secret).unwrap();

    // Build a TC history segment (JSON-serialised operations).
    // Format matches taskchampion's internal Version struct:
    //   {"operations":[{"Create":{"uuid":"..."}},{"Update":{...}},...]}`
    let task_uuid = Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
    let version_json = serde_json::json!({
        "operations": [
            { "Create": { "uuid": task_uuid.to_string() } },
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "status",
                "value": "pending",
                "timestamp": now
            }},
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "description",
                "value": "Task from TC CLI",
                "timestamp": now
            }},
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "entry",
                "value": now,
                "timestamp": now
            }},
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "modified",
                "value": now,
                "timestamp": now
            }}
        ]
    });
    let plaintext = serde_json::to_vec(&version_json).unwrap();

    // Encrypt with parent_version_id = nil (first version in chain).
    // TC's SyncServer uses parent_version_id as the AAD version_id for history segments.
    let nil = Uuid::nil();
    let encrypted = cryptor.seal(nil, &plaintext).unwrap();

    // POST encrypted operations to the TC sync endpoint
    let (ch, cv) = client_id_header(&env.client_id);
    let resp = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch.clone(), cv.clone())
        .add_header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.taskchampion.history-segment"),
        )
        .bytes(encrypted.into())
        .await;
    resp.assert_status_ok();

    // Verify the version was stored
    let version_id_str = resp.header("X-Version-Id").to_str().unwrap().to_string();
    Uuid::parse_str(&version_id_str).expect("X-Version-Id should be a valid UUID");

    // Now read via REST API. GET /api/tasks triggers sync_user_replica which
    // pulls the encrypted version from the sync chain, decrypts it, and
    // applies the operations to the user's replica.
    let (ah, av) = auth_header(&env.bearer_token);
    let resp = env.server.get("/api/tasks").add_header(ah, av).await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    // Find our task by UUID
    let found = tasks.iter().any(|t| {
        t["uuid"].as_str() == Some(&task_uuid.to_string())
            && t["description"].as_str() == Some("Task from TC CLI")
    });
    assert!(
        found,
        "Task pushed via TC sync should appear in REST API. Got tasks: {tasks:?}"
    );
}

#[tokio::test]
async fn test_tc_write_degrades_to_queued_bridge_fallback_on_timeout() {
    let env = setup().await;

    let replica = env
        .state
        .replica_manager
        .get_replica(&env.user_id)
        .await
        .unwrap();
    let held_replica = replica.clone();
    let hold = tokio::spawn(async move {
        let _guard = held_replica.lock().await;
        tokio::time::sleep(Duration::from_secs(7)).await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let client_uuid = Uuid::parse_str(&env.client_id).unwrap();
    let cryptor = SyncCryptor::new(client_uuid, &env.encryption_secret).unwrap();
    let task_uuid = Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
    let version_json = serde_json::json!({
        "operations": [
            { "Create": { "uuid": task_uuid.to_string() } },
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "status",
                "value": "pending",
                "timestamp": now
            }},
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "description",
                "value": "Queued fallback task",
                "timestamp": now
            }},
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "entry",
                "value": now,
                "timestamp": now
            }},
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "modified",
                "value": now,
                "timestamp": now
            }}
        ]
    });
    let plaintext = serde_json::to_vec(&version_json).unwrap();
    let nil = Uuid::nil();
    let encrypted = cryptor.seal(nil, &plaintext).unwrap();

    let start = std::time::Instant::now();
    let (ch, cv) = client_id_header(&env.client_id);
    let resp = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .add_header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.taskchampion.history-segment"),
        )
        .bytes(encrypted.into())
        .await;
    resp.assert_status_ok();
    assert!(
        start.elapsed() < Duration::from_secs(7),
        "TC write should fall back and return before the blocked replica lock is released"
    );

    let (ah, av) = auth_header(&env.bearer_token);
    hold.await.unwrap();

    let mut found = false;
    for _ in 0..40 {
        let resp = env
            .server
            .get("/api/tasks")
            .add_header(ah.clone(), av.clone())
            .await;
        resp.assert_status_ok();
        let tasks: Vec<Value> = resp.json();
        if tasks.iter().any(|t| {
            t["uuid"].as_str() == Some(&task_uuid.to_string())
                && t["description"].as_str() == Some("Queued fallback task")
        }) {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        found,
        "queued bridge fallback should eventually reconcile the TC write into canonical state"
    );
}

#[tokio::test]
async fn test_stale_second_device_read_triggers_targeted_sync_and_marks_fresh() {
    let env = setup().await;
    let (device2_id, device2_secret) = register_additional_device(&env, "Second device").await;

    let device2_uuid = Uuid::parse_str(&device2_id).unwrap();
    let device2_cryptor = SyncCryptor::new(device2_uuid, &device2_secret).unwrap();

    let (ah, av) = auth_header(&env.bearer_token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(ah.clone(), av.clone())
        .json(&serde_json::json!({ "raw": "Freshness sync task project:bridge" }))
        .await;
    resp.assert_status_ok();

    assert!(
        env.state
            .runtime_sync
            .device_needs_sync(&env.user_id, &device2_id),
        "secondary device should be marked stale after a canonical REST write"
    );

    let nil = Uuid::nil();
    let (ch2, cv2) = client_id_header(&device2_id);
    let mut decrypted = None;
    for _ in 0..80 {
        let resp = env
            .server
            .get(&format!("/v1/client/get-child-version/{nil}"))
            .add_header(ch2.clone(), cv2.clone())
            .await;
        if resp.status_code() == axum::http::StatusCode::OK {
            let encrypted_body = resp.into_bytes();
            decrypted = Some(
                device2_cryptor
                    .unseal(nil, encrypted_body.as_ref())
                    .unwrap(),
            );
            break;
        }
        assert_eq!(
            resp.status_code(),
            axum::http::StatusCode::NOT_FOUND,
            "stale device read should either synchronise or report no child yet"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let decrypted = decrypted.expect("device 2 should eventually receive the new version");
    assert!(
        !decrypted.is_empty(),
        "targeted sync should return a non-empty history segment for the stale device"
    );

    let mut fresh = false;
    for _ in 0..20 {
        if !env
            .state
            .runtime_sync
            .device_needs_sync(&env.user_id, &device2_id)
        {
            fresh = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        fresh,
        "targeted TC read should eventually mark the stale device as fresh"
    );

    let resp = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch2, cv2)
        .await;
    resp.assert_status_ok();
    let encrypted_body = resp.into_bytes();
    let decrypted = device2_cryptor
        .unseal(nil, encrypted_body.as_ref())
        .unwrap();
    assert!(
        !decrypted.is_empty(),
        "fresh device should still read a non-empty history segment on repeated reads"
    );
    assert!(
        !env.state
            .runtime_sync
            .device_needs_sync(&env.user_id, &device2_id),
        "fresh device should remain marked in sync on repeated reads"
    );
}

#[tokio::test]
async fn test_tc_write_from_device_a_eventually_reaches_device_b() {
    let env = setup().await;
    let (device2_id, device2_secret) = register_additional_device(&env, "Second device").await;
    let description = "TC device A task for device B";

    let _task_uuid = push_tc_task_from_device(
        &env.server,
        &env.client_id,
        &env.encryption_secret,
        description,
    )
    .await;
    cmdock_server::sync_bridge::sync_user_replica(&env.state, &env.user_id)
        .await
        .unwrap();

    let device2_uuid = Uuid::parse_str(&device2_id).unwrap();
    let device2_cryptor = SyncCryptor::new(device2_uuid, &device2_secret).unwrap();
    let nil = Uuid::nil();
    let (ch2, cv2) = client_id_header(&device2_id);

    let mut received = false;
    for _ in 0..80 {
        let resp = env
            .server
            .get(&format!("/v1/client/get-child-version/{nil}"))
            .add_header(ch2.clone(), cv2.clone())
            .await;

        if resp.status_code() == axum::http::StatusCode::OK {
            let encrypted_body = resp.into_bytes();
            let decrypted = device2_cryptor
                .unseal(nil, encrypted_body.as_ref())
                .unwrap();
            let text = String::from_utf8_lossy(&decrypted);
            if text.contains(description) {
                received = true;
                break;
            }
        } else {
            assert_eq!(resp.status_code(), axum::http::StatusCode::NOT_FOUND);
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(
        received,
        "device B should eventually receive the task written by device A"
    );
}
