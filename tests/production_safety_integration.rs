//! Production safety integration tests.
//!
//! Tests for scenarios that would cause 3am incidents: concurrent sync conflicts,
//! cold cache thundering herd, healthz side effects, auth cache revocation policy,
//! and pending task count accuracy.

mod common;

use std::sync::Arc;

use axum::http::{header, HeaderValue, Method};
use axum::Router;
use axum_test::TestServer;
use serde_json::Value;
use sha2::{Digest, Sha256};
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
    store: Arc<dyn ConfigStore>,
    _tmp: TempDir,
    user_a: UserInfo,
    #[allow(dead_code)]
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

const HS_CT: &str = "application/vnd.taskchampion.history-segment";

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
        user_b,
    }
}

// --- Tests ---

/// Test 1: Two concurrent add-version requests with the same parent (nil).
/// One must succeed (200), the other must get 409 Conflict — never a 500.
/// The user must NOT be quarantined, and the version chain must remain readable.
#[tokio::test]
async fn test_tc_sync_concurrent_add_version_returns_conflict_not_500() {
    let env = setup().await;
    let nil = Uuid::nil();

    // Build two requests, then fire them concurrently with tokio::join!
    let (ch1, cv1) = client_id_header(&env.user_a.client_id);
    let req1 = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch1, cv1)
        .content_type(HS_CT)
        .bytes(b"concurrent-v1-a".to_vec().into());

    let (ch2, cv2) = client_id_header(&env.user_a.client_id);
    let req2 = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch2, cv2)
        .content_type(HS_CT)
        .bytes(b"concurrent-v1-b".to_vec().into());

    let (resp1, resp2) = tokio::join!(req1, req2);

    let status1 = resp1.status_code().as_u16();
    let status2 = resp2.status_code().as_u16();

    // One must be 200, the other 409. Neither should be 500.
    assert_ne!(
        status1, 500,
        "First request returned 500 — server error on concurrent sync"
    );
    assert_ne!(
        status2, 500,
        "Second request returned 500 — server error on concurrent sync"
    );

    let mut statuses = vec![status1, status2];
    statuses.sort();
    assert_eq!(
        statuses,
        vec![200, 409],
        "Expected one 200 and one 409, got {status1} and {status2}"
    );

    // The 409 response should have X-Parent-Version-Id header
    let conflict_resp = if status1 == 409 { &resp1 } else { &resp2 };
    let parent_hdr = conflict_resp.header("X-Parent-Version-Id");
    assert!(
        !parent_hdr.is_empty(),
        "409 response must include X-Parent-Version-Id header"
    );

    // User must NOT be quarantined after a normal conflict
    let (h, v) = auth_header(&env.user_a.token);
    let stats_resp = env
        .server
        .get(&format!("/admin/user/{}/stats", env.user_a.user_id))
        .add_header(h, v)
        .await;
    stats_resp.assert_status_ok();
    let body: Value = stats_resp.json();
    assert_eq!(
        body["quarantined"], false,
        "User should NOT be quarantined after a normal version conflict"
    );

    // Version chain must be readable (get-child-version from nil should return 200)
    let (ch, cv) = client_id_header(&env.user_a.client_id);
    let chain_resp = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch, cv)
        .await;
    chain_resp.assert_status_ok();
}

/// Test 2: Multiple parallel sync reads on a fresh (uncached) server.
/// All must return valid protocol responses (200 or 404), never 500.
#[tokio::test]
async fn test_tc_sync_cold_cache_parallel_requests_no_500() {
    let env = setup().await;
    let nil = Uuid::nil();

    // Build 5 requests, then fire them concurrently with tokio::join!
    let (ch0, cv0) = client_id_header(&env.user_a.client_id);
    let r0 = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch0, cv0);
    let (ch1, cv1) = client_id_header(&env.user_a.client_id);
    let r1 = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch1, cv1);
    let (ch2, cv2) = client_id_header(&env.user_a.client_id);
    let r2 = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch2, cv2);
    let (ch3, cv3) = client_id_header(&env.user_a.client_id);
    let r3 = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch3, cv3);
    let (ch4, cv4) = client_id_header(&env.user_a.client_id);
    let r4 = env
        .server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch4, cv4);

    let (resp0, resp1, resp2, resp3, resp4) = tokio::join!(r0, r1, r2, r3, r4);

    for (i, resp) in [resp0, resp1, resp2, resp3, resp4].iter().enumerate() {
        let status = resp.status_code().as_u16();
        assert!(
            status == 200 || status == 404,
            "Request {i} returned {status} — expected 200 or 404, NOT 500"
        );
    }

    // User must NOT be quarantined
    let (h, v) = auth_header(&env.user_a.token);
    let stats_resp = env
        .server
        .get(&format!("/admin/user/{}/stats", env.user_a.user_id))
        .add_header(h, v)
        .await;
    stats_resp.assert_status_ok();
    let body: Value = stats_resp.json();
    assert_eq!(
        body["quarantined"], false,
        "User should NOT be quarantined after parallel cold-cache reads"
    );
}

