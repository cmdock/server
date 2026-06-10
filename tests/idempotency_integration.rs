//! Integration tests for `Idempotency-Key` support per
//! `task-write-contract.md` § Idempotency (cmdock/architecture commit
//! a3f242a). Implements server#114.
//!
//! Scenarios covered (mirrors the contract acceptance list 1:1, plus
//! review-driven regression locks):
//!
//! - Happy-path replay (same key, same body → byte-identical replay)
//! - Body conflict (same key, different body → 409 IDEMPOTENCY_KEY_CONFLICT)
//! - In-flight 503 simulation (pending row exists with matching fp)
//! - Failed-then-retry (validation rejection rolls back pending row)
//! - Cross-user isolation
//! - Cross-endpoint isolation (same key on add vs modify)
//! - Header validation (empty / >64 chars / non-ASCII / control chars)
//! - Lifecycle endpoints (`done`, `undo`, `delete`) tolerate the header
//! - Stale-finalizer race (Phase 3 from superseded attempt is silently discarded)
//! - Lookup-time expiry (independent of background reaper)
//! - Modify endpoint replay (byte-identical)
//! - Modify endpoint body conflict
//! - Concurrent storage-level retries (UNIQUE constraint serialises lookup-or-insert)
//! - Whitespace-different body produces conflict (no JSON normalisation)

mod common;

use std::sync::Arc;

use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::Router;
use axum_test::TestServer;
use serde_json::Value;
use tempfile::TempDir;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use cmdock_server::app_config;
use cmdock_server::app_state::AppState;
use cmdock_server::config::ServerConfig;
use cmdock_server::config_api;
use cmdock_server::health;
use cmdock_server::store::models::{IdempotencyLookupOutcome, NewUser};
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::summary;
use cmdock_server::sync;
use cmdock_server::tasks;
use cmdock_server::views;

const IDEMPOTENCY_HEADER: HeaderName = HeaderName::from_static("idempotency-key");

fn auth_header(token: &str) -> (HeaderName, HeaderValue) {
    (
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

struct TestEnv {
    server: TestServer,
    _tmp: TempDir,
    user_id: String,
    token: String,
    store: Arc<dyn ConfigStore>,
    maintenance: Arc<dyn cmdock_server::store::OperatorMaintenanceBackend>,
    config: ServerConfig,
}

async fn setup() -> TestEnv {
    setup_with_config(|_| {}).await
}

/// Setup with a config customisation hook — used to shrink retention /
/// pending-timeout / retry-after windows for fast tests.
async fn setup_with_config<F: FnOnce(&mut ServerConfig)>(customise: F) -> TestEnv {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite_store = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite_store.clone();
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "idempotency_user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();
    cmdock_server::admin::prefix::backfill_missing_user_prefixes(store.as_ref())
        .await
        .unwrap();
    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();
    std::fs::create_dir_all(data_dir.join("users").join(&user.id)).unwrap();

    let mut config = common::test_server_config(data_dir.clone());
    customise(&mut config);
    let config_for_state = config.clone();
    let state = AppState::new(store.clone(), sqlite_store.clone(), &config_for_state);

    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);

    TestEnv {
        server,
        _tmp: tmp,
        user_id: user.id,
        token,
        store,
        maintenance: sqlite_store,
        config,
    }
}

// ============================================================================
// Happy path
// ============================================================================

#[tokio::test]
async fn test_add_task_with_idempotency_key_replays_on_retry() {
    let env = setup().await;
    let key = "550e8400-e29b-41d4-a716-446655440000";

    // First request — creates a task.
    let (auth_h, auth_v) = auth_header(&env.token);
    let resp1 = env
        .server
        .post("/api/tasks")
        .add_header(auth_h.clone(), auth_v.clone())
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"raw": "+smoke First call"}))
        .await;
    resp1.assert_status_ok();
    let body1: Value = resp1.json();
    let uuid1 = body1["output"]
        .as_str()
        .unwrap()
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap();

    // Second request with same key + same body — should replay without
    // creating a second task.
    let resp2 = env
        .server
        .post("/api/tasks")
        .add_header(auth_h, auth_v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"raw": "+smoke First call"}))
        .await;
    resp2.assert_status_ok();
    let body2: Value = resp2.json();
    assert_eq!(
        body2["output"], body1["output"],
        "replay must return identical body"
    );
    assert_eq!(body2["success"], body1["success"]);

    // Verify only one task exists.
    let (h, v) = auth_header(&env.token);
    let list = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = list.json();
    assert_eq!(tasks.len(), 1, "replay must not create a second task");
    assert_eq!(tasks[0]["uuid"].as_str().unwrap(), uuid1);
}

