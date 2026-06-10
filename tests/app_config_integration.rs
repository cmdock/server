//! Integration tests for app-config mega endpoint and typed config CRUD.
//!
//! Tests GET /api/app-config, shopping/context/store/preset CRUD,
//! and auth requirements.

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

struct UserInfo {
    id: String,
    token: String,
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
    cmdock_server::admin::prefix::backfill_missing_user_prefixes(store.as_ref())
        .await
        .unwrap();

    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();

    UserInfo { id: user.id, token }
}

struct TestEnv {
    server: TestServer,
    _tmp: TempDir,
    store: Arc<dyn ConfigStore>,
    user: UserInfo,
}

struct MultiUserTestEnv {
    server: TestServer,
    _tmp: TempDir,
    user_a: UserInfo,
    user_b: UserInfo,
}

fn build_app(state: AppState) -> Router {
    Router::new()
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
        )
}

async fn setup() -> TestEnv {
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

    let user = create_user_with_token(&store, "appconfig_user").await;

    let config = common::test_server_config(data_dir.clone());

    let state = AppState::new(store.clone(), sqlite_store.clone(), &config);
    let app = build_app(state);
    let server = TestServer::new(app);

    TestEnv {
        server,
        _tmp: tmp,
        store,
        user,
    }
}

async fn setup_multi_user() -> MultiUserTestEnv {
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

    let user_a = create_user_with_token(&store, "appconfig_user_a").await;
    let user_b = create_user_with_token(&store, "appconfig_user_b").await;

    let config = common::test_server_config(data_dir.clone());

    let state = AppState::new(store, sqlite_store, &config);
    let app = build_app(state);
    let server = TestServer::new(app);

    MultiUserTestEnv {
        server,
        _tmp: tmp,
        user_a,
        user_b,
    }
}

// --- Tests ---

#[tokio::test]
async fn test_app_config_reconciles_default_views() {
    let env = setup().await;
    let (h, v) = auth_header(&env.user.token);

    let resp = env.server.get("/api/app-config").add_header(h, v).await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    let contexts = body["contexts"].as_array().unwrap();
    assert_eq!(
        contexts.len(),
        3,
        "app-config should seed 3 default contexts (personal/work/health)"
    );
    let views = body["views"].as_array().unwrap();
    assert_eq!(views.len(), 6, "app-config should include default views");
    assert!(body["presets"].as_array().unwrap().is_empty());
    assert!(body["stores"].as_array().unwrap().is_empty());
    assert!(body["shopping"].is_null());
    assert!(body["geofences"].as_array().unwrap().is_empty());

    let duesoon = views.iter().find(|v| v["id"] == "duesoon").unwrap();
    let action = views.iter().find(|v| v["id"] == "action").unwrap();
    let personal = views.iter().find(|v| v["id"] == "personal").unwrap();

    assert_eq!(duesoon["contextFiltered"], true);
    assert!(duesoon.get("contextId").is_none() || duesoon["contextId"].is_null());
    assert_eq!(action["contextFiltered"], true);
    assert_eq!(personal["contextFiltered"], true);
    assert_eq!(personal["contextId"], "personal");
}

#[tokio::test]
async fn test_app_config_reconciles_stale_builtin_view_flags() {
    let env = setup().await;

    let mut defaults = cmdock_server::views::defaults::default_views();
    for view in &mut defaults {
        if view.id == "duesoon" || view.id == "action" {
            view.context_filtered = false;
        }
        if view.id == "duesoon" {
            view.label = "Old Due Soon".to_string();
            view.filter = "status:pending due.before:3d".to_string();
            view.user_modified = true;
        }
        view.template_version = cmdock_server::views::defaults::VIEWSET_VERSION - 1;
    }

    for view in &defaults {
        env.store.upsert_view(&env.user.id, view).await.unwrap();
    }

    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/app-config").add_header(h, v).await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    let views = body["views"].as_array().unwrap();
    let duesoon = views.iter().find(|v| v["id"] == "duesoon").unwrap();
    let action = views.iter().find(|v| v["id"] == "action").unwrap();

    assert_eq!(duesoon["label"], "Old Due Soon");
    assert_eq!(
        duesoon["filter"],
        "status:pending due.before:3d -BLOCKED -WAITING"
    );
    assert_eq!(duesoon["contextFiltered"], true);
    assert_eq!(
        action["filter"],
        "status:pending -BLOCKED -WAITING priority:H"
    );
    assert_eq!(action["contextFiltered"], true);
}

