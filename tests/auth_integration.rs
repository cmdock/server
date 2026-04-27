//! Integration tests for authentication behaviour.
//!
//! Tests missing auth header, wrong auth scheme, invalid token,
//! and valid token access.

mod common;

use std::sync::Arc;

use axum::http::{header, HeaderValue, Method};
use axum::Router;
use axum_test::TestServer;
use tempfile::TempDir;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

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
use cmdock_server::views;

// --- Setup ---

fn auth_header(token: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

struct TestEnv {
    server: TestServer,
    _tmp: TempDir,
    token: String,
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
            username: "auth_user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();

    std::fs::create_dir_all(tmp.path().join("users").join(&user.id)).unwrap();

    let config = common::test_server_config(data_dir.clone());

    let state = AppState::new(store, &config);

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
        token,
    }
}

// --- Tests ---

#[tokio::test]
async fn test_missing_auth_header() {
    let env = setup().await;

    let resp = env.server.get("/api/tasks").await;
    resp.assert_status_unauthorized();
    let body = resp.text();
    assert!(
        body.contains("Missing") || body.contains("missing") || body.contains("Authorization"),
        "Expected error about missing auth, got: {body}"
    );
}

#[tokio::test]
async fn test_wrong_auth_scheme() {
    let env = setup().await;

    let resp = env
        .server
        .get("/api/tasks")
        .add_header(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        )
        .await;
    resp.assert_status_unauthorized();
    let body = resp.text();
    assert!(
        body.contains("Missing")
            || body.contains("Bearer")
            || body.contains("Invalid")
            || body.contains("invalid"),
        "Expected error about auth scheme, got: {body}"
    );
}

#[tokio::test]
async fn test_invalid_token() {
    let env = setup().await;

    let (h, v) = auth_header("completely-bogus-token-value");
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_unauthorized();
    let body = resp.text();
    assert!(
        body.contains("Invalid")
            || body.contains("invalid")
            || body.contains("Unauthorized")
            || body.contains("unauthorized"),
        "Expected error about invalid token, got: {body}"
    );
}

#[tokio::test]
async fn test_valid_token() {
    let env = setup().await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();
}