#[tokio::test]
async fn test_add_task_without_idempotency_key_creates_two_tasks() {
    // Regression: at-least-once retry semantics preserved when header is
    // absent. Behaviour change in #114 is purely additive.
    let env = setup().await;
    let (h, v) = auth_header(&env.token);

    env.server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+smoke No key"}))
        .await
        .assert_status_ok();
    env.server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+smoke No key"}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let tasks: Vec<Value> = env.server.get("/api/tasks").add_header(h, v).await.json();
    assert_eq!(tasks.len(), 2, "no key → two tasks (at-least-once)");
}

#[tokio::test]
async fn test_add_task_replay_returns_byte_identical_body() {
    let env = setup().await;
    let key = "byte-identity-test-001";

    let (h, v) = auth_header(&env.token);
    let resp1 = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"raw": "+smoke A"}))
        .await;
    let bytes1 = resp1.as_bytes().to_vec();

    let resp2 = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"raw": "+smoke A"}))
        .await;
    let bytes2 = resp2.as_bytes().to_vec();

    assert_eq!(
        bytes1, bytes2,
        "replay must be byte-identical to original response body"
    );
}

// ============================================================================
// Body conflict — same key, different body → 409
// ============================================================================

#[tokio::test]
async fn test_add_task_body_conflict_returns_409() {
    let env = setup().await;
    let key = "conflict-test";

    let (h, v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"raw": "+smoke Body A"}))
        .await
        .assert_status_ok();

    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"raw": "+smoke Body B"}))
        .await;
    resp.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "IDEMPOTENCY_KEY_CONFLICT"
    );
}

// ============================================================================
// In-flight 503 — pending row exists with matching fingerprint
// ============================================================================

#[tokio::test]
async fn test_add_task_inflight_returns_503_with_retry_after() {
    // Pre-seed a pending row at the storage layer (simulates a request
    // mid-Phase-2 or stranded by process death) and verify the next
    // retry returns 503 IDEMPOTENCY_IN_FLIGHT with a Retry-After header.
    let env = setup_with_config(|c| {
        c.task_write.idempotency_retry_after_seconds = 7;
    })
    .await;
    let key = "in-flight-test";
    let body = serde_json::json!({"raw": "+smoke In flight"});
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let fingerprint = sha256(&body_bytes);

    // Pre-seed the pending row directly via the store. This simulates a
    // concurrent in-flight request without needing real concurrency.
    let now = cmdock_server::idempotency::now_unix_seconds();
    let outcome = env
        .store
        .lookup_or_insert_idempotency_pending(
            &env.user_id,
            "/api/tasks",
            key,
            &fingerprint,
            env.config.task_write.idempotency_pending_timeout_seconds,
            env.config.task_write.idempotency_retention_hours,
            now,
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        IdempotencyLookupOutcome::FreshExecution { .. }
    ));

    // Now retry from the client — should hit the pending row and 503.
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&body)
        .await;
    resp.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "IDEMPOTENCY_IN_FLIGHT"
    );
    let retry_after = resp.headers().get("retry-after");
    assert_eq!(
        retry_after.map(|v| v.to_str().unwrap()),
        Some("7"),
        "Retry-After must reflect configured value"
    );
}

#[tokio::test]
async fn test_add_task_inflight_with_mismatched_body_returns_409() {
    // Pending row exists but client sends a different body — conflict
    // wins regardless of state per § Replay behaviour table.
    let env = setup().await;
    let key = "in-flight-mismatch";

    let body = serde_json::json!({"raw": "+smoke Original"});
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let fingerprint = sha256(&body_bytes);

    let now = cmdock_server::idempotency::now_unix_seconds();
    env.store
        .lookup_or_insert_idempotency_pending(
            &env.user_id,
            "/api/tasks",
            key,
            &fingerprint,
            env.config.task_write.idempotency_pending_timeout_seconds,
            env.config.task_write.idempotency_retention_hours,
            now,
        )
        .await
        .unwrap();

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"raw": "+smoke DIFFERENT"}))
        .await;
    resp.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "IDEMPOTENCY_KEY_CONFLICT"
    );
}