#[tokio::test]
async fn test_app_config_preserves_builtin_user_edits_but_fixes_actionable_filter_contract() {
    let env = setup().await;

    let (h, v) = auth_header(&env.user.token);
    env.server
        .get("/api/views")
        .add_header(h, v)
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.user.token);
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

    let (h, v) = auth_header(&env.user.token);
    env.server
        .put("/api/views/action")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "My Action",
            "icon": "bolt.fill",
            "filter": "status:pending priority:H +next",
            "group": null
        }))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/app-config").add_header(h, v).await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    let views = body["views"].as_array().unwrap();
    let duesoon = views.iter().find(|v| v["id"] == "duesoon").unwrap();
    let action = views.iter().find(|v| v["id"] == "action").unwrap();

    assert_eq!(duesoon["label"], "My Due Soon");
    assert_eq!(
        duesoon["filter"],
        "status:pending due.before:3d -BLOCKED -WAITING"
    );
    assert_eq!(duesoon["contextFiltered"], true);

    assert_eq!(action["label"], "My Action");
    assert_eq!(
        action["filter"],
        "status:pending priority:H +next -BLOCKED -WAITING"
    );
    assert_eq!(action["contextFiltered"], true);
}

#[tokio::test]
async fn test_shopping_config_roundtrip_and_delete() {
    let env = setup().await;

    let (h, v) = auth_header(&env.user.token);
    env.server
        .put("/api/shopping-config")
        .add_header(h, v)
        .json(&serde_json::json!({
            "project": "PERSONAL.Home",
            "defaultTags": ["shopping", "errand"]
        }))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/app-config").add_header(h, v).await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    assert_eq!(body["shopping"]["project"], "PERSONAL.Home");
    assert_eq!(body["shopping"]["defaultTags"][0], "shopping");
    assert_eq!(body["shopping"]["defaultTags"][1], "errand");

    let (h, v) = auth_header(&env.user.token);
    env.server
        .delete("/api/shopping-config")
        .add_header(h, v)
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/app-config").add_header(h, v).await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    assert!(body["shopping"].is_null());
}

#[tokio::test]
async fn test_context_crud() {
    let env = setup().await;

    // PUT a user-defined context with an ID that doesn't collide with seeds.
    let (h, v) = auth_header(&env.user.token);
    let resp = env
        .server
        .put("/api/contexts/garage")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Garage",
            "projectPrefixes": ["garage."],
            "color": "#ff0000",
            "icon": "wrench"
        }))
        .await;
    resp.assert_status_ok();

    // List contexts: 3 seeded + 1 user-created.
    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/contexts").add_header(h, v).await;
    resp.assert_status_ok();

    let contexts: Vec<Value> = resp.json();
    assert_eq!(contexts.len(), 4, "3 default + 1 user context");
    let garage = contexts.iter().find(|c| c["id"] == "garage").unwrap();
    assert_eq!(garage["label"], "Garage");
    assert_eq!(garage["projectPrefixes"][0], "garage.");
    assert_eq!(garage["color"], "#ff0000");
    assert_eq!(garage["icon"], "wrench");

    // Delete user-created context (hard delete since origin=user)
    let (h, v) = auth_header(&env.user.token);
    let resp = env
        .server
        .delete("/api/contexts/garage")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::NO_CONTENT);

    // Verify deleted: only the 3 seeds remain.
    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/contexts").add_header(h, v).await;
    resp.assert_status_ok();
    let contexts: Vec<Value> = resp.json();
    assert_eq!(contexts.len(), 3, "user context deleted, seeds remain");
    assert!(
        !contexts.iter().any(|c| c["id"] == "garage"),
        "garage should be gone"
    );
}

#[tokio::test]
async fn test_store_crud() {
    let env = setup().await;

    // Create store
    let (h, v) = auth_header(&env.user.token);
    let resp = env
        .server
        .put("/api/stores/groceries")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Groceries",
            "tag": "groceries"
        }))
        .await;
    resp.assert_status_ok();

    // List stores
    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/stores").add_header(h, v).await;
    resp.assert_status_ok();

    let stores: Vec<Value> = resp.json();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0]["id"], "groceries");
    assert_eq!(stores[0]["label"], "Groceries");
    assert_eq!(stores[0]["tag"], "groceries");

    // Delete store
    let (h, v) = auth_header(&env.user.token);
    let resp = env
        .server
        .delete("/api/stores/groceries")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::NO_CONTENT);

    // Verify deleted
    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/stores").add_header(h, v).await;
    resp.assert_status_ok();
    let stores: Vec<Value> = resp.json();
    assert!(stores.is_empty(), "Store should be deleted");
}

