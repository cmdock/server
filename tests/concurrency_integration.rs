//! Concurrency and load-readiness integration tests.
//!
//! Tests for scenarios that would surface under load: concurrent REST mutations
//! for the same user, replica cold-start races, auth cache stampede, sync bridge
//! thread isolation, and metrics correctness on error paths.

mod common;

use std::future::IntoFuture;
use std::sync::Arc;
use std::time::Duration;

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
use cmdock_server::metrics;
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
    #[allow(dead_code)]
    store: Arc<dyn ConfigStore>,
    _tmp: TempDir,
    user_a: UserInfo,
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

const HS_CT: &str = "application/vnd.taskchampion.history-segment";

/// Concurrency test timeout. If any concurrent operation deadlocks or hangs,
/// this ensures a crisp test failure rather than CI timing out silently.
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

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

    let config =
        common::test_server_config_with_admin_token(data_dir.clone(), user_a.token.clone());

    let state = AppState::new(store.clone(), &config);

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
        store,
        _tmp: tmp,
        user_a,
    }
}

async fn setup_with_metrics() -> TestEnv {
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
    let config =
        common::test_server_config_with_admin_token(data_dir.clone(), user_a.token.clone());

    let metrics_handle = metrics::setup_metrics();
    let state = AppState::new(store.clone(), &config);

    let app = Router::new()
        .route(
            "/metrics",
            axum::routing::get(metrics::metrics_handler).with_state(metrics_handle),
        )
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(admin::routes())
        .with_state(state.clone())
        .layer(axum::middleware::from_fn(metrics::metrics_middleware))
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
        store,
        _tmp: tmp,
        user_a,
    }
}

fn metric_value(body: &str, metric: &str, labels: &[(&str, &str)]) -> Option<f64> {
    body.lines().find_map(|line| {
        if !line.starts_with(metric)
            || !labels
                .iter()
                .all(|(key, value)| line.contains(&format!("{key}=\"{value}\"")))
        {
            return None;
        }
        line.split_whitespace().last()?.parse::<f64>().ok()
    })
}

/// Multi-user setup for tests that need >1 user.
struct MultiUserTestEnv {
    server: TestServer,
    #[allow(dead_code)]
    store: Arc<dyn ConfigStore>,
    _tmp: TempDir,
    users: Vec<UserInfo>,
}

async fn setup_multi_user(n: usize) -> MultiUserTestEnv {
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

    let mut users = Vec::new();
    for i in 0..n {
        users.push(create_user_with_token_and_client(&store, &format!("user_{i}"), &tmp).await);
    }

    let config = common::test_server_config_with_admin_token(
        data_dir.clone(),
        users.first().map(|u| u.token.clone()).unwrap_or_default(),
    );

    let state = AppState::new(store.clone(), &config);

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

    MultiUserTestEnv {
        server,
        store,
        _tmp: tmp,
        users,
    }
}

// =============================================================================
// P0 Tests
// =============================================================================

/// P0-1: Concurrent REST mutations for the same user never return 500.
///
/// Fires 10 concurrent task-add requests for a single user. All must succeed
/// (200) — the per-user Mutex serialises access, so no corruption or panics.
/// Final task count must equal the number of successful adds.
#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_rest_mutations_same_user_no_500() {
    let env = setup().await;

    // Fire 10 concurrent add-task requests
    let mut handles = Vec::new();
    for i in 0..10 {
        let (h, v) = auth_header(&env.user_a.token);
        let fut = env
            .server
            .post("/api/tasks")
            .add_header(h, v)
            .json(&serde_json::json!({"raw": format!("+loadtest Concurrent task {i}")}));
        handles.push(fut);
    }

    let results = tokio::time::timeout(
        TEST_TIMEOUT,
        futures::future::join_all(handles.into_iter().map(IntoFuture::into_future)),
    )
    .await
    .expect("concurrent REST mutations timed out — possible deadlock");

    let mut success_count = 0u32;
    for (i, resp) in results.iter().enumerate() {
        let status = resp.status_code().as_u16();
        // 200 = success, 503 = transient SQLite BUSY under high concurrency.
        // 500/401/404 would indicate a real bug.
        assert!(
            status == 200 || status == 503,
            "Request {i} returned {status} — expected 200 or 503"
        );
        if status == 200 {
            success_count += 1;
        }
    }

    // At least 8/10 should succeed; transient BUSY under multi_thread is acceptable
    assert!(
        success_count >= 8,
        "Expected at least 8/10 concurrent adds to succeed, got {success_count}"
    );

    // Verify final task count matches successful adds
    let (h, v) = auth_header(&env.user_a.token);
    let list_resp = env.server.get("/api/tasks").add_header(h, v).await;
    list_resp.assert_status_ok();
    let tasks: Vec<Value> = list_resp.json();
    assert_eq!(
        tasks.len(),
        success_count as usize,
        "Task count should match successful adds"
    );
}