// ============================================================================
// Failed-then-retry — validation rejection rolls back pending row
// ============================================================================

#[tokio::test]
async fn test_add_task_failed_validation_does_not_block_retry() {
    let env = setup().await;
    let key = "rollback-test";

    // First attempt has bad body (fails garde validation: empty raw).
    let (h, v) = auth_header(&env.token);
    let bad_resp = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"raw": ""}))
        .await;
    bad_resp.assert_status(StatusCode::BAD_REQUEST);

    // Second attempt with same key + valid body must succeed (fresh
    // execution — pending row was rolled back).
    let good_resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"raw": "+smoke Now valid"}))
        .await;
    good_resp.assert_status_ok();
}

// ============================================================================
// Lookup-time expiry — pending row past timeout treated as fresh
// ============================================================================

#[tokio::test]
async fn test_lookup_time_expiry_independent_of_reaper() {
    // Configure tiny pending-timeout. Pre-seed a pending row "in the
    // past" (created_at < now - timeout). Lookup must treat it as
    // expired and run fresh — without invoking the reaper.
    let env = setup_with_config(|c| {
        c.task_write.idempotency_pending_timeout_seconds = 2;
    })
    .await;
    let key = "expiry-test";
    let body = serde_json::json!({"raw": "+smoke Expiry"});
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let fingerprint = sha256(&body_bytes);

    // Pre-seed pending row 10 seconds ago (well past the 2s timeout).
    let now = cmdock_server::idempotency::now_unix_seconds();
    let stale_now = now - 10;
    env.store
        .lookup_or_insert_idempotency_pending(
            &env.user_id,
            "/api/tasks",
            key,
            &fingerprint,
            env.config.task_write.idempotency_pending_timeout_seconds,
            env.config.task_write.idempotency_retention_hours,
            stale_now,
        )
        .await
        .unwrap();

    // Retry — should hit lookup-time expiry, treat as fresh, succeed.
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&body)
        .await;
    resp.assert_status_ok();
}

// ============================================================================
// Stale-finalizer race — Phase 3 from superseded attempt is discarded
// ============================================================================

#[tokio::test]
async fn test_stale_finalizer_race_attempt_id_guard() {
    let env = setup().await;
    let key = "stale-finalizer-test";
    let fingerprint = [0u8; 32];
    let now = cmdock_server::idempotency::now_unix_seconds();

    // Phase 1A: attempt A inserts pending row, gets attempt_id_a.
    let outcome_a = env
        .store
        .lookup_or_insert_idempotency_pending(
            &env.user_id,
            "/api/tasks",
            key,
            &fingerprint,
            env.config.task_write.idempotency_pending_timeout_seconds,
            env.config.task_write.idempotency_retention_hours,
            now,
        )
        .await
        .unwrap();
    let attempt_id_a = match outcome_a {
        IdempotencyLookupOutcome::FreshExecution { attempt_id } => attempt_id,
        _ => panic!("expected fresh execution"),
    };

    // Simulate a long enough wait for attempt A to expire.
    // Pre-seed B by calling lookup with stale_now far enough back that A
    // is past the pending-timeout.
    let pending_timeout = env.config.task_write.idempotency_pending_timeout_seconds as i64;
    let lookup_now_for_b = now + pending_timeout + 1;
    let outcome_b = env
        .store
        .lookup_or_insert_idempotency_pending(
            &env.user_id,
            "/api/tasks",
            key,
            &fingerprint,
            env.config.task_write.idempotency_pending_timeout_seconds,
            env.config.task_write.idempotency_retention_hours,
            lookup_now_for_b,
        )
        .await
        .unwrap();
    let attempt_id_b = match outcome_b {
        IdempotencyLookupOutcome::FreshExecution { attempt_id } => attempt_id,
        _ => panic!("expected fresh execution after A expired"),
    };
    assert_ne!(attempt_id_a, attempt_id_b);

    // Now A's stale Phase 3 arrives — must affect zero rows.
    let stale_finalize = env
        .store
        .finalize_idempotency_completed(
            &env.user_id,
            "/api/tasks",
            key,
            &attempt_id_a,
            200,
            b"stale-payload",
            Some("application/json"),
        )
        .await
        .unwrap();
    assert!(
        !stale_finalize,
        "stale Phase 3 from superseded attempt must NOT update any row"
    );

    // B's Phase 3 arrives normally — must succeed.
    let fresh_finalize = env
        .store
        .finalize_idempotency_completed(
            &env.user_id,
            "/api/tasks",
            key,
            &attempt_id_b,
            200,
            b"fresh-payload",
            Some("application/json"),
        )
        .await
        .unwrap();
    assert!(fresh_finalize);
}