#[tokio::test]
async fn test_preset_crud() {
    let env = setup().await;

    // Create preset
    let (h, v) = auth_header(&env.user.token);
    let resp = env
        .server
        .put("/api/presets/quick-work")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Quick Work",
            "rawSuffix": "project:work +office"
        }))
        .await;
    resp.assert_status_ok();

    // Verify via app-config (presets have no dedicated list endpoint)
    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/app-config").add_header(h, v).await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    let presets = body["presets"].as_array().unwrap();
    assert_eq!(presets.len(), 1);
    assert_eq!(presets[0]["id"], "quick-work");
    assert_eq!(presets[0]["label"], "Quick Work");
    assert_eq!(presets[0]["rawSuffix"], "project:work +office");

    // Delete preset
    let (h, v) = auth_header(&env.user.token);
    let resp = env
        .server
        .delete("/api/presets/quick-work")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::NO_CONTENT);

    // Verify deleted
    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/app-config").add_header(h, v).await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert!(body["presets"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_app_config_reflects_changes() {
    let env = setup().await;

    // Create a context
    let (h, v) = auth_header(&env.user.token);
    env.server
        .put("/api/contexts/home")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Home",
            "projectPrefixes": ["home."],
            "color": null,
            "icon": null
        }))
        .await
        .assert_status_ok();

    // Create a store
    let (h, v) = auth_header(&env.user.token);
    env.server
        .put("/api/stores/hardware")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Hardware",
            "tag": "hardware"
        }))
        .await
        .assert_status_ok();

    // Create a preset
    let (h, v) = auth_header(&env.user.token);
    env.server
        .put("/api/presets/errand")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Errand",
            "rawSuffix": "+errand due:tomorrow"
        }))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.user.token);
    env.server
        .put("/api/shopping-config")
        .add_header(h, v)
        .json(&serde_json::json!({
            "project": "PERSONAL.Home",
            "defaultTags": ["shopping"]
        }))
        .await
        .assert_status_ok();

    // Verify all reflected in app-config
    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/app-config").add_header(h, v).await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    let contexts = body["contexts"].as_array().unwrap();
    assert_eq!(contexts.len(), 4, "3 seeds + 1 user-created (home)");
    assert!(contexts.iter().any(|c| c["id"] == "home"));
    assert_eq!(body["stores"].as_array().unwrap().len(), 1);
    assert_eq!(body["stores"][0]["id"], "hardware");
    assert_eq!(body["presets"].as_array().unwrap().len(), 1);
    assert_eq!(body["presets"][0]["id"], "errand");
    assert_eq!(body["shopping"]["project"], "PERSONAL.Home");
    assert_eq!(body["shopping"]["defaultTags"][0], "shopping");
}

#[tokio::test]
async fn test_app_config_requires_auth() {
    let env = setup().await;

    let resp = env.server.get("/api/app-config").await;
    resp.assert_status_unauthorized();
}

#[tokio::test]
async fn test_context_cross_user_isolation() {
    let env = setup_multi_user().await;

    // User A creates a context with an ID that doesn't collide with seeds.
    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .put("/api/contexts/garage")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Garage",
            "projectPrefixes": ["garage."],
            "color": "#ff0000",
            "icon": "wrench"
        }))
        .await
        .assert_status_ok();

    // User B should not see user A's `garage` context — only their own 3 seeds.
    let (h, v) = auth_header(&env.user_b.token);
    let resp = env.server.get("/api/contexts").add_header(h, v).await;
    resp.assert_status_ok();
    let contexts: Vec<Value> = resp.json();
    assert_eq!(
        contexts.len(),
        3,
        "user B should see only the 3 seeded contexts"
    );
    assert!(
        !contexts.iter().any(|c| c["id"] == "garage"),
        "user B should not see user A's garage context"
    );

    // User A should see seeds + their own.
    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/contexts").add_header(h, v).await;
    resp.assert_status_ok();
    let contexts: Vec<Value> = resp.json();
    assert_eq!(contexts.len(), 4, "user A: 3 seeds + 1 user-created");
    assert!(contexts.iter().any(|c| c["id"] == "garage"));

    // Also verify via /api/app-config mega-endpoint
    let (h, v) = auth_header(&env.user_b.token);
    let resp = env.server.get("/api/app-config").add_header(h, v).await;
    resp.assert_status_ok();
    let config: Value = resp.json();
    let app_contexts = config["contexts"].as_array().unwrap();
    assert_eq!(app_contexts.len(), 3, "user B's app-config: only 3 seeds");
    assert!(
        !app_contexts.iter().any(|c| c["id"] == "garage"),
        "user B's app-config should not include user A's contexts"
    );
}