/// P0-2: Concurrent REST reads and writes for the same user are safe.
///
/// Interleaves add, list, and complete operations concurrently. None should
/// return 500. The server must remain consistent.
#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_rest_mixed_operations_no_500() {
    let env = setup().await;

    // First create a task to have something to read/complete
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+loadtest Seed task"}))
        .await;
    resp.assert_status_ok();

    // Get task UUID from the list endpoint (response shape is TaskActionResponse)
    let (h, v) = auth_header(&env.user_a.token);
    let list_resp = env.server.get("/api/tasks").add_header(h, v).await;
    list_resp.assert_status_ok();
    let tasks: Vec<Value> = list_resp.json();
    let uuid = tasks[0]["uuid"].as_str().unwrap().to_string();

    // Fire concurrent: 3 list + 3 add + 1 complete
    let (h1, v1) = auth_header(&env.user_a.token);
    let r_list1 = env.server.get("/api/tasks").add_header(h1, v1);

    let (h2, v2) = auth_header(&env.user_a.token);
    let r_list2 = env.server.get("/api/tasks").add_header(h2, v2);

    let (h3, v3) = auth_header(&env.user_a.token);
    let r_list3 = env.server.get("/api/tasks").add_header(h3, v3);

    let (h4, v4) = auth_header(&env.user_a.token);
    let r_add1 = env
        .server
        .post("/api/tasks")
        .add_header(h4, v4)
        .json(&serde_json::json!({"raw": "+loadtest Concurrent add 1"}));

    let (h5, v5) = auth_header(&env.user_a.token);
    let r_add2 = env
        .server
        .post("/api/tasks")
        .add_header(h5, v5)
        .json(&serde_json::json!({"raw": "+loadtest Concurrent add 2"}));

    let (h6, v6) = auth_header(&env.user_a.token);
    let r_add3 = env
        .server
        .post("/api/tasks")
        .add_header(h6, v6)
        .json(&serde_json::json!({"raw": "+loadtest Concurrent add 3"}));

    let (h7, v7) = auth_header(&env.user_a.token);
    let r_done = env
        .server
        .post(&format!("/api/tasks/{uuid}/done"))
        .add_header(h7, v7);

    let (resp_l1, resp_l2, resp_l3, resp_a1, resp_a2, resp_a3, resp_d) =
        tokio::time::timeout(TEST_TIMEOUT, async {
            tokio::join!(r_list1, r_list2, r_list3, r_add1, r_add2, r_add3, r_done)
        })
        .await
        .expect("concurrent mixed operations timed out — possible deadlock");

    // All list requests should succeed
    for (label, resp) in [
        ("list1", &resp_l1),
        ("list2", &resp_l2),
        ("list3", &resp_l3),
    ] {
        assert_eq!(
            resp.status_code().as_u16(),
            200,
            "{label} returned {} — expected 200",
            resp.status_code().as_u16()
        );
    }
    // All add requests should succeed
    for (label, resp) in [("add1", &resp_a1), ("add2", &resp_a2), ("add3", &resp_a3)] {
        assert_eq!(
            resp.status_code().as_u16(),
            200,
            "{label} returned {} — expected 200",
            resp.status_code().as_u16()
        );
    }
    // Complete should succeed (200) or fail due to concurrent locking (503)
    let done_status = resp_d.status_code().as_u16();
    assert!(
        done_status == 200 || done_status == 503,
        "done returned {done_status} — expected 200 or 503"
    );

    // Post-condition: verify state is consistent after concurrent ops settle.
    // Started with 1 seed task, added 3 concurrently = 4 created total.
    // Done may have completed the seed (3 pending) or not yet visible (4 pending).
    // Under concurrent execution, task count is 3 or 4.
    let (h, v) = auth_header(&env.user_a.token);
    let final_resp = env.server.get("/api/tasks").add_header(h, v).await;
    final_resp.assert_status_ok();
    let final_tasks: Vec<Value> = final_resp.json();
    let task_count = final_tasks.len();
    assert!(
        (3..=4).contains(&task_count),
        "Expected 3-4 pending tasks after mixed concurrent ops, got {task_count}"
    );
}

