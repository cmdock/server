//! Integration tests for view definition CRUD endpoints.
//!
//! Tests GET /api/views, PUT /api/views/{id}, DELETE /api/views/{id}
//! including cross-user isolation, auth requirements, and default view
//! behaviour (builtin views seeded on first access, tombstoning, etc.).

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
use cmdock_server::health;
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::summary;
use cmdock_server::sync;
use cmdock_server::tasks;
use cmdock_server::views;

/// Number of built-in default views seeded for every user.
const DEFAULT_VIEW_COUNT: usize = 6;

// --- Setup ---

struct UserInfo {
    token: String,
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

async fn create_user_with_token(store: &Arc<dyn ConfigStore>, username: &str) -> UserInfo {
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

    UserInfo { token }
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

    let user_a = create_user_with_token(&store, "user_a").await;
    let user_b = create_user_with_token(&store, "user_b").await;

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
        user_a,
        user_b,
    }
}

// --- Tests ---

/// New user gets default built-in views on first GET /api/views (lazy reconcile).
#[tokio::test]
async fn test_new_user_gets_default_views() {
    let env = setup().await;
    let (h, v) = auth_header(&env.user_a.token);

    let resp = env.server.get("/api/views").add_header(h, v).await;
    resp.assert_status_ok();

    let views: Vec<Value> = resp.json();
    assert_eq!(
        views.len(),
        DEFAULT_VIEW_COUNT,
        "New user should get {DEFAULT_VIEW_COUNT} default views"
    );

    // Verify expected view IDs
    let ids: Vec<&str> = views.iter().map(|v| v["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"duesoon"), "should have duesoon view");
    assert!(ids.contains(&"action"), "should have action view");
    assert!(ids.contains(&"shopping"), "should have shopping view");
}

/// User can create custom views alongside defaults.
#[tokio::test]
async fn test_create_custom_view_alongside_defaults() {
    let env = setup().await;

    // Create a custom view
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .put("/api/views/myview")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "My Custom View",
            "icon": "star",
            "filter": "status:pending +important",
            "group": null
        }))
        .await;
    resp.assert_status_ok();

    // List views — should have defaults + custom
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/views").add_header(h, v).await;
    resp.assert_status_ok();

    let views: Vec<Value> = resp.json();
    assert_eq!(
        views.len(),
        DEFAULT_VIEW_COUNT + 1,
        "Should have {DEFAULT_VIEW_COUNT} defaults + 1 custom"
    );
    assert!(
        views.iter().any(|v| v["id"] == "myview"),
        "Custom view should be present"
    );
}

#[tokio::test]
async fn test_view_validation_rejects_invalid_payload_and_id() {
    let env = setup().await;

    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .put("/api/views/myview")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "label": "   ",
            "icon": "star",
            "filter": "status:pending",
            "group": null
        }))
        .await
        .assert_status_bad_request();

    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .put("/api/views/myview")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Valid",
            "icon": "bad\nicon",
            "filter": "status:pending",
            "group": null
        }))
        .await
        .assert_status_bad_request();
}

/// User can modify a builtin view. Actionable builtin views keep user-facing
/// edits, but the server still enforces blocked/waiting exclusion in the filter.
#[tokio::test]
async fn test_modify_builtin_view() {
    let env = setup().await;

    // First GET to trigger reconcile
    let (h, v) = auth_header(&env.user_a.token);
    env.server.get("/api/views").add_header(h, v).await;

    // Modify the builtin "duesoon" view
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .put("/api/views/duesoon")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "My Due Soon",
            "icon": "clock.fill",
            "filter": "status:pending due.before:3d",
            "group": null
        }))
        .await
        .assert_status_ok();

    // Verify modified values
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/views").add_header(h, v).await;
    let views: Vec<Value> = resp.json();
    let duesoon = views.iter().find(|v| v["id"] == "duesoon").unwrap();
    assert_eq!(duesoon["label"], "My Due Soon");
    assert_eq!(
        duesoon["filter"],
        "status:pending due.before:3d -BLOCKED -WAITING"
    );

    // Total count unchanged (modification, not creation)
    assert_eq!(views.len(), DEFAULT_VIEW_COUNT);
}

/// Deleting a user-created view removes it completely.
#[tokio::test]
async fn test_delete_user_view() {
    let env = setup().await;

    // Create a user view
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .put("/api/views/deleteme")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Delete Me",
            "icon": "trash",
            "filter": "status:pending",
            "group": null
        }))
        .await
        .assert_status_ok();

    // Delete it
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .delete("/api/views/deleteme")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::NO_CONTENT);

    // Verify gone — only defaults remain
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/views").add_header(h, v).await;
    let views: Vec<Value> = resp.json();
    assert_eq!(views.len(), DEFAULT_VIEW_COUNT);
    assert!(
        !views.iter().any(|v| v["id"] == "deleteme"),
        "Deleted view should not appear"
    );
}