#[tokio::test]
async fn test_upsert_context_invalid_json() {
    let env = setup().await;
    let (h, v) = auth_header(&env.user.token);

    // Send malformed JSON — Axum's Json extractor should reject it
    let resp = env
        .server
        .put("/api/contexts/broken")
        .add_header(h, v)
        .content_type("application/json")
        .bytes("{not valid json!!!}".into())
        .await;

    let status = resp.status_code().as_u16();
    assert!(
        status == 400 || status == 422,
        "malformed JSON should return 400 or 422, got {status}"
    );
}

#[tokio::test]
async fn test_context_validation_rejects_bad_id_and_label() {
    let env = setup().await;
    let (h, v) = auth_header(&env.user.token);

    let resp = env
        .server
        .put("/api/contexts/bad..id")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "label": "Work",
            "projectPrefixes": ["work."],
            "color": "#ff0000",
            "icon": "briefcase"
        }))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

    let resp = env
        .server
        .put("/api/contexts/work")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "label": "   ",
            "projectPrefixes": ["work."],
            "color": "#ff0000",
            "icon": "briefcase"
        }))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_store_validation_rejects_bad_tag() {
    let env = setup().await;
    let (h, v) = auth_header(&env.user.token);

    let resp = env
        .server
        .put("/api/stores/groceries")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "label": "Groceries",
            "tag": "bad\ntag"
        }))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_preset_validation_rejects_bad_id() {
    let env = setup().await;
    let (h, v) = auth_header(&env.user.token);

    let resp = env
        .server
        .put("/api/presets/bad..id")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "label": "Quick Work",
            "rawSuffix": "project:work +office"
        }))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_shopping_config_validation_rejects_bad_project() {
    let env = setup().await;
    let (h, v) = auth_header(&env.user.token);

    let resp = env
        .server
        .put("/api/shopping-config")
        .add_header(h, v)
        .json(&serde_json::json!({
            "project": " \t ",
            "defaultTags": ["shopping"]
        }))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_get_app_config_empty_has_all_fields() {
    // Dedicated test verifying all expected top-level fields exist on a fresh DB,
    // even when every collection is empty.
    let env = setup().await;
    let (h, v) = auth_header(&env.user.token);

    let resp = env.server.get("/api/app-config").add_header(h, v).await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    let obj = body
        .as_object()
        .expect("app-config should be a JSON object");
    for field in &["contexts", "views", "presets", "stores", "geofences"] {
        assert!(
            obj.contains_key(*field),
            "app-config response missing expected field: {field}"
        );
        assert!(
            obj[*field].as_array().is_some(),
            "field '{field}' should be an array"
        );
    }
    assert!(
        obj.contains_key("shopping"),
        "app-config response missing expected field: shopping"
    );
    assert!(obj["shopping"].is_null(), "shopping should default to null");
}

#[tokio::test]
async fn test_list_contexts_seeds_defaults_on_first_access() {
    let env = setup().await;

    // First hit on /api/contexts should seed personal/work/health.
    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/contexts").add_header(h, v).await;
    resp.assert_status_ok();

    let contexts: Vec<Value> = resp.json();
    let mut ids: Vec<&str> = contexts.iter().map(|c| c["id"].as_str().unwrap()).collect();
    ids.sort();
    assert_eq!(ids, vec!["health", "personal", "work"]);

    let personal = contexts.iter().find(|c| c["id"] == "personal").unwrap();
    assert_eq!(personal["label"], "Personal");
    assert_eq!(personal["projectPrefixes"][0], "PERSONAL");
    let work = contexts.iter().find(|c| c["id"] == "work").unwrap();
    assert_eq!(work["projectPrefixes"][0], "WORK");
    let health = contexts.iter().find(|c| c["id"] == "health").unwrap();
    assert_eq!(health["projectPrefixes"][0], "HEALTH");
}

#[tokio::test]
async fn test_user_modified_seed_context_preserved() {
    let env = setup().await;

    // Force seeding.
    let (h, v) = auth_header(&env.user.token);
    env.server
        .get("/api/contexts")
        .add_header(h, v)
        .await
        .assert_status_ok();

    // User customises the `work` seed.
    let (h, v) = auth_header(&env.user.token);
    env.server
        .put("/api/contexts/work")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "My Work",
            "projectPrefixes": ["ACME", "WORK"],
            "color": "#000000",
            "icon": "briefcase"
        }))
        .await
        .assert_status_ok();

    // Subsequent reconcile must not clobber the user's edits.
    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/contexts").add_header(h, v).await;
    resp.assert_status_ok();
    let contexts: Vec<Value> = resp.json();
    let work = contexts.iter().find(|c| c["id"] == "work").unwrap();
    assert_eq!(work["label"], "My Work");
    assert_eq!(work["projectPrefixes"][0], "ACME");
    assert_eq!(work["color"], "#000000");
}