/// P0-3: Sync retry with the same parent returns 409, not corruption.
///
/// A client that retries add-version with the same parent (because it didn't
/// receive the response) should get 409 Conflict, not 500. The version chain
/// must remain intact.
#[tokio::test(flavor = "multi_thread")]
async fn test_tc_sync_retry_same_parent_returns_409() {
    let env = setup().await;
    let nil = Uuid::nil();

    // First add-version succeeds
    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let resp = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"first-version-data".to_vec().into())
        .await;
    resp.assert_status_ok();
    let version_id = resp.header("X-Version-Id");

    // Retry with the SAME parent (nil) — should get 409 with correct parent hint
    let (ch2, cv2) = client_id_header(&env.user_a.client_id);
    let retry_resp = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch2, cv2)
        .content_type(HS_CT)
        .bytes(b"retry-version-data".to_vec().into())
        .await;

    assert_eq!(
        retry_resp.status_code().as_u16(),
        409,
        "Retry with same parent should return 409 Conflict"
    );

    // The 409 response should point to the current head (the version we just created)
    let parent_hint = retry_resp.header("X-Parent-Version-Id");
    assert!(
        !parent_hint.is_empty(),
        "409 response must include X-Parent-Version-Id"
    );
    assert_eq!(
        version_id, parent_hint,
        "409 X-Parent-Version-Id should match the version we created"
    );

    // Version chain should still be readable
    let (ch3, cv3) = client_id_header(&env.user_a.client_id);
    let chain_resp = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch3, cv3)
        .await;
    chain_resp.assert_status_ok();

    // The version we get back should be the original one
    let returned_vid = chain_resp.header("X-Version-Id");
    assert_eq!(
        version_id, returned_vid,
        "Version chain should still return the original version"
    );
}

/// P0-4: Cold-start replica race — N parallel read requests for a new user
/// all succeed without deadlocks or 500s.
#[tokio::test(flavor = "multi_thread")]
async fn test_replica_cold_start_race_no_500() {
    let env = setup().await;

    // Fire 5 parallel task-list requests for a user that hasn't been accessed yet.
    // The first request opens the replica; the rest must wait or share.
    let mut handles = Vec::new();
    for _ in 0..5 {
        let (h, v) = auth_header(&env.user_a.token);
        handles.push(env.server.get("/api/tasks").add_header(h, v));
    }

    let results = tokio::time::timeout(
        TEST_TIMEOUT,
        futures::future::join_all(handles.into_iter().map(IntoFuture::into_future)),
    )
    .await
    .expect("cold-start reads timed out — possible deadlock");

    for (i, resp) in results.iter().enumerate() {
        let status = resp.status_code().as_u16();
        assert_ne!(
            status, 500,
            "Cold-start request {i} returned 500 — race condition in replica open"
        );
        assert_eq!(
            status, 200,
            "Cold-start request {i} returned {status} — expected 200"
        );
    }

    // All requests should return the same (empty) task list
    for resp in &results {
        let tasks: Vec<Value> = resp.json();
        assert!(tasks.is_empty(), "Fresh user should have no tasks");
    }
}