// ============================================================================
// Cross-user / cross-endpoint isolation
// ============================================================================

#[tokio::test]
async fn test_cross_user_idempotency_keys_do_not_collide() {
    let env = setup().await;
    let user2 = env
        .store
        .create_user(&NewUser {
            username: "idempotency_user_2".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();
    cmdock_server::admin::prefix::backfill_missing_user_prefixes(env.store.as_ref())
        .await
        .unwrap();
    let token2 = env
        .store
        .create_api_token(&user2.id, Some("test"))
        .await
        .unwrap();
    std::fs::create_dir_all(env._tmp.path().join("users").join(&user2.id)).unwrap();

    let key = "shared-key";
    let body = serde_json::json!({"raw": "+smoke Shared"});

    let (h1, v1) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(h1, v1)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&body)
        .await
        .assert_status_ok();

    let (h2, v2) = auth_header(&token2);
    env.server
        .post("/api/tasks")
        .add_header(h2, v2)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&body)
        .await
        .assert_status_ok();

    // Each user gets their own task — no cross-user replay or conflict.
    let (h, v) = auth_header(&env.token);
    let user1_tasks: Vec<Value> = env.server.get("/api/tasks").add_header(h, v).await.json();
    assert_eq!(user1_tasks.len(), 1);
}

#[tokio::test]
async fn test_cross_endpoint_idempotency_keys_are_independent() {
    let env = setup().await;
    let key = "cross-endpoint-key";

    let (h, v) = auth_header(&env.token);
    let create = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"raw": "+smoke For modify"}))
        .await;
    let body: Value = create.json();
    let uuid = body["output"]
        .as_str()
        .unwrap()
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap()
        .to_string();

    // Same key on /api/tasks/{uuid}/modify — different request_path,
    // independent dedup record. Must succeed (not 409, not 503).
    let modify = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"priority": "H"}))
        .await;
    modify.assert_status_ok();
}

// ============================================================================
// Header validation
// ============================================================================

#[tokio::test]
async fn test_idempotency_key_header_validation() {
    let env = setup().await;

    // Empty header value.
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_static(""))
        .json(&serde_json::json!({"raw": "+smoke Empty key"}))
        .await;
    resp.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_IDEMPOTENCY_KEY"
    );

    // > 64 chars.
    let too_long = "a".repeat(65);
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .add_header(
            IDEMPOTENCY_HEADER,
            HeaderValue::from_str(&too_long).unwrap(),
        )
        .json(&serde_json::json!({"raw": "+smoke Too long"}))
        .await;
    resp.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_IDEMPOTENCY_KEY"
    );

    // Exactly 64 chars — valid.
    let exactly_64 = "a".repeat(64);
    let (h, v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(h, v)
        .add_header(
            IDEMPOTENCY_HEADER,
            HeaderValue::from_str(&exactly_64).unwrap(),
        )
        .json(&serde_json::json!({"raw": "+smoke 64 chars OK"}))
        .await
        .assert_status_ok();

    // ASCII control character (\t).
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_static("abc\tdef"))
        .json(&serde_json::json!({"raw": "+smoke Tab"}))
        .await;
    resp.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_IDEMPOTENCY_KEY"
    );
}

// ============================================================================
// Lifecycle endpoints tolerate the header forward-compat
// ============================================================================