#[tokio::test]
async fn test_deleted_seed_context_stays_tombstoned() {
    let env = setup().await;

    // Seed.
    let (h, v) = auth_header(&env.user.token);
    env.server
        .get("/api/contexts")
        .add_header(h, v)
        .await
        .assert_status_ok();

    // Delete builtin (soft tombstone).
    let (h, v) = auth_header(&env.user.token);
    env.server
        .delete("/api/contexts/health")
        .add_header(h, v)
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    // Subsequent reconcile must respect the tombstone.
    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/contexts").add_header(h, v).await;
    resp.assert_status_ok();
    let contexts: Vec<Value> = resp.json();
    assert_eq!(
        contexts.len(),
        2,
        "personal + work remain; health tombstoned"
    );
    assert!(!contexts.iter().any(|c| c["id"] == "health"));
}

#[tokio::test]
async fn test_view_put_rejects_dangling_context_id() {
    let env = setup().await;

    // PUT a custom view referring to a context that does not exist.
    let (h, v) = auth_header(&env.user.token);
    let resp = env
        .server
        .put("/api/views/my-custom")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Custom",
            "icon": "star",
            "filter": "status:pending",
            "group": null,
            "contextId": "does-not-exist",
            "displayMode": "list"
        }))
        .await;
    assert_eq!(resp.status_code(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text(), "INVALID_CONTEXT_ID");
}

#[tokio::test]
async fn test_view_put_accepts_valid_context_id() {
    let env = setup().await;

    // Trigger context seeding.
    let (h, v) = auth_header(&env.user.token);
    env.server
        .get("/api/contexts")
        .add_header(h, v)
        .await
        .assert_status_ok();

    // Valid seeded context is accepted on a custom view.
    let (h, v) = auth_header(&env.user.token);
    let resp = env
        .server
        .put("/api/views/my-personal")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "My Personal",
            "icon": "person",
            "filter": "status:pending",
            "group": null,
            "contextId": "personal",
            "displayMode": "list"
        }))
        .await;
    resp.assert_status_ok();

    // GET back: contextId persisted, contextFiltered=true (custom view, since context_id is set).
    let (h, v) = auth_header(&env.user.token);
    let resp = env.server.get("/api/views").add_header(h, v).await;
    resp.assert_status_ok();
    let views: Vec<Value> = resp.json();
    let mine = views.iter().find(|v| v["id"] == "my-personal").unwrap();
    assert_eq!(mine["contextId"], "personal");
    assert_eq!(mine["contextFiltered"], true);
}

#[tokio::test]
async fn test_view_put_seeds_contexts_so_builtin_recreate_works() {
    // Re-creating a deleted/missing builtin view via PUT must succeed even
    // if /api/contexts hasn't been called yet — upsert_view reconciles
    // contexts before validating context_id.
    let env = setup().await;

    let (h, v) = auth_header(&env.user.token);
    let resp = env
        .server
        .put("/api/views/personal")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Personal Reborn",
            "icon": "person",
            "filter": "status:pending",
            "group": "project",
            "displayMode": "grouped"
        }))
        .await;
    resp.assert_status_ok();
}