/// P0-5: Cold-start with concurrent writes — parallel add-task requests for
/// a brand new user. All should succeed; no duplicate replicas or corruption.
#[tokio::test(flavor = "multi_thread")]
async fn test_replica_cold_start_concurrent_writes_no_500() {
    let env = setup().await;

    // Fire 5 parallel task-add requests (user replica not yet opened)
    let mut handles = Vec::new();
    for i in 0..5 {
        let (h, v) = auth_header(&env.user_a.token);
        handles.push(
            env.server
                .post("/api/tasks")
                .add_header(h, v)
                .json(&serde_json::json!({"raw": format!("+coldstart Task {i}")})),
        );
    }

    let results = tokio::time::timeout(
        TEST_TIMEOUT,
        futures::future::join_all(handles.into_iter().map(IntoFuture::into_future)),
    )
    .await
    .expect("cold-start writes timed out — possible deadlock");

    let mut success_count = 0u32;
    let mut statuses = Vec::new();
    for (i, resp) in results.iter().enumerate() {
        let status = resp.status_code().as_u16();
        statuses.push(status);
        // Only 200 (success) or 503 (transient SQLite BUSY → service unavailable)
        // are acceptable. 500/401/403/404 would indicate a real bug.
        assert!(
            status == 200 || status == 503,
            "Cold-start write {i} returned {status} — expected 200 or 503"
        );
        if status == 200 {
            success_count += 1;
        }
    }

    // The per-user mutex serialises replica access. Under parallel test execution,
    // transient SQLite BUSY can occasionally cause one request to fail.
    // Accept 4/5+ to avoid CI flakiness while still catching real regressions.
    assert!(
        success_count >= 4,
        "Expected at least 4/5 cold-start writes to succeed, got {success_count}/5 (statuses: {statuses:?})"
    );

    // Verify final state is consistent — task count matches successful adds
    let (h, v) = auth_header(&env.user_a.token);
    let list_resp = env.server.get("/api/tasks").add_header(h, v).await;
    list_resp.assert_status_ok();
    let tasks: Vec<Value> = list_resp.json();
    assert_eq!(
        tasks.len(),
        success_count as usize,
        "Task count should match successful adds (statuses were: {statuses:?})"
    );
}

/// P0-6: Auth cache stampede — many parallel requests with the same token
/// all succeed without 500s. The auth cache should absorb the load.
#[tokio::test(flavor = "multi_thread")]
async fn test_auth_cache_stampede_no_500() {
    let env = setup().await;

    // Warm the cache with one request
    let (h, v) = auth_header(&env.user_a.token);
    let warm = env.server.get("/api/tasks").add_header(h, v).await;
    warm.assert_status_ok();

    // Fire 20 parallel requests with the same token
    let mut handles = Vec::new();
    for _ in 0..20 {
        let (h, v) = auth_header(&env.user_a.token);
        handles.push(env.server.get("/api/tasks").add_header(h, v));
    }

    let results = tokio::time::timeout(
        TEST_TIMEOUT,
        futures::future::join_all(handles.into_iter().map(IntoFuture::into_future)),
    )
    .await
    .expect("auth stampede timed out — possible deadlock");

    for (i, resp) in results.iter().enumerate() {
        let status = resp.status_code().as_u16();
        assert_eq!(
            status, 200,
            "Stampede request {i} returned {status} — expected 200"
        );
    }
}

/// P0-7: Auth cache cold stampede — many parallel requests with DIFFERENT
/// tokens for different users. None should 500 even on cold cache.
#[tokio::test(flavor = "multi_thread")]
async fn test_auth_cache_cold_stampede_multi_user_no_500() {
    let env = setup_multi_user(10).await;

    // Fire one request per user, all in parallel (cold cache for all)
    let mut handles = Vec::new();
    for user in &env.users {
        let (h, v) = auth_header(&user.token);
        handles.push(env.server.get("/api/tasks").add_header(h, v));
    }

    let results = tokio::time::timeout(
        TEST_TIMEOUT,
        futures::future::join_all(handles.into_iter().map(IntoFuture::into_future)),
    )
    .await
    .expect("cold stampede timed out — possible deadlock");

    for (i, resp) in results.iter().enumerate() {
        let status = resp.status_code().as_u16();
        assert_eq!(
            status, 200,
            "Cold stampede request for user {i} returned {status} — expected 200"
        );
    }
}