#[tokio::test]
async fn test_lifecycle_endpoints_tolerate_idempotency_key_header() {
    // Per contract § Endpoints accepting Idempotency-Key, lifecycle
    // endpoints (done, undo, delete, restore) SHOULD ignore the header
    // forward-compat — neither error on its presence nor engage dedup
    // machinery. Today they don't read the header at all; this test
    // locks the regression.
    let env = setup().await;
    let (h, v) = auth_header(&env.token);
    let create = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+smoke Lifecycle"}))
        .await;
    let body: Value = create.json();
    let uuid = body["output"]
        .as_str()
        .unwrap()
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap()
        .to_string();

    // /done — header tolerated, returns 200.
    env.server
        .post(&format!("/api/tasks/{uuid}/done"))
        .add_header(h.clone(), v.clone())
        .add_header(
            IDEMPOTENCY_HEADER,
            HeaderValue::from_static("ignored-on-done"),
        )
        .await
        .assert_status_ok();

    // /undo — header tolerated, returns 200.
    env.server
        .post(&format!("/api/tasks/{uuid}/undo"))
        .add_header(h.clone(), v.clone())
        .add_header(
            IDEMPOTENCY_HEADER,
            HeaderValue::from_static("ignored-on-undo"),
        )
        .await
        .assert_status_ok();

    // /delete — header tolerated, returns 200.
    env.server
        .post(&format!("/api/tasks/{uuid}/delete"))
        .add_header(h, v)
        .add_header(
            IDEMPOTENCY_HEADER,
            HeaderValue::from_static("ignored-on-delete"),
        )
        .await
        .assert_status_ok();
}

// ============================================================================
// Modify endpoint replay
// ============================================================================

#[tokio::test]
async fn test_modify_task_with_idempotency_key_replays_on_retry() {
    let env = setup().await;
    let (h, v) = auth_header(&env.token);
    let create = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+smoke For modify replay"}))
        .await;
    let body: Value = create.json();
    let uuid = body["output"]
        .as_str()
        .unwrap()
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap()
        .to_string();

    let key = "modify-replay-key";

    let resp1 = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h.clone(), v.clone())
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"priority": "H"}))
        .await;
    resp1.assert_status_ok();
    let bytes1 = resp1.as_bytes().to_vec();

    let resp2 = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"priority": "H"}))
        .await;
    resp2.assert_status_ok();
    let bytes2 = resp2.as_bytes().to_vec();

    assert_eq!(
        bytes1, bytes2,
        "modify replay must be byte-identical to original response"
    );
}

// ============================================================================
// Helper
// ============================================================================

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

// ============================================================================
// Concurrent retries — UNIQUE constraint serialises lookup-or-insert
// ============================================================================

#[tokio::test]
async fn test_concurrent_storage_lookup_or_insert_serialises() {
    // Storage-level concurrency: two tasks call lookup_or_insert in parallel
    // with the same tuple key. Per § Server behaviour Phase 1, BEGIN
    // IMMEDIATE serialises them; one must observe the other's pending row.
    let env = setup().await;
    let key = "concurrent-retries";
    let fingerprint = [42u8; 32];
    let now = cmdock_server::idempotency::now_unix_seconds();
    let cfg = env.config.task_write.clone();
    let store = env.store.clone();
    let user_id = env.user_id.clone();

    let s1 = store.clone();
    let u1 = user_id.clone();
    let h1 = tokio::spawn(async move {
        s1.lookup_or_insert_idempotency_pending(
            &u1,
            "/api/tasks",
            key,
            &fingerprint,
            cfg.idempotency_pending_timeout_seconds,
            cfg.idempotency_retention_hours,
            now,
        )
        .await
        .unwrap()
    });
    let s2 = store.clone();
    let u2 = user_id.clone();
    let cfg2 = env.config.task_write.clone();
    let h2 = tokio::spawn(async move {
        s2.lookup_or_insert_idempotency_pending(
            &u2,
            "/api/tasks",
            key,
            &fingerprint,
            cfg2.idempotency_pending_timeout_seconds,
            cfg2.idempotency_retention_hours,
            now,
        )
        .await
        .unwrap()
    });

    let r1 = h1.await.unwrap();
    let r2 = h2.await.unwrap();

    // Exactly one of the two outcomes must be FreshExecution; the other
    // sees the now-committed pending row and returns InFlight.
    let (fresh, in_flight) = match (&r1, &r2) {
        (IdempotencyLookupOutcome::FreshExecution { .. }, IdempotencyLookupOutcome::InFlight) => {
            (1, 1)
        }
        (IdempotencyLookupOutcome::InFlight, IdempotencyLookupOutcome::FreshExecution { .. }) => {
            (1, 1)
        }
        _ => panic!("expected exactly one FreshExecution + one InFlight; got {r1:?} / {r2:?}"),
    };
    assert_eq!(fresh + in_flight, 2);
}