/// Deleting a builtin view hides it (tombstone) — it won't reappear.
#[tokio::test]
async fn test_delete_builtin_view_tombstones() {
    let env = setup().await;

    // First GET to trigger reconcile
    let (h, v) = auth_header(&env.user_a.token);
    env.server.get("/api/views").add_header(h, v).await;

    // Delete the builtin "shopping" view
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .delete("/api/views/shopping")
        .add_header(h, v)
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    // Verify hidden — one fewer view
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/views").add_header(h, v).await;
    let views: Vec<Value> = resp.json();
    assert_eq!(views.len(), DEFAULT_VIEW_COUNT - 1);
    assert!(
        !views.iter().any(|v| v["id"] == "shopping"),
        "Deleted builtin should be hidden"
    );

    // Second GET should NOT re-create the deleted builtin (tombstone respected)
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/views").add_header(h, v).await;
    let views: Vec<Value> = resp.json();
    assert_eq!(
        views.len(),
        DEFAULT_VIEW_COUNT - 1,
        "Tombstone should prevent re-creation"
    );
}

#[tokio::test]
async fn test_delete_nonexistent_view() {
    let env = setup().await;

    let (h, v) = auth_header(&env.user_a.token);
    let resp = env
        .server
        .delete("/api/views/nonexistent")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::NO_CONTENT);
}

/// Each user gets their own set of default views (cross-user isolation).
#[tokio::test]
async fn test_cross_user_isolation() {
    let env = setup().await;

    // User A creates a custom view
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .put("/api/views/private")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "User A Private",
            "icon": "lock",
            "filter": "status:pending",
            "group": null
        }))
        .await
        .assert_status_ok();

    // User B should have defaults only (no User A's custom view)
    let (h, v) = auth_header(&env.user_b.token);
    let resp = env.server.get("/api/views").add_header(h, v).await;
    resp.assert_status_ok();
    let views: Vec<Value> = resp.json();
    assert_eq!(
        views.len(),
        DEFAULT_VIEW_COUNT,
        "User B should only have defaults"
    );
    assert!(
        !views.iter().any(|v| v["id"] == "private"),
        "User B should not see User A's custom view"
    );

    // User A should have defaults + custom
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/views").add_header(h, v).await;
    let views: Vec<Value> = resp.json();
    assert_eq!(views.len(), DEFAULT_VIEW_COUNT + 1);
    assert!(views.iter().any(|v| v["id"] == "private"));
}

/// Regression: user-created view with same ID as a builtin must not be overwritten.
#[tokio::test]
async fn test_user_view_id_collision_with_builtin() {
    let env = setup().await;

    // User creates a custom view with ID "work" BEFORE first GET (before reconcile)
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .put("/api/views/work")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "My Custom Work",
            "icon": "hammer",
            "filter": "status:pending +mywork",
            "group": null
        }))
        .await
        .assert_status_ok();

    // First GET triggers reconcile — should NOT overwrite the user's "work" view
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/views").add_header(h, v).await;
    resp.assert_status_ok();
    let views: Vec<Value> = resp.json();

    let work = views.iter().find(|v| v["id"] == "work").unwrap();
    assert_eq!(
        work["label"], "My Custom Work",
        "User's custom 'work' view should NOT be overwritten by builtin"
    );
    assert_eq!(
        work["filter"], "status:pending +mywork",
        "User's filter should be preserved"
    );
}

/// PUT before GET: user creates view, then GET triggers reconcile — user's view preserved.
#[tokio::test]
async fn test_put_before_get_not_clobbered() {
    let env = setup().await;

    // User creates "duesoon" with custom filter BEFORE first GET
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .put("/api/views/duesoon")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "My Due Soon",
            "icon": "timer",
            "filter": "status:pending due.before:1d",
            "group": null
        }))
        .await
        .assert_status_ok();

    // GET triggers reconcile
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/views").add_header(h, v).await;
    resp.assert_status_ok();
    let views: Vec<Value> = resp.json();

    let duesoon = views.iter().find(|v| v["id"] == "duesoon").unwrap();
    assert_eq!(
        duesoon["label"], "My Due Soon",
        "User's pre-GET 'duesoon' should not be clobbered by reconcile"
    );
    assert_eq!(duesoon["filter"], "status:pending due.before:1d");
}

#[tokio::test]
async fn test_views_require_auth() {
    let env = setup().await;

    let resp = env.server.get("/api/views").await;
    resp.assert_status_unauthorized();

    let resp = env
        .server
        .put("/api/views/noauth")
        .json(&serde_json::json!({
            "label": "No Auth",
            "icon": "x",
            "filter": "status:pending",
            "group": null
        }))
        .await;
    resp.assert_status_unauthorized();

    let resp = env.server.delete("/api/views/noauth").await;
    resp.assert_status_unauthorized();
}