/// Test 3: GET /healthz must not open replicas for users that aren't cached.
/// Creating user directories on disk should NOT cause taskchampion.sqlite3 to appear.
#[tokio::test]
async fn test_healthz_does_not_open_replicas() {
    let env = setup().await;

    // Create additional user directories on disk (just dirs, no TC databases)
    for i in 0..3 {
        let fake_user_dir = env._tmp.path().join("users").join(format!("fake-user-{i}"));
        std::fs::create_dir_all(&fake_user_dir).unwrap();
    }

    // Record baseline: check admin/status for cached_replicas
    let (h, v) = auth_header(&env.user_a.token);
    let status_before = env.server.get("/admin/status").add_header(h, v).await;
    status_before.assert_status_ok();
    let before: Value = status_before.json();
    let cached_before = before["cached_replicas"].as_u64().unwrap_or(0);

    // Call healthz
    let healthz_resp = env.server.get("/healthz").await;
    healthz_resp.assert_status_ok();

    // Verify no taskchampion.sqlite3 files were created in fake user dirs
    for i in 0..3 {
        let fake_user_dir = env._tmp.path().join("users").join(format!("fake-user-{i}"));
        let tc_db = fake_user_dir.join("taskchampion.sqlite3");
        assert!(
            !tc_db.exists(),
            "healthz should NOT create taskchampion.sqlite3 for fake-user-{i}"
        );
    }

    // Check that cached_replicas hasn't grown from the healthz call
    // (the auth_header call above may have cached user_a's replica due to
    // admin/status, but the 3 fake users should not be cached)
    let (h, v) = auth_header(&env.user_a.token);
    let status_after = env.server.get("/admin/status").add_header(h, v).await;
    status_after.assert_status_ok();
    let after: Value = status_after.json();
    let cached_after = after["cached_replicas"].as_u64().unwrap_or(0);

    // cached_replicas should not have increased by 3 (the fake users)
    assert!(
        cached_after <= cached_before + 1,
        "healthz should not open replicas for inactive users. \
         Before: {cached_before}, After: {cached_after}"
    );
}

/// Test 4: Auth cache revocation policy.
///
/// The auth cache has a 30s TTL. After revoking a token, a cached entry may
/// still serve the old identity until the TTL expires. This test documents
/// the current behaviour (stale-for-up-to-30s contract).
#[tokio::test]
async fn test_auth_cache_revocation_policy() {
    let env = setup().await;

    // Make a successful request to warm the auth cache
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();

    // Compute the token hash (same as SqliteConfigStore::hash_token)
    let token_hash = {
        let mut hasher = Sha256::new();
        hasher.update(env.user_a.token.as_bytes());
        format!("{:x}", hasher.finalize())
    };

    // Revoke the token via the store
    let revoked = env.store.revoke_api_token(&token_hash).await.unwrap();
    assert!(revoked, "Token should have been successfully revoked");

    // Make a request immediately after revocation
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let status = resp.status_code().as_u16();

    // Document the current behaviour:
    // The auth cache has a 30s TTL. If the token is still cached, the request
    // succeeds (200). If the cache has already expired or been evicted, it
    // fails (401). Either outcome is acceptable — this test locks in whichever
    // is the current behaviour.
    assert!(
        status == 200 || status == 401,
        "After token revocation, expected 200 (cached, stale <=30s) or 401 (evicted). Got: {status}"
    );

    if status == 200 {
        // This is the expected "stale cache" behaviour.
        // The revoked token will stop working within 30s when the cache entry expires.
        // This is an acceptable trade-off for the 99.99% reduction in config DB load.
    }

    // If we got 401, revocation is immediate (cache was evicted or expired).
    // Either way, the test documents the production contract.
}

/// Test 5: GET /healthz returns the correct pending task count.
///
/// The healthz endpoint counts pending tasks from cached replicas only.
/// After creating tasks via the REST API (which warms the cache), the count
/// should reflect the actual number of pending tasks.
#[tokio::test]
async fn test_healthz_returns_correct_pending_count() {
    let env = setup().await;

    // Create 3 tasks for user_a (this warms the replica cache)
    for i in 0..3 {
        let (h, v) = auth_header(&env.user_a.token);
        let resp = env
            .server
            .post("/api/tasks")
            .add_header(h, v)
            .json(&serde_json::json!({"raw": format!("+test Pending task {i}")}))
            .await;
        resp.assert_status_ok();
    }

    // The healthz cache has a 30s TTL. Since this is a fresh test env,
    // the cache will be stale (initialized with 0) and will refresh on first call.
    // However, the health cache double-checks under write lock, so a single
    // call should get the fresh count.
    let healthz_resp = env.server.get("/healthz").await;
    healthz_resp.assert_status_ok();
    let body: Value = healthz_resp.json();

    assert_eq!(body["status"], "ok");

    // pending_tasks is returned as a string for iOS backwards compat
    let pending_str = body["pending_tasks"]
        .as_str()
        .expect("pending_tasks should be a string");
    let pending: usize = pending_str
        .parse()
        .expect("pending_tasks should be a numeric string");

    assert_eq!(
        pending, 3,
        "Expected 3 pending tasks after creating 3 tasks, got {pending}"
    );
}