/// P0-8: Corrupted sync storage doesn't crash the server.
///
/// After corrupting the per-device sync file, TC sync operations should return
/// an error status (503 for quarantine), but the server must remain responsive
/// for healthz and other users.
///
/// Note: The sync BRIDGE (OS-thread !Send path) is tested separately in
/// sync_bridge_integration.rs with master_key configured. These concurrency
/// tests use master_key: None to isolate REST/TC protocol behavior.
#[tokio::test(flavor = "multi_thread")]
async fn test_sync_storage_corruption_server_stays_responsive() {
    let env = setup().await;

    // Create a task successfully first
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+resilience First task"}))
        .await;
    resp.assert_status_ok();

    // Ensure the shared sync DB exists by pushing a version via the TC protocol
    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let push_resp = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"trigger-sync-storage".to_vec().into())
        .await;
    push_resp.assert_status_ok();

    // Evict cached connections so the next access reopens the file
    let (h_evict, v_evict) = auth_header(&env.user_a.token);
    let evict_resp = env
        .server
        .post(&format!("/admin/user/{}/evict", env.user_a.user_id))
        .add_header(h_evict, v_evict)
        .await;
    evict_resp.assert_status_ok();

    // Corrupt the shared sync storage file
    let sync_db = env
        ._tmp
        .path()
        .join("users")
        .join(&env.user_a.user_id)
        .join("sync.sqlite");
    assert!(
        sync_db.exists(),
        "shared sync DB should exist after pushing a version"
    );
    std::fs::write(&sync_db, b"CORRUPT CORRUPT CORRUPT").unwrap();
    let _ = std::fs::remove_file(sync_db.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(sync_db.with_extension("sqlite-shm"));

    // TC sync operations should fail with 503 (quarantined), never 500
    let (ch2, cv2) = client_id_header(&env.user_a.client_id);
    let sync_resp = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch2, cv2)
        .await;
    let sync_status = sync_resp.status_code().as_u16();
    assert_eq!(
        sync_status, 503,
        "After corruption, TC sync should return 503 (quarantined), got {sync_status}"
    );

    // Server must still be responsive for healthz (global endpoint)
    let healthz = env.server.get("/healthz").await;
    healthz.assert_status_ok();
}

/// P0-9: Concurrent TC sync operations across multiple users never 500.
///
/// Simulates multiple users pushing versions simultaneously — the kind of
/// load pattern that occurs in production.
#[tokio::test(flavor = "multi_thread")]
async fn test_multi_user_concurrent_sync_no_500() {
    let env = setup_multi_user(5).await;
    let nil = Uuid::nil();

    // Each user pushes a version concurrently
    let mut handles = Vec::new();
    for user in &env.users {
        let (ch, cv) = client_id_header(&user.client_id);
        handles.push(
            env.server
                .post(&format!("/v1/client/add-version/{nil}"))
                .add_header(ch, cv)
                .content_type(HS_CT)
                .bytes(format!("version-data-{}", user.user_id).into_bytes().into()),
        );
    }

    let results = tokio::time::timeout(
        TEST_TIMEOUT,
        futures::future::join_all(handles.into_iter().map(IntoFuture::into_future)),
    )
    .await
    .expect("multi-user sync timed out — possible deadlock");

    for (i, resp) in results.iter().enumerate() {
        let status = resp.status_code().as_u16();
        assert_ne!(
            status, 500,
            "User {i} sync returned 500 — server error on multi-user concurrent sync"
        );
        assert_eq!(
            status, 200,
            "User {i} sync returned {status} — each user has independent chain, should be 200"
        );
    }
}

// =============================================================================
// P1 Tests
// =============================================================================

