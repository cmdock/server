//! Integration tests for request body size limits.
//!
//! Verifies that the 1 MiB default limit applies to REST API routes and the
//! 10 MiB limit applies to TaskChampion sync routes. Also checks that
//! normal-sized requests are unaffected.

mod common;

use std::sync::Arc;

use axum::http::{header, HeaderValue, Method};
use axum::Router;
use axum_test::TestServer;
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

// --- Helpers ---

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

struct TestEnv {
    server: TestServer,
    _tmp: TempDir,
    token: String,
    client_id: String,
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

    let user = store
        .create_user(&NewUser {
            username: "body_limit_user".to_string(),
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

    let config = common::test_server_config(data_dir.clone());

    let state = AppState::new(store, &config);

    // Mirror the body-limit layering from main.rs:
    // REST routes get 1 MiB limit, tc_sync routes get their own 10 MiB limit
    // (applied inside tc_sync::routes()). Built as separate routers then merged
    // so the 1 MiB layer does NOT shadow the sync routes.
    let rest_routes = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(admin::routes())
        .with_state(state.clone())
        .layer(RequestBodyLimitLayer::new(1024 * 1024)); // 1 MiB for REST

    let sync_routes = tc_sync::routes().with_state(state);

    let app = Router::new()
        .merge(rest_routes)
        .merge(sync_routes)
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
        token,
        client_id,
    }
}

// --- Tests ---

#[tokio::test]
async fn test_rest_api_rejects_body_over_1mib() {
    let env = setup().await;

    // 1 MiB + 1 byte — should exceed the limit
    let oversized_body = vec![b'x'; 1024 * 1024 + 1];

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .content_type("application/json")
        .bytes(oversized_body.into())
        .await;

    resp.assert_status(axum::http::StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_sync_add_version_rejects_body_over_10mib() {
    let env = setup().await;

    let nil = Uuid::nil();
    // 10 MiB + 1 byte
    let oversized_body = vec![b'x'; 10 * 1024 * 1024 + 1];

    let (ch, cv) = client_id_header(&env.client_id);
    let resp = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type("application/vnd.taskchampion.history-segment")
        .bytes(oversized_body.into())
        .await;

    resp.assert_status(axum::http::StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_sync_add_snapshot_rejects_body_over_10mib() {
    let env = setup().await;

    let version = Uuid::new_v4();
    // 10 MiB + 1 byte
    let oversized_body = vec![b'x'; 10 * 1024 * 1024 + 1];

    let (ch, cv) = client_id_header(&env.client_id);
    let resp = env
        .server
        .post(&format!("/v1/client/add-snapshot/{version}"))
        .add_header(ch, cv)
        .content_type("application/vnd.taskchampion.snapshot")
        .bytes(oversized_body.into())
        .await;

    resp.assert_status(axum::http::StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_sync_body_between_1mib_and_10mib() {
    // Verifies that the 1 MiB REST body limit does NOT apply to tc_sync routes.
    // Sync routes have their own 10 MiB limit (applied inside tc_sync::routes()).
    // A 5 MiB payload should pass the body limit layer and reach the handler.
    let env = setup().await;
    let nil = Uuid::nil();

    // 5 MiB — under sync's 10 MiB limit but over the REST 1 MiB limit
    let mid_body = vec![b'x'; 5 * 1024 * 1024];

    let (ch, cv) = client_id_header(&env.client_id);
    let resp = env
        .server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type("application/vnd.taskchampion.history-segment")
        .bytes(mid_body.into())
        .await;

    // The request reaches the handler (not rejected by body limit).
    // The handler may return various status codes depending on storage state,
    // but it must NOT be 413 Payload Too Large.
    assert_ne!(
        resp.status_code(),
        axum::http::StatusCode::PAYLOAD_TOO_LARGE,
        "5 MiB sync payload should not be rejected — tc_sync routes have a 10 MiB limit"
    );
}

#[tokio::test]
async fn test_normal_sized_request_works() {
    let env = setup().await;

    // A normal task creation request (well under 1 MiB)
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+test Normal sized task"}))
        .await;

    resp.assert_status_ok();
}