// ============================================================================
// Modify endpoint — body conflict path (mirrors add_task)
// ============================================================================

#[tokio::test]
async fn test_modify_task_body_conflict_returns_409() {
    let env = setup().await;
    let (h, v) = auth_header(&env.token);
    let create = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+smoke For modify conflict"}))
        .await;
    let body: Value = create.json();
    let uuid = body["output"]
        .as_str()
        .unwrap()
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap()
        .to_string();

    let key = "modify-conflict-key";
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h.clone(), v.clone())
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"priority": "H"}))
        .await
        .assert_status_ok();

    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .json(&serde_json::json!({"priority": "L"}))
        .await;
    resp.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "IDEMPOTENCY_KEY_CONFLICT"
    );
}

// ============================================================================
// Ambiguous Phase 2 outcome leaves the pending row in place
// ============================================================================
//
// Drives `run_idempotent` directly with a closure that returns
// `Phase2Outcome::Ambiguous`. End-to-end pre-flight verification that
// an ambiguous Phase 2 result does NOT roll back the dedup row, so a
// subsequent retry within the pending-timeout returns 503
// IDEMPOTENCY_IN_FLIGHT (matching the in-flight semantics for an
// in-progress original).

#[tokio::test]
async fn test_ambiguous_phase2_leaves_pending_row_subsequent_retry_503() {
    use cmdock_server::idempotency::{run_idempotent, Phase2Outcome};

    let env = setup().await;
    let key = "ambiguous-test";
    let body = serde_json::to_vec(&serde_json::json!({"raw": "+smoke amb"})).unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", env.token)).unwrap(),
    );
    headers.insert(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap());

    let state = AppState::new(env.store.clone(), env.maintenance.clone(), &env.config);

    // First call: closure returns Ambiguous → pending row left in place.
    let resp1 = run_idempotent(
        &state,
        &env.user_id,
        "/api/tasks",
        &headers,
        key,
        &body,
        |_| async {
            Phase2Outcome::Ambiguous {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                response_body: b"simulated ambiguous outcome".to_vec(),
                content_type: None,
            }
        },
    )
    .await;
    assert_eq!(resp1.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // Second call within pending-timeout, same fingerprint → 503.
    let resp2 = run_idempotent(
        &state,
        &env.user_id,
        "/api/tasks",
        &headers,
        key,
        &body,
        |_| async {
            // Should NOT be invoked — lookup short-circuits to in-flight.
            panic!("phase2 must not run on in-flight retry")
        },
    )
    .await;
    assert_eq!(resp2.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body_bytes = axum::body::to_bytes(resp2.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        std::str::from_utf8(&body_bytes).unwrap(),
        "IDEMPOTENCY_IN_FLIGHT"
    );
}

// ============================================================================
// Whitespace-different body produces conflict (no JSON normalisation)
// ============================================================================

#[tokio::test]
async fn test_whitespace_different_body_triggers_conflict() {
    // Per § Body-conflict comparison: server does NOT normalise. Two
    // semantically-identical JSON payloads with different whitespace
    // hash to different fingerprints → 409 IDEMPOTENCY_KEY_CONFLICT.
    // Clients that want flexibility MUST canonicalise before submission.
    let env = setup().await;
    let key = "whitespace-test";
    let (h, v) = auth_header(&env.token);

    // Send raw bytes via .text() to avoid axum-test re-serialising.
    let resp1 = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .add_header(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        .text(r#"{"raw":"+smoke compact"}"#)
        .await;
    resp1.assert_status_ok();

    let resp2 = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(key).unwrap())
        .add_header(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        .text(r#"{ "raw" : "+smoke compact" }"#)
        .await;
    resp2.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        std::str::from_utf8(resp2.as_bytes()).unwrap(),
        "IDEMPOTENCY_KEY_CONFLICT"
    );
}
