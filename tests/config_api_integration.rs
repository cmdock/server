//! Integration tests for the generic config API (backwards compat).
//!
//! Tests GET /api/config/{type}, POST /api/config/{type},
//! DELETE /api/config/{type}/{id}, and auth requirements.

mod common;

use std::sync::Arc;

use axum::http::{header, HeaderValue, Method};
use axum::Router;
use axum_test::TestServer;
use serde_json::Value;
use tempfile::TempDir;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use cmdock_server::app_config;
use cmdock_server::app_state::AppState;
use cmdock_server::config_api;
use cmdock_server::geofences;
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
            username: "config_api_user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();

    let config = common::test_server_config(data_dir.clone());

    let state = AppState::new(store, &config);

    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(geofences::routes())
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
async fn test_config_empty() {
    let env = setup().await;
    let (h, v) = auth_header(&env.token);

    let resp = env
        .server
        .get("/api/config/custom_type")
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    assert!(body["items"].as_array().unwrap().is_empty());
    assert_eq!(body["legacy"], false);
}

#[tokio::test]
async fn test_config_upsert_roundtrip() {
    let env = setup().await;

    // Upsert config
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/config/widgets")
        .add_header(h, v)
        .json(&serde_json::json!({
            "version": "v1",
            "items": [
                {"id": "w-1", "label": "Widget A", "kind": "custom"}
            ]
        }))
        .await;
    resp.assert_status_ok();

    // Get it back
    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/config/widgets").add_header(h, v).await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], "w-1");
    assert_eq!(items[0]["label"], "Widget A");
}

#[tokio::test]
async fn test_config_delete() {
    let env = setup().await;

    // Create config with an item
    let (h, v) = auth_header(&env.token);
    env.server
        .post("/api/config/widgets")
        .add_header(h, v)
        .json(&serde_json::json!({
            "version": "v1",
            "items": [
                {"id": "w-1", "label": "Widget A"},
                {"id": "w-2", "label": "Widget B"}
            ]
        }))
        .await
        .assert_status_ok();

    // Delete item w-1
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .delete("/api/config/widgets/w-1")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::NO_CONTENT);

    // Verify remaining items
    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/config/widgets").add_header(h, v).await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], "w-2");
}

#[tokio::test]
async fn test_config_requires_auth() {
    let env = setup().await;

    // GET without auth
    let resp = env.server.get("/api/config/widgets").await;
    resp.assert_status_unauthorized();

    // POST without auth
    let resp = env
        .server
        .post("/api/config/widgets")
        .json(&serde_json::json!({"version": "v1", "items": []}))
        .await;
    resp.assert_status_unauthorized();

    // DELETE without auth
    let resp = env.server.delete("/api/config/widgets/w-1").await;
    resp.assert_status_unauthorized();
}

#[tokio::test]
async fn test_config_validation_rejects_bad_path_and_version() {
    let env = setup().await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post("/api/config/widgets")
        .add_header(h, v)
        .json(&serde_json::json!({
            "version": "   ",
            "items": []
        }))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);

    let (h, v) = auth_header(&env.token);
    env.server
        .get("/api/config/%20bad%20")
        .add_header(h, v)
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);

    let (h, v) = auth_header(&env.token);
    env.server
        .delete("/api/config/widgets/%20bad%20")
        .add_header(h, v)
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);
}