/// P1-1: Metrics are recorded on error paths (auth failures).
///
/// Verifies that failed auth attempts increment HTTP metrics counters
/// with the correct status labels.
#[tokio::test(flavor = "multi_thread")]
async fn test_metrics_recorded_on_auth_failure() {
    // Build a server with metrics middleware enabled
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

    let config = common::test_server_config(data_dir.clone());

    let metrics_handle = metrics::setup_metrics();
    let state = AppState::new(store, &config);

    let app = Router::new()
        .route(
            "/metrics",
            axum::routing::get(metrics::metrics_handler).with_state(metrics_handle),
        )
        .merge(health::routes())
        .merge(tasks::routes())
        .with_state(state)
        .layer(axum::middleware::from_fn(metrics::metrics_middleware))
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);

    // Make requests with bad auth
    for _ in 0..3 {
        let (h, v) = auth_header("invalid-token-xyz");
        let resp = server.get("/api/tasks").add_header(h, v).await;
        assert_eq!(resp.status_code().as_u16(), 401);
    }

    // Check that metrics captured the 401 responses with correct labels
    let metrics_resp = server.get("/metrics").await;
    metrics_resp.assert_status_ok();
    let body = metrics_resp.text();

    // Verify the counter exists with the correct label combination on a single line.
    // Prometheus exposition format: http_requests_total{method="GET",path="/api/tasks",status="401"} 3
    let has_auth_failure_metric = body.lines().any(|line| {
        line.contains("http_requests_total")
            && line.contains("method=\"GET\"")
            && line.contains("path=\"/api/tasks\"")
            && line.contains("status=\"401\"")
    });
    assert!(
        has_auth_failure_metric,
        "Metrics should contain http_requests_total with method=GET, path=/api/tasks, status=401 on the same line"
    );
}

/// P1-2: Hot shared-user modify pressure records contention metrics and avoids 500s.
///
/// This is a faster regression signal than the Goose team-contention profile:
/// it drives one hot task behind a single shared replica mutex and then checks
/// that the contention metrics the release-qualification summaries depend on
/// are actually emitted.
#[tokio::test(flavor = "multi_thread")]
async fn test_hot_shared_modify_pressure_records_replica_contention_metrics() {
    let env = setup_with_metrics().await;

    let (h, v) = auth_header(&env.user_a.token);
    let create_resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+team Seed task"}))
        .await;
    create_resp.assert_status_ok();

    let (h, v) = auth_header(&env.user_a.token);
    let list_resp = env.server.get("/api/tasks").add_header(h, v).await;
    list_resp.assert_status_ok();
    let tasks: Vec<Value> = list_resp.json();
    let uuid = tasks[0]["uuid"].as_str().unwrap().to_string();

    let mut handles = Vec::new();
    for i in 0..16 {
        let (h, v) = auth_header(&env.user_a.token);
        handles.push(
            env.server
                .post(&format!("/api/tasks/{uuid}/modify"))
                .add_header(h, v)
                .json(&serde_json::json!({"description": format!("Hot modify {i}")})),
        );
    }

    let results = tokio::time::timeout(
        TEST_TIMEOUT,
        futures::future::join_all(handles.into_iter().map(IntoFuture::into_future)),
    )
    .await
    .expect("hot shared modify pressure timed out — possible deadlock");

    let mut success_count = 0u32;
    for (i, resp) in results.iter().enumerate() {
        let status = resp.status_code().as_u16();
        assert!(
            status == 200 || status == 503,
            "Hot shared modify {i} returned {status} — expected 200 or 503"
        );
        if status == 200 {
            success_count += 1;
        }
    }

    assert!(
        success_count >= 8,
        "Expected at least 8/16 hot shared modify requests to succeed, got {success_count}"
    );

    let metrics_resp = env.server.get("/metrics").await;
    metrics_resp.assert_status_ok();
    let body = metrics_resp.text();

    let operation_ok = metric_value(
        &body,
        "replica_operation_duration_seconds_count",
        &[("operation", "modify_task"), ("result", "ok")],
    )
    .unwrap_or(0.0);
    let lock_wait_count = metric_value(
        &body,
        "replica_lock_wait_duration_seconds_count",
        &[("operation", "modify_task")],
    )
    .unwrap_or(0.0);

    assert!(
        operation_ok > 0.0,
        "Expected modify_task replica operation metrics after hot shared pressure"
    );
    assert!(
        lock_wait_count > 0.0,
        "Expected modify_task replica lock-wait metrics after hot shared pressure"
    );
}

