//! Integration tests for the summary endpoint.
//!
//! Spins up a real server with a temp database, creates a user with tasks,
//! and exercises the GET /api/summary endpoint.

mod common;

use std::sync::Arc;

use axum::http::{header, HeaderValue, Method};
use axum::Router;
use axum_test::TestServer;
use serde_json::Value;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use cmdock_server::admin;
use cmdock_server::app_config;
use cmdock_server::app_state::AppState;
use cmdock_server::config_api;
use cmdock_server::health;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::summary;
use cmdock_server::sync;
use cmdock_server::tasks;
use cmdock_server::views;

fn auth_header(token: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

async fn setup_test_server() -> (TestServer, String) {
    let tmp = tempfile::tempdir().unwrap();
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
        .create_user(&cmdock_server::store::models::NewUser {
            username: "testuser".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();

    let config = common::test_server_config_with_admin_token(data_dir.clone(), token.clone());

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
    std::mem::forget(tmp);

    (server, token)
}

#[tokio::test]
async fn test_summary_no_tasks_returns_all_clear() {
    let (server, token) = setup_test_server().await;
    let (hdr_name, hdr_val) = auth_header(&token);

    let response = server
        .get("/api/summary")
        .add_query_param("type", "today")
        .add_header(hdr_name, hdr_val)
        .await;

    response.assert_status_ok();
    let body: Value = response.json();

    assert_eq!(body["type"], "today");
    assert_eq!(body["task_count"], 0);
    assert!(body["summary"].as_str().unwrap().contains("all clear"));
    assert!(body["generated_at"].as_str().is_some());
}

#[tokio::test]
async fn test_summary_with_tasks_uses_template_fallback() {
    let (server, token) = setup_test_server().await;
    let (hdr_name, hdr_val) = auth_header(&token);

    // Add a task
    let (add_name, add_val) = auth_header(&token);
    server
        .post("/api/tasks")
        .add_header(add_name, add_val)
        .json(&serde_json::json!({"raw": "priority:H Test summary task"}))
        .await
        .assert_status_ok();

    // Get summary — should use template fallback (no LLM configured)
    let response = server
        .get("/api/summary")
        .add_header(hdr_name, hdr_val)
        .await;

    response.assert_status_ok();
    let body: Value = response.json();

    assert!(body["task_count"].as_u64().is_some());
    assert!(body["summary"].as_str().is_some());
}

#[tokio::test]
async fn test_summary_default_type_is_today() {
    let (server, token) = setup_test_server().await;
    let (hdr_name, hdr_val) = auth_header(&token);

    let response = server
        .get("/api/summary")
        .add_header(hdr_name, hdr_val)
        .await;

    response.assert_status_ok();
    let body: Value = response.json();
    assert_eq!(body["type"], "today");
}

#[tokio::test]
async fn test_summary_overdue_type() {
    let (server, token) = setup_test_server().await;
    let (hdr_name, hdr_val) = auth_header(&token);

    let response = server
        .get("/api/summary")
        .add_query_param("type", "overdue")
        .add_header(hdr_name, hdr_val)
        .await;

    response.assert_status_ok();
    let body: Value = response.json();
    assert_eq!(body["type"], "overdue");
    assert_eq!(body["task_count"], 0);
}

#[tokio::test]
async fn test_summary_requires_auth() {
    let (server, _token) = setup_test_server().await;

    let response = server.get("/api/summary").await;
    response.assert_status_unauthorized();
}

#[tokio::test]
async fn test_summary_response_shape() {
    let (server, token) = setup_test_server().await;
    let (hdr_name, hdr_val) = auth_header(&token);

    let response = server
        .get("/api/summary")
        .add_query_param("type", "week")
        .add_header(hdr_name, hdr_val)
        .await;

    response.assert_status_ok();
    let body: Value = response.json();

    assert!(body["type"].is_string());
    assert!(body["generated_at"].is_string());
    assert!(body["task_count"].is_number());
    assert!(body["summary"].is_string());

    // Verify generated_at is a valid ISO 8601 timestamp
    let ts = body["generated_at"].as_str().unwrap();
    assert!(
        chrono::DateTime::parse_from_rfc3339(ts).is_ok(),
        "generated_at should be valid RFC3339: {ts}"
    );
}

#[tokio::test]
async fn test_healthz_no_auth_required() {
    let (server, _token) = setup_test_server().await;

    let response = server.get("/healthz").await;
    response.assert_status_ok();

    let body: Value = response.json();
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn test_tasks_crud_lifecycle() {
    let (server, token) = setup_test_server().await;

    // Create
    let (h1, v1) = auth_header(&token);
    let create_resp = server
        .post("/api/tasks")
        .add_header(h1, v1)
        .json(&serde_json::json!({"raw": "project:TEST +integration Test task"}))
        .await;
    create_resp.assert_status_ok();
    let create_body: Value = create_resp.json();
    assert!(create_body["success"].as_bool().unwrap());

    // Extract UUID
    let output = create_body["output"].as_str().unwrap();
    let uuid = output
        .strip_prefix("Created task ")
        .and_then(|s| s.strip_suffix('.'))
        .unwrap();

    // List
    let (h2, v2) = auth_header(&token);
    let list_resp = server.get("/api/tasks").add_header(h2, v2).await;
    list_resp.assert_status_ok();
    let tasks: Vec<Value> = list_resp.json();
    assert!(tasks.iter().any(|t| t["uuid"] == uuid));

    // Complete
    let (h3, v3) = auth_header(&token);
    let done_resp = server
        .post(&format!("/api/tasks/{uuid}/done"))
        .add_header(h3, v3)
        .json(&serde_json::json!({}))
        .await;
    done_resp.assert_status_ok();
    assert!(done_resp.json::<Value>()["success"].as_bool().unwrap());

    // Delete
    let (h4, v4) = auth_header(&token);
    let delete_resp = server
        .post(&format!("/api/tasks/{uuid}/delete"))
        .add_header(h4, v4)
        .json(&serde_json::json!({}))
        .await;
    delete_resp.assert_status_ok();
    assert!(delete_resp.json::<Value>()["success"].as_bool().unwrap());
}

/// Setup a test server that includes admin routes (needed for quarantine tests).
/// Returns (server, token, user_id).
async fn setup_test_server_with_admin() -> (TestServer, String, String) {
    let tmp = tempfile::tempdir().unwrap();
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
        .create_user(&cmdock_server::store::models::NewUser {
            username: "testuser".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let user_id = user.id.clone();

    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();

    let config = common::test_server_config_with_admin_token(data_dir.clone(), token.clone());

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
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);
    std::mem::forget(tmp);

    (server, token, user_id)
}

#[tokio::test]
async fn test_summary_quarantine_returns_503() {
    let (server, token, user_id) = setup_test_server_with_admin().await;

    // Verify summary works before quarantine
    let (h1, v1) = auth_header(&token);
    let resp = server.get("/api/summary").add_header(h1, v1).await;
    resp.assert_status_ok();

    // Quarantine the user via admin offline endpoint
    let (h_off, v_off) = auth_header(&token);
    let offline_resp = server
        .post(&format!("/admin/user/{user_id}/offline"))
        .add_header(h_off, v_off)
        .await;
    offline_resp.assert_status_ok();

    // Summary should now return 503 Service Unavailable
    let (h2, v2) = auth_header(&token);
    let quarantined_resp = server.get("/api/summary").add_header(h2, v2).await;
    quarantined_resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    // Bring user back online
    let (h_on, v_on) = auth_header(&token);
    let online_resp = server
        .post(&format!("/admin/user/{user_id}/online"))
        .add_header(h_on, v_on)
        .await;
    online_resp.assert_status_ok();

    // Summary should work again
    let (h3, v3) = auth_header(&token);
    let recovered_resp = server.get("/api/summary").add_header(h3, v3).await;
    recovered_resp.assert_status_ok();

    let body: Value = recovered_resp.json();
    assert_eq!(body["type"], "today");
    assert!(body["summary"].as_str().is_some());
}