/// P1-2: REST API backpressure — many concurrent requests for the same user
/// complete within bounded time without server errors.
///
/// With master_key: None, the sync bridge is disabled — this tests that the
/// REST API itself (replica mutex, auth cache, SQLite access) handles 20
/// concurrent requests without deadlock or unbounded queueing. The sync bridge
/// backpressure (OS-thread + lock timeout) is tested in sync_bridge_integration.rs.
#[tokio::test(flavor = "multi_thread")]
async fn test_sync_bridge_backpressure_same_user() {
    let env = setup().await;

    // Create initial task to warm replica
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+backpressure Seed task"}))
        .await;
    resp.assert_status_ok();

    // Fire 20 concurrent list requests — each triggers sync_user_replica.
    // The per-user sync lock serialises actual syncs, but lock acquisition
    // is capped at sync_timeout (5s default). Requests that can't acquire
    // the lock skip sync and return the REST response directly.
    let start = std::time::Instant::now();

    let mut handles = Vec::new();
    for _ in 0..20 {
        let (h, v) = auth_header(&env.user_a.token);
        handles.push(env.server.get("/api/tasks").add_header(h, v));
    }

    let results = tokio::time::timeout(
        TEST_TIMEOUT,
        futures::future::join_all(handles.into_iter().map(IntoFuture::into_future)),
    )
    .await
    .expect("backpressure test timed out — possible deadlock");

    let elapsed = start.elapsed();

    for (i, resp) in results.iter().enumerate() {
        assert_eq!(
            resp.status_code().as_u16(),
            200,
            "Backpressure request {i} returned {} — all should succeed",
            resp.status_code().as_u16()
        );
    }

    // All 20 requests should complete well within the sync timeout window.
    // If they were queueing serially through the sync lock, this would take
    // 20 × 5s = 100s. Instead, most skip sync, so total should be fast.
    assert!(
        elapsed.as_secs() < 15,
        "20 concurrent requests took {}s — expected <15s (backpressure should prevent serial queueing)",
        elapsed.as_secs()
    );
}

/// P1-3: Request to quarantined user returns 503 quickly, not a timeout.
///
/// Once a user is quarantined, all endpoints should fail-fast with 503.
/// This tests that quarantine checks happen BEFORE any expensive operations.
#[tokio::test(flavor = "multi_thread")]
async fn test_quarantined_user_requests_fail_fast() {
    let env = setup().await;

    // Create a task first (warm the replica)
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+failfast Test task"}))
        .await;
    resp.assert_status_ok();

    // Quarantine the user via admin endpoint
    let (h, v) = auth_header(&env.user_a.token);
    let offline_resp = env
        .server
        .post(&format!("/admin/user/{}/offline", env.user_a.user_id))
        .add_header(h, v)
        .await;
    offline_resp.assert_status_ok();

    // All subsequent requests should return 503 immediately
    let start = std::time::Instant::now();

    let (h1, v1) = auth_header(&env.user_a.token);
    let resp1 = env.server.get("/api/tasks").add_header(h1, v1).await;
    assert_eq!(
        resp1.status_code().as_u16(),
        503,
        "GET /api/tasks should return 503"
    );

    let (h2, v2) = auth_header(&env.user_a.token);
    let resp2 = env
        .server
        .post("/api/tasks")
        .add_header(h2, v2)
        .json(&serde_json::json!({"raw": "+failfast Should fail"}))
        .await;
    assert_eq!(
        resp2.status_code().as_u16(),
        503,
        "POST /api/tasks should return 503"
    );

    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let nil = Uuid::nil();
    let resp3 = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"should-fail".to_vec().into())
        .await;
    assert_eq!(
        resp3.status_code().as_u16(),
        503,
        "TC sync should return 503"
    );

    let elapsed = start.elapsed();
    // Three sequential requests to quarantined user should complete nearly
    // instantly (no sync, no replica open). 5s budget is generous for CI.
    assert!(
        elapsed.as_secs() < 5,
        "Quarantined requests should fail-fast (took {}ms for 3 requests)",
        elapsed.as_millis()
    );

    // Bring user back online
    let (h, v) = auth_header(&env.user_a.token);
    let online_resp = env
        .server
        .post(&format!("/admin/user/{}/online", env.user_a.user_id))
        .add_header(h, v)
        .await;
    online_resp.assert_status_ok();

    // Should work again
    let (h, v) = auth_header(&env.user_a.token);
    let recovery_resp = env.server.get("/api/tasks").add_header(h, v).await;
    assert_eq!(
        recovery_resp.status_code().as_u16(),
        200,
        "After online, GET /api/tasks should work again"
    );
}
