//! Integration tests for task endpoints.
//!
//! Tests task modification, invalid UUID handling, unknown UUID,
//! complete/undo conflicts, and auth requirements.

mod common;

use chrono::{Duration, Utc};
use std::path::PathBuf;
use std::sync::Arc;

use axum::http::{header, HeaderValue, Method};
use axum::Router;
use axum_test::TestServer;
use serde_json::Value;
use taskchampion::storage::AccessMode;
use taskchampion::{Operations, Replica, SqliteStorage};
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
    data_dir: PathBuf,
    user_id: String,
    token: String,
    store: Arc<dyn ConfigStore>,
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

    let user = store
        .create_user(&NewUser {
            username: "tasks_user".to_string(),
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

    std::fs::create_dir_all(tmp.path().join("users").join(&user.id)).unwrap();

    let config = common::test_server_config(data_dir.clone());

    let state = AppState::new(store.clone(), sqlite_store.clone(), &config);

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
        data_dir,
        user_id: user.id,
        token,
        store,
    }
}

/// Helper to create a task and return its UUID.
async fn create_task(env: &TestEnv, raw: &str) -> String {
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": raw}))
        .await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    // output is "Created task {uuid}."
    let output = body["output"].as_str().unwrap();
    output
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap()
        .to_string()
}

async fn open_replica(env: &TestEnv) -> Replica<SqliteStorage> {
    let user_dir = env.data_dir.join("users").join(&env.user_id);
    let storage = SqliteStorage::new(&user_dir, AccessMode::ReadWrite, true)
        .await
        .unwrap();
    Replica::new(storage)
}

async fn mark_task_blocked(env: &TestEnv, task_uuid: &str, dependency_uuid: &str) {
    let mut replica = open_replica(env).await;
    let mut ops = Operations::new();
    let mut task = replica
        .get_task(task_uuid.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    task.set_value(
        format!("dep_{dependency_uuid}"),
        Some(String::new()),
        &mut ops,
    )
    .unwrap();
    replica.commit_operations(ops).await.unwrap();
}

async fn mark_task_waiting(env: &TestEnv, task_uuid: &str, wait: chrono::DateTime<Utc>) {
    let mut replica = open_replica(env).await;
    let mut ops = Operations::new();
    let mut task = replica
        .get_task(task_uuid.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    task.set_wait(Some(wait), &mut ops).unwrap();
    replica.commit_operations(ops).await.unwrap();
}

// --- Tests ---

#[tokio::test]
async fn test_modify_task() {
    let env = setup().await;

    let uuid = create_task(&env, "+test Modify me").await;

    // Modify the description
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"description": "Modified description"}))
        .await;
    resp.assert_status_ok();

    // Verify via GET /api/tasks
    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["description"], "Modified description");
}

#[tokio::test]
async fn test_invalid_uuid_returns_400() {
    let env = setup().await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks/not-a-uuid/done")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_task_validation_rejects_invalid_payloads() {
    let env = setup().await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "   "}))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);

    let (h, v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "bad\nnewline"}))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);

    let uuid = create_task(&env, "+test Validate modify").await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"description": "   "}))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"tags": ["ok", ""]}))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_unknown_uuid_returns_404() {
    let env = setup().await;

    // Create at least one task so the replica DB exists
    create_task(&env, "+test Dummy task").await;

    let random_uuid = uuid::Uuid::new_v4();
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{random_uuid}/done"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_complete_twice_returns_409() {
    let env = setup().await;

    let uuid = create_task(&env, "+test Complete twice").await;

    // First complete — should succeed
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/done"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    // Second complete — should return 409 Conflict
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/done"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_undo_completed_task_returns_to_pending() {
    let env = setup().await;

    let uuid = create_task(&env, "+test Undo complete").await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/done"))
        .add_header(h, v)
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/undo"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    assert!(resp.json::<Value>()["success"].as_bool().unwrap());

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();
    let tasks: Vec<Value> = resp.json();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["uuid"], uuid);
    assert_eq!(tasks[0]["status"], "pending");
}

#[tokio::test]
async fn test_undo_non_completed_task_returns_409() {
    let env = setup().await;

    let uuid = create_task(&env, "+test Undo pending").await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/undo"))
        .add_header(h, v)
        .await
        .assert_status(axum::http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_tasks_require_auth() {
    let env = setup().await;

    let resp = env.server.get("/api/tasks").await;
    resp.assert_status_unauthorized();
}

// --- View filter tests ---

#[tokio::test]
async fn test_list_tasks_with_view_filter() {
    let env = setup().await;

    // Create a view with filter "status:pending"
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .put("/api/views/pending-view")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Pending Tasks",
            "icon": "checklist",
            "filter": "status:pending",
            "group": null
        }))
        .await;
    resp.assert_status_ok();

    // Create some tasks
    let _uuid1 = create_task(&env, "+test View filter task one").await;
    let _uuid2 = create_task(&env, "+test View filter task two").await;

    // List tasks with the view filter
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks")
        .add_query_param("view", "pending-view")
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    assert_eq!(
        tasks.len(),
        2,
        "view filter should return both pending tasks"
    );
}

#[tokio::test]
async fn test_task_item_exposes_blocked_and_waiting_state() {
    let env = setup().await;

    let dependency_uuid = create_task(&env, "+test prerequisite").await;
    let blocked_uuid = create_task(&env, "project:PERSONAL blocked task").await;
    let waiting_uuid = create_task(&env, "project:PERSONAL waiting task").await;

    mark_task_blocked(&env, &blocked_uuid, &dependency_uuid).await;
    mark_task_waiting(&env, &waiting_uuid, Utc::now() + Duration::days(3)).await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    let blocked = tasks.iter().find(|t| t["uuid"] == blocked_uuid).unwrap();
    let waiting = tasks.iter().find(|t| t["uuid"] == waiting_uuid).unwrap();

    assert_eq!(blocked["blocked"], true);
    assert_eq!(blocked["waiting"], false);
    assert_eq!(waiting["blocked"], false);
    assert_eq!(waiting["waiting"], true);
}

#[tokio::test]
async fn test_duesoon_view_excludes_blocked_tasks() {
    let env = setup().await;

    let dependency_uuid = create_task(&env, "+test prerequisite").await;
    let blocked_uuid = create_task(&env, "project:PERSONAL due:tomorrow blocked due soon").await;
    let visible_uuid = create_task(&env, "project:PERSONAL due:tomorrow visible due soon").await;

    mark_task_blocked(&env, &blocked_uuid, &dependency_uuid).await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks")
        .add_query_param("view", "duesoon")
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    assert!(
        tasks.iter().any(|t| t["uuid"] == visible_uuid),
        "unblocked due-soon task should remain visible"
    );
    assert!(
        !tasks.iter().any(|t| t["uuid"] == blocked_uuid),
        "blocked due-soon task should be excluded from duesoon"
    );
}

#[tokio::test]
async fn test_action_view_excludes_waiting_tasks() {
    let env = setup().await;

    let waiting_uuid = create_task(&env, "priority:H project:PERSONAL waiting action task").await;
    let visible_uuid = create_task(&env, "priority:H project:PERSONAL visible action task").await;

    mark_task_waiting(&env, &waiting_uuid, Utc::now() + Duration::days(2)).await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks")
        .add_query_param("view", "action")
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    assert!(
        tasks.iter().any(|t| t["uuid"] == visible_uuid),
        "high-priority actionable task should remain visible"
    );
    assert!(
        !tasks.iter().any(|t| t["uuid"] == waiting_uuid),
        "waiting task should be excluded from action"
    );
}

#[tokio::test]
async fn test_named_context_view_keeps_blocked_and_waiting_tasks_visible() {
    let env = setup().await;

    let dependency_uuid = create_task(&env, "project:PERSONAL prerequisite").await;
    let blocked_uuid = create_task(&env, "project:PERSONAL blocked personal task").await;
    let waiting_uuid = create_task(&env, "project:PERSONAL waiting personal task").await;

    mark_task_blocked(&env, &blocked_uuid, &dependency_uuid).await;
    mark_task_waiting(&env, &waiting_uuid, Utc::now() + Duration::days(4)).await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks")
        .add_query_param("view", "personal")
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    let blocked = tasks.iter().find(|t| t["uuid"] == blocked_uuid).unwrap();
    let waiting = tasks.iter().find(|t| t["uuid"] == waiting_uuid).unwrap();

    assert_eq!(blocked["blocked"], true);
    assert_eq!(blocked["waiting"], false);
    assert_eq!(waiting["blocked"], false);
    assert_eq!(waiting["waiting"], true);
}

#[tokio::test]
async fn test_list_tasks_reconciles_stale_builtin_view_filters() {
    let env = setup().await;

    let dependency_uuid = create_task(&env, "+test prerequisite").await;
    let blocked_uuid = create_task(&env, "due:tomorrow stale builtin blocked task").await;
    let visible_uuid = create_task(&env, "due:tomorrow stale builtin visible task").await;

    mark_task_blocked(&env, &blocked_uuid, &dependency_uuid).await;

    let mut stale_duesoon = cmdock_server::views::defaults::builtin_view("duesoon").unwrap();
    stale_duesoon.filter = "status:pending due.before:7d".to_string();
    stale_duesoon.template_version = cmdock_server::views::defaults::VIEWSET_VERSION - 1;
    env.store
        .upsert_view(&env.user_id, &stale_duesoon)
        .await
        .unwrap();

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks")
        .add_query_param("view", "duesoon")
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    assert!(
        tasks.iter().any(|t| t["uuid"] == visible_uuid),
        "reconciled duesoon should still include visible due-soon tasks"
    );
    assert!(
        !tasks.iter().any(|t| t["uuid"] == blocked_uuid),
        "task listing should reconcile stale builtin duesoon filter before evaluation"
    );
}

#[tokio::test]
async fn test_list_tasks_normalizes_current_modified_actionable_builtin_filters() {
    let env = setup().await;

    let dependency_uuid = create_task(&env, "+test prerequisite").await;
    let blocked_uuid =
        create_task(&env, "due:tomorrow current-version modified blocked task").await;
    let visible_uuid =
        create_task(&env, "due:tomorrow current-version modified visible task").await;

    mark_task_blocked(&env, &blocked_uuid, &dependency_uuid).await;

    let mut modified_duesoon = cmdock_server::views::defaults::builtin_view("duesoon").unwrap();
    modified_duesoon.filter = "status:pending due.before:7d".to_string();
    modified_duesoon.user_modified = true;
    modified_duesoon.template_version = cmdock_server::views::defaults::VIEWSET_VERSION;
    env.store
        .upsert_view(&env.user_id, &modified_duesoon)
        .await
        .unwrap();

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks")
        .add_query_param("view", "duesoon")
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    assert!(
        tasks.iter().any(|t| t["uuid"] == visible_uuid),
        "normalized modified duesoon should keep visible due-soon tasks"
    );
    assert!(
        !tasks.iter().any(|t| t["uuid"] == blocked_uuid),
        "modified current-version duesoon should still enforce blocked-task exclusion"
    );

    let views = env.store.list_views_all(&env.user_id).await.unwrap();
    let duesoon = views.iter().find(|v| v.id == "duesoon").unwrap();
    assert_eq!(
        duesoon.filter,
        "status:pending due.before:7d -BLOCKED -WAITING"
    );
}

#[tokio::test]
async fn test_modify_dependencies_sets_blocked_and_unblocks_after_completion() {
    let env = setup().await;

    let blocker_uuid = create_task(&env, "project:PERSONAL blocker task").await;
    let dependent_uuid = create_task(&env, "project:PERSONAL dependent task").await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{dependent_uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"depends": [blocker_uuid]}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();
    let tasks: Vec<Value> = resp.json();
    let dependent = tasks.iter().find(|t| t["uuid"] == dependent_uuid).unwrap();
    assert_eq!(dependent["blocked"], true);

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{blocker_uuid}/done"))
        .add_header(h, v)
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();
    let tasks: Vec<Value> = resp.json();
    let dependent = tasks.iter().find(|t| t["uuid"] == dependent_uuid).unwrap();
    assert_eq!(dependent["blocked"], false);
}

#[tokio::test]
async fn test_modify_dependencies_replaces_existing_dependency_set() {
    let env = setup().await;

    let blocker_a = create_task(&env, "project:PERSONAL blocker a").await;
    let blocker_b = create_task(&env, "project:PERSONAL blocker b").await;
    let dependent_uuid = create_task(&env, "project:PERSONAL dependent task").await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{dependent_uuid}/modify"))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"depends": [blocker_a]}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{dependent_uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"depends": [blocker_b]}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{blocker_b}/done"))
        .add_header(h, v)
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();
    let tasks: Vec<Value> = resp.json();
    let dependent = tasks.iter().find(|t| t["uuid"] == dependent_uuid).unwrap();
    assert_eq!(
        dependent["blocked"], false,
        "dependency replacement should remove the older blocker set"
    );
}

#[tokio::test]
async fn test_modify_dependencies_can_clear_all_dependencies() {
    let env = setup().await;

    let blocker_uuid = create_task(&env, "project:PERSONAL blocker task").await;
    let dependent_uuid = create_task(&env, "project:PERSONAL dependent task").await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{dependent_uuid}/modify"))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"depends": [blocker_uuid]}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{dependent_uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"depends": []}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();
    let tasks: Vec<Value> = resp.json();
    let dependent = tasks.iter().find(|t| t["uuid"] == dependent_uuid).unwrap();
    assert_eq!(dependent["blocked"], false);
}

#[tokio::test]
async fn test_modify_dependencies_rejects_invalid_uuid() {
    let env = setup().await;

    let dependent_uuid = create_task(&env, "project:PERSONAL dependent task").await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{dependent_uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"depends": ["not-a-uuid"]}))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_modify_dependencies_rejects_unknown_task_uuid() {
    let env = setup().await;

    let dependent_uuid = create_task(&env, "project:PERSONAL dependent task").await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{dependent_uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"depends": [uuid::Uuid::new_v4().to_string()]}))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_list_tasks_missing_view_returns_404() {
    let env = setup().await;

    // Create at least one task so the replica exists
    create_task(&env, "+test Dummy").await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks")
        .add_query_param("view", "nonexistent")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
}

// --- Modify edge case tests ---

#[tokio::test]
async fn test_modify_deleted_task_returns_409() {
    let env = setup().await;

    let uuid = create_task(&env, "+test Delete then modify").await;

    // Delete the task
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/delete"))
        .add_header(h, v)
        .json(&serde_json::json!({}))
        .await;
    resp.assert_status_ok();

    // Attempt to modify the deleted task — should return 409
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"description": "Should fail"}))
        .await;
    resp.assert_status(axum::http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_modify_tags_replaces() {
    let env = setup().await;

    let uuid = create_task(&env, "+alpha +beta Tag replace test").await;

    // Verify initial tags
    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    let initial_tags: Vec<&str> = task["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        initial_tags.contains(&"alpha"),
        "should have alpha tag initially"
    );
    assert!(
        initial_tags.contains(&"beta"),
        "should have beta tag initially"
    );

    // Modify with new tags — should replace, not merge
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"tags": ["gamma", "delta"]}))
        .await;
    resp.assert_status_ok();

    // Verify tags were replaced
    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    let new_tags: Vec<&str> = task["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        new_tags.contains(&"gamma"),
        "should have gamma tag after replace"
    );
    assert!(
        new_tags.contains(&"delta"),
        "should have delta tag after replace"
    );
    assert!(
        !new_tags.contains(&"alpha"),
        "alpha should be removed after replace"
    );
    assert!(
        !new_tags.contains(&"beta"),
        "beta should be removed after replace"
    );
}

#[tokio::test]
async fn test_modify_with_invalid_due_format() {
    let env = setup().await;

    let uuid = create_task(&env, "+test Invalid due format").await;

    // Modify with an invalid due date string.
    // parse_tw_date returns None for unparseable strings, which clears the due date.
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"due": "not-a-date"}))
        .await;

    // The handler calls parse_tw_date which returns None, then set_due(None)
    // effectively clears the due date. This succeeds with 200.
    resp.assert_status_ok();

    // Verify the task still exists and due is null/absent
    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    assert!(
        task["due"].is_null() || task.get("due").is_none(),
        "due should be null after invalid date format (parse returns None, clearing due)"
    );
}

// --- Regression tests (from iOS integration testing on staging) ---

/// Regression: named due dates ("tomorrow", "friday") were silently dropped
/// because parse_tw_date only handled YYYYMMDDTHHmmssZ format.
/// Fixed by using parse_date_value which supports named dates.
#[tokio::test]
async fn test_add_task_with_named_due_date() {
    let env = setup().await;

    let uuid = create_task(&env, "+test Named due date due:tomorrow").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    assert!(
        task["due"].is_string(),
        "due field should be present for task with due:tomorrow (was: {:?})",
        task["due"]
    );
    // Should be in TW format: YYYYMMDDTHHmmssZ
    let due = task["due"].as_str().unwrap();
    assert!(
        due.ends_with('Z') && due.contains('T'),
        "due should be in TW format (YYYYMMDDTHHmmssZ), got: {due}"
    );
}

/// Regression: ISO date format (2026-04-01) should also work for due dates.
#[tokio::test]
async fn test_add_task_with_iso_due_date() {
    let env = setup().await;

    let uuid = create_task(&env, "+test ISO due date due:2026-12-25").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    assert!(
        task["due"].is_string(),
        "due field should be present for ISO date format"
    );
    let due = task["due"].as_str().unwrap();
    assert!(
        due.starts_with("20261225"),
        "due should start with 20261225, got: {due}"
    );
}

/// Regression: TW format due dates should continue to work.
#[tokio::test]
async fn test_add_task_with_tw_format_due_date() {
    let env = setup().await;

    let uuid = create_task(&env, "+test TW due date due:20261231T120000Z").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    assert_eq!(
        task["due"].as_str().unwrap(),
        "20261231T120000Z",
        "TW format due date should round-trip exactly"
    );
}

/// Regression: empty view parameter (?view=) should return all pending tasks,
/// not 404 with empty body.
#[tokio::test]
async fn test_empty_view_parameter_returns_all_tasks() {
    let env = setup().await;

    create_task(&env, "+test Empty view param task").await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks")
        .add_query_param("view", "")
        .add_header(h, v)
        .await;

    // Should return 200 with tasks, not 404
    resp.assert_status_ok();
    let tasks: Vec<Value> = resp.json();
    assert!(
        !tasks.is_empty(),
        "empty view parameter should return all pending tasks, not 404"
    );
}

/// Regression: delete followed by `/api/tasks?view=` should omit the deleted task,
/// because empty view is intended to fall onto the default pending-task list.
#[tokio::test]
async fn test_empty_view_parameter_omits_deleted_tasks() {
    let env = setup().await;

    let uuid = create_task(&env, "+test Delete then fetch empty view").await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/delete"))
        .add_header(h, v)
        .json(&serde_json::json!({}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks")
        .add_query_param("view", "")
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    assert!(
        tasks.iter().all(|task| task["uuid"] != uuid),
        "deleted task should not appear in /api/tasks?view="
    );
}

/// Regression: modify with named due date should work.
#[tokio::test]
async fn test_modify_task_with_named_due_date() {
    let env = setup().await;

    let uuid = create_task(&env, "+test Modify due date").await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"due": "friday"}))
        .await;
    resp.assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    assert!(
        task["due"].is_string(),
        "due field should be present after modify with named date"
    );
}

// --- Urgency calculation tests ---

/// Urgency for a task with priority H and a project should include both factors.
#[tokio::test]
async fn test_urgency_priority_and_project() {
    let env = setup().await;
    let uuid = create_task(&env, "project:Work priority:H +test Urgent task").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    let urgency = task["urgency"].as_f64().unwrap();

    // Priority H = 6.0, project = 1.0, 1 tag = 0.8, plus small age contribution
    // Should be at least 7.8
    assert!(
        urgency >= 7.5,
        "expected urgency >= 7.5 for H priority + project + tag, got {urgency}"
    );
}

/// Urgency with a near-future due date should be positive (never negative).
#[tokio::test]
async fn test_urgency_due_date_never_negative() {
    let env = setup().await;
    let uuid = create_task(&env, "+test due:30d Far future task").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    let urgency = task["urgency"].as_f64().unwrap();

    // Due 30 days out = 2.4 (floor) + tag(0.8) + small age — must be positive
    assert!(
        urgency > 0.0,
        "urgency should never be negative for a far-future due date, got {urgency}"
    );
}

/// Minimal task (no priority, no project, no tags, no due) should have
/// near-zero urgency (only tiny age contribution from just-created).
#[tokio::test]
async fn test_urgency_minimal_task() {
    let env = setup().await;
    let uuid = create_task(&env, "Bare task").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    let urgency = task["urgency"].as_f64().unwrap();

    // Only age contributes (< 1 second old → near zero)
    assert!(
        urgency < 1.0,
        "bare task urgency should be near zero, got {urgency}"
    );
}

// --- Depends field tests ---

/// Blocked task should expose depends UUIDs of pending dependencies.
#[tokio::test]
async fn test_depends_field_with_pending_dependency() {
    let env = setup().await;

    let dep_uuid = create_task(&env, "+test prerequisite task").await;
    let blocked_uuid = create_task(&env, "+test blocked task").await;
    mark_task_blocked(&env, &blocked_uuid, &dep_uuid).await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let blocked = tasks.iter().find(|t| t["uuid"] == blocked_uuid).unwrap();

    assert_eq!(blocked["blocked"], true);
    let depends = blocked["depends"].as_array().unwrap();
    assert_eq!(depends.len(), 1);
    assert_eq!(depends[0].as_str().unwrap(), dep_uuid);
}

/// After completing the dependency, depends should be empty and blocked false.
#[tokio::test]
async fn test_depends_field_clears_after_completing_dependency() {
    let env = setup().await;

    let dep_uuid = create_task(&env, "+test prerequisite task").await;
    let blocked_uuid = create_task(&env, "+test blocked task").await;
    mark_task_blocked(&env, &blocked_uuid, &dep_uuid).await;

    // Complete the dependency
    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{dep_uuid}/done"))
        .add_header(h, v)
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == blocked_uuid).unwrap();

    assert_eq!(task["blocked"], false);
    let depends = task["depends"].as_array().unwrap();
    assert!(
        depends.is_empty(),
        "depends should be empty after completing dep"
    );
}

/// Task with no dependencies should have empty depends and blocked=false.
#[tokio::test]
async fn test_depends_field_empty_for_independent_task() {
    let env = setup().await;
    let uuid = create_task(&env, "+test independent task").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();

    assert_eq!(task["blocked"], false);
    let depends = task["depends"].as_array().unwrap();
    assert!(depends.is_empty());
}

/// Invariant: blocked == (depends.len > 0) across all tasks in a response.
#[tokio::test]
async fn test_depends_blocked_invariant() {
    let env = setup().await;

    let dep_uuid = create_task(&env, "+test dep").await;
    let _blocked_uuid = create_task(&env, "+test blocked").await;
    mark_task_blocked(&env, &_blocked_uuid, &dep_uuid).await;
    let _free_uuid = create_task(&env, "+test free").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();

    for task in &tasks {
        let blocked = task["blocked"].as_bool().unwrap();
        let depends_len = task["depends"].as_array().unwrap().len();
        assert_eq!(
            blocked,
            depends_len > 0,
            "invariant violated for task {}: blocked={blocked}, depends.len={depends_len}",
            task["uuid"]
        );
    }
}

// --- UDA pass-through tests ---

async fn set_uda(env: &TestEnv, task_uuid: &str, key: &str, value: &str) {
    let mut replica = open_replica(env).await;
    let mut ops = Operations::new();
    let mut task = replica
        .get_task(task_uuid.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    task.set_value(key, Some(value.to_string()), &mut ops)
        .unwrap();
    replica.commit_operations(ops).await.unwrap();
}

async fn read_uda(env: &TestEnv, task_uuid: &str, key: &str) -> Option<String> {
    let mut replica = open_replica(env).await;
    let task = replica
        .get_task(task_uuid.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    task.get_value(key).map(|s| s.to_string())
}

/// Task with UDAs should expose them as top-level keys in the response.
#[tokio::test]
async fn test_uda_fields_appear_at_top_level() {
    let env = setup().await;
    let uuid = create_task(&env, "+test UDA task").await;

    set_uda(&env, &uuid, "estimate", "large").await;
    set_uda(&env, &uuid, "energy", "medium").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();

    assert_eq!(task["estimate"], "large");
    assert_eq!(task["energy"], "medium");
}

/// Task without UDAs should not have extra keys beyond the known schema.
#[tokio::test]
async fn test_no_uda_no_extra_keys() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Plain task").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    let obj = task.as_object().unwrap();

    let known_keys = [
        "uuid",
        "description",
        "project",
        "tags",
        "priority",
        "due",
        "urgency",
        "depends",
        "blocked",
        "waiting",
        "status",
        // server#130/#141: user-facing canonical key (`<PREFIX>-<n>`) and
        // Task Scope prefix projection. Sourced from the allocation table,
        // NOT spoofable TC UDAs.
        "key",
        "cmdock_task_scope",
    ];
    for key in obj.keys() {
        assert!(
            known_keys.contains(&key.as_str()),
            "unexpected key '{key}' in TaskItem without UDAs"
        );
    }
}

/// Internal task-key UDAs must not flatten from TC onto the REST TaskItem shape.
/// `key` and canonical `cmdock_task_scope` are projected from the allocation table,
/// not from spoofable TC UDA values. `cmdock_account` is suppressed entirely.
#[tokio::test]
async fn test_task_key_scope_udas_projected_from_allocation_not_spoofed_uda() {
    let env = setup().await;
    let uuid = create_task(&env, "+test hidden task-key UDAs").await;

    set_uda(&env, &uuid, "cmdock_key", "BOGUS-99").await;
    set_uda(&env, &uuid, "cmdock_account", "BOGUS").await;
    set_uda(&env, &uuid, "cmdock_task_scope", "BOGUS").await;
    set_uda(&env, &uuid, "estimate", "small").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    let obj = task.as_object().unwrap();

    let prefix = task["key"].as_str().unwrap().rsplit_once('-').unwrap().0;
    assert_eq!(task["estimate"], "small");
    assert!(!obj.contains_key("cmdock_key"));
    assert!(
        !obj.contains_key("cmdock_account"),
        "deprecated cmdock_account must not appear in TaskItem"
    );
    assert_eq!(task["cmdock_task_scope"], prefix);
}

/// REST raw syntax accepts canonical `cmdock_task_scope` assertions and
/// tolerates (ignores) deprecated `cmdock_account` values without error.
#[tokio::test]
async fn test_task_scope_udas_write_validation_via_rest_raw() {
    let env = setup().await;
    let uuid = create_task(&env, "+test scope assertion baseline").await;
    let prefix = read_uda(&env, &uuid, "cmdock_task_scope").await.unwrap();

    let (h, v) = auth_header(&env.token);
    let ok = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": format!("cmdock_task_scope:{prefix} +test accepted scope assertion")}))
        .await;
    ok.assert_status_ok();

    // cmdock_account in raw is tolerated-and-ignored even when it doesn't match;
    // only cmdock_task_scope drives the INVALID_TASK_SCOPE check.
    let tolerated = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": format!("cmdock_task_scope:{prefix} cmdock_account:BOGUS +test tolerated account")}))
        .await;
    tolerated.assert_status_ok();
}

/// Regression: project and scheduled must NOT leak into UDA extras.
/// TC considers these user-defined (not in its Prop enum), but we consume
/// them as explicit TaskItem fields.
#[tokio::test]
async fn test_uda_excludes_explicit_non_prop_keys() {
    let env = setup().await;
    let uuid = create_task(&env, "project:WORK +test Task with project and UDA").await;
    set_uda(&env, &uuid, "estimate", "small").await;
    set_uda(&env, &uuid, "scheduled", "1750000000").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    let obj = task.as_object().unwrap();

    // project appears exactly once (as the explicit field, not duplicated in extras)
    assert_eq!(task["project"], "WORK");
    let project_count = obj.keys().filter(|k| *k == "project").count();
    assert_eq!(project_count, 1, "project key should appear exactly once");

    // estimate UDA appears as expected
    assert_eq!(task["estimate"], "small");

    // scheduled should not appear as a top-level UDA key
    assert!(
        !obj.contains_key("scheduled"),
        "scheduled should not leak into UDA extras"
    );
}

/// Unknown key:value tokens in raw syntax stay in description (not parsed as UDAs).
/// UDAs are set via direct TC writes, not the raw parser.
#[tokio::test]
async fn test_unknown_key_value_stays_in_description() {
    let env = setup().await;
    let uuid = create_task(&env, "+test estimate:large energy:high Ship the feature").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();

    // Unknown key:value preserved in description, not parsed as UDAs
    let desc = task["description"].as_str().unwrap();
    assert!(
        desc.contains("estimate:large"),
        "estimate:large should be in description"
    );
    assert!(
        desc.contains("energy:high"),
        "energy:high should be in description"
    );
}

/// URLs and times in raw syntax should remain in description, not become UDAs.
#[tokio::test]
async fn test_urls_and_times_stay_in_description() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Review https://example.com at 12:30").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();

    assert_eq!(task["description"], "Review https://example.com at 12:30");
    // No phantom UDA keys
    let obj = task.as_object().unwrap();
    assert!(!obj.contains_key("https"));
    assert!(!obj.contains_key("12"));
}

/// TC internal keys in raw syntax should be treated as description, not UDAs.
#[tokio::test]
async fn test_reserved_keys_rejected_as_udas() {
    let env = setup().await;
    let uuid = create_task(&env, "+test status:deleted should stay in description").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();

    assert_eq!(task["status"], "pending", "status should not be overridden");
    assert!(
        task["description"]
            .as_str()
            .unwrap()
            .contains("status:deleted"),
        "reserved key:value should be in description"
    );
}

// --- Determinism + UUID-ascending order (server#102) ---
//
// Contract requirement (default-views-contract § Sort ownership): identical
// requests over identical data must produce identically-ordered task arrays,
// with tasks in UUID-ascending lexicographic order on the canonical-string
// `uuid` field.
//
// These tests use *controlled* UUIDs (set via TaskChampion's create_task)
// inserted in deliberately non-sorted order, so a regression that surfaced
// insertion order or any shuffled ordering would fail deterministically — not
// probabilistically, as random UUIDs would.

/// Seed a task with an explicit UUID and description via the underlying
/// TaskChampion replica. Bypasses the REST add_task path so we control
/// the full UUID string used by the wire response.
async fn seed_task_with_uuid(env: &TestEnv, uuid: &str, description: &str) {
    let mut replica = open_replica(env).await;
    let mut ops = Operations::new();
    let mut task = replica
        .create_task(uuid.parse().unwrap(), &mut ops)
        .await
        .unwrap();
    task.set_description(description.to_string(), &mut ops)
        .unwrap();
    task.set_status(taskchampion::Status::Pending, &mut ops)
        .unwrap();
    replica.commit_operations(ops).await.unwrap();
}

/// Five UUIDs in deliberately non-sorted insertion order. Their sorted
/// (canonical-lowercase ascending) order is `00…01, 33…33, 66…66, 99…99,
/// cc…cc`. We insert them as `cc, 00, 99, 33, 66` so any shuffle/insertion
/// order regression produces a detectably-wrong sequence.
const ORDERING_FIXTURE: &[(&str, &str)] = &[
    ("cccccccc-cccc-4ccc-8ccc-cccccccccccc", "inserted-1"),
    ("00000000-0000-4000-8000-000000000001", "inserted-2"),
    ("99999999-9999-4999-8999-999999999999", "inserted-3"),
    ("33333333-3333-4333-8333-333333333333", "inserted-4"),
    ("66666666-6666-4666-8666-666666666666", "inserted-5"),
];

const ORDERING_FIXTURE_SORTED: &[&str] = &[
    "00000000-0000-4000-8000-000000000001",
    "33333333-3333-4333-8333-333333333333",
    "66666666-6666-4666-8666-666666666666",
    "99999999-9999-4999-8999-999999999999",
    "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
];

async fn seed_ordering_fixture(env: &TestEnv) {
    for (uuid, desc) in ORDERING_FIXTURE {
        seed_task_with_uuid(env, uuid, desc).await;
    }
}

#[tokio::test]
async fn test_list_tasks_pending_path_is_uuid_ascending() {
    let env = setup().await;
    seed_ordering_fixture(&env).await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    assert_eq!(tasks.len(), ORDERING_FIXTURE.len());
    let uuids: Vec<&str> = tasks.iter().map(|t| t["uuid"].as_str().unwrap()).collect();
    assert_eq!(
        uuids, ORDERING_FIXTURE_SORTED,
        "tasks must be in UUID-ascending order on the no-filter path"
    );
}

#[tokio::test]
async fn test_list_tasks_filtered_path_is_uuid_ascending() {
    let env = setup().await;
    seed_ordering_fixture(&env).await;

    // The `personal` builtin filter is the bare `status:pending` filter and
    // matches all five seeded pending tasks. This exercises the filtered path
    // (HashMap → Vec) which is the path that surfaced non-determinism.
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks?view=personal")
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let tasks: Vec<Value> = resp.json();
    assert_eq!(tasks.len(), ORDERING_FIXTURE.len());
    let uuids: Vec<&str> = tasks.iter().map(|t| t["uuid"].as_str().unwrap()).collect();
    assert_eq!(
        uuids, ORDERING_FIXTURE_SORTED,
        "tasks must be in UUID-ascending order on the filtered-view path"
    );
}

#[tokio::test]
async fn test_list_tasks_response_is_byte_identical_across_calls() {
    let env = setup().await;
    seed_ordering_fixture(&env).await;

    // Three sequential calls. Compare raw response text — no JSON normalisation —
    // so any field-order shift in the array would also fail this assertion.
    // (Note: tasks here have no UDAs; UDA-flatten ordering is a separate concern
    // tracked outside #102.)
    let mut bodies: Vec<String> = Vec::with_capacity(3);
    for _ in 0..3 {
        let (h, v) = auth_header(&env.token);
        let resp = env
            .server
            .get("/api/tasks?view=personal")
            .add_header(h, v)
            .await;
        resp.assert_status_ok();
        bodies.push(resp.text());
    }
    assert_eq!(
        bodies[0], bodies[1],
        "calls 1 and 2 produced different bodies (filter path is non-deterministic)"
    );
    assert_eq!(
        bodies[1], bodies[2],
        "calls 2 and 3 produced different bodies (filter path is non-deterministic)"
    );
}

// --- Task-write contract: wait / scheduled / strict-recognise (server#100) ---
//
// Contract: cmdock/architecture docs/task-write-contract.md
// ADR: cmdock/architecture docs/adr/ADR-0011-write-surface-evolution-policy.md

/// Read a task's wait timestamp directly from the underlying TC replica.
/// `TaskItem` exposes `waiting: bool` (computed) but not the wait date string;
/// tests assert against the replica to verify the date was set/cleared correctly.
async fn read_task_wait(env: &TestEnv, uuid: &str) -> Option<chrono::DateTime<Utc>> {
    let mut replica = open_replica(env).await;
    let task = replica
        .get_task(uuid.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    task.get_wait()
}

/// Read a task's scheduled timestamp directly from the underlying TC replica.
/// TC stores `scheduled` as a generic property holding epoch seconds.
async fn read_task_scheduled(env: &TestEnv, uuid: &str) -> Option<chrono::DateTime<Utc>> {
    let mut replica = open_replica(env).await;
    let task = replica
        .get_task(uuid.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    task.get_value("scheduled")
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
}

#[tokio::test]
async fn test_add_task_recognises_wait_and_scheduled_in_raw() {
    let env = setup().await;

    let uuid = create_task(&env, "project:WORK wait:7d scheduled:14d Review proposal").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();

    // Description has only the bare-word remainder; project + dates were extracted.
    assert_eq!(task["description"], "Review proposal");
    assert_eq!(task["project"], "WORK");
    // Wait is 7 days in the future, so the task is waiting (not in default pending list, but visible via /api/tasks no-filter).
    assert_eq!(task["waiting"], true);

    // Verify replica has both timestamps set.
    let wait = read_task_wait(&env, &uuid)
        .await
        .expect("wait should be set");
    let now = Utc::now();
    let expected_wait_low = now + Duration::days(6);
    let expected_wait_high = now + Duration::days(8);
    assert!(
        wait >= expected_wait_low && wait <= expected_wait_high,
        "wait should be ~7d in the future, got {wait}"
    );
    let scheduled = read_task_scheduled(&env, &uuid)
        .await
        .expect("scheduled should be set");
    let expected_sched_low = now + Duration::days(13);
    let expected_sched_high = now + Duration::days(15);
    assert!(
        scheduled >= expected_sched_low && scheduled <= expected_sched_high,
        "scheduled should be ~14d in the future, got {scheduled}"
    );
}

#[tokio::test]
async fn test_add_task_unknown_top_level_field_rejected() {
    let env = setup().await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({
            "raw": "Buy milk",
            "extra_field": "should be rejected"
        }))
        .await;
    assert_eq!(resp.status_code(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text(), "INVALID_FIELD");
}

#[tokio::test]
async fn test_modify_wait_canonical_set() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Set wait via modify").await;

    let canonical = "20260601T090000Z";
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"wait": canonical}))
        .await;
    resp.assert_status_ok();

    let wait = read_task_wait(&env, &uuid)
        .await
        .expect("wait should be set after modify");
    let expected = chrono::NaiveDateTime::parse_from_str(canonical, "%Y%m%dT%H%M%SZ")
        .unwrap()
        .and_utc();
    assert_eq!(wait, expected);
}

#[tokio::test]
async fn test_modify_wait_explicit_null_clears() {
    let env = setup().await;
    let uuid = create_task(&env, "wait:7d Test wait clearing").await;

    // Sanity: wait is set after create
    assert!(read_task_wait(&env, &uuid).await.is_some());

    // Explicit null clears
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"wait": null}))
        .await;
    resp.assert_status_ok();

    assert!(
        read_task_wait(&env, &uuid).await.is_none(),
        "wait should be cleared after explicit null on modify"
    );
}

#[tokio::test]
async fn test_modify_wait_omitted_leaves_unchanged() {
    let env = setup().await;
    let uuid = create_task(&env, "wait:7d Test wait omission").await;
    let original_wait = read_task_wait(&env, &uuid)
        .await
        .expect("wait set after create");

    // Modify another field, leave wait omitted
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"priority": "H"}))
        .await;
    resp.assert_status_ok();

    let after_wait = read_task_wait(&env, &uuid)
        .await
        .expect("wait should still be set");
    assert_eq!(
        after_wait, original_wait,
        "wait must be unchanged when omitted from modify body"
    );
}

#[tokio::test]
async fn test_modify_wait_non_canonical_rejected_with_invalid_date() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Reject non-canonical wait").await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"wait": "tomorrow"}))
        .await;
    assert_eq!(resp.status_code(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text(), "INVALID_DATE");
}

#[tokio::test]
async fn test_modify_scheduled_canonical_set_and_null_clears() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Test scheduled lifecycle").await;

    let canonical = "20260615T123000Z";
    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"scheduled": canonical}))
        .await
        .assert_status_ok();
    let expected = chrono::NaiveDateTime::parse_from_str(canonical, "%Y%m%dT%H%M%SZ")
        .unwrap()
        .and_utc();
    assert_eq!(read_task_scheduled(&env, &uuid).await.unwrap(), expected);

    // Explicit null clears
    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"scheduled": null}))
        .await
        .assert_status_ok();
    assert!(read_task_scheduled(&env, &uuid).await.is_none());
}

#[tokio::test]
async fn test_modify_scheduled_non_canonical_rejected() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Reject non-canonical scheduled").await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"scheduled": "next week"}))
        .await;
    assert_eq!(resp.status_code(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text(), "INVALID_DATE");
}

#[tokio::test]
async fn test_modify_unknown_top_level_field_rejected() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Reject unknown field on modify").await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({
            "priority": "H",
            "garbage_field": "not allowed"
        }))
        .await;
    assert_eq!(resp.status_code(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text(), "INVALID_FIELD");
}

#[tokio::test]
async fn test_modify_due_broad_parser_preserved() {
    // Per task-write-contract.md § Date format on modify, `due` continues
    // to accept the broad date parser (named dates, ISO, relative durations)
    // on set, while `wait` / `scheduled` are canonical-only. This is an
    // intentional asymmetry preserved per ADR-0011's retrofit rule on #100,
    // not a pending retrofit. (#105 closed the separate null-clears retrofit
    // for project/priority/due; broad-parser-on-due is independent.)
    let env = setup().await;
    let uuid = create_task(&env, "+test due broad parser").await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"due": "tomorrow"}))
        .await;
    resp.assert_status_ok();

    // The broad parser should have set a due date (tomorrow ≈ now + 1 day).
    let (h, v) = auth_header(&env.token);
    let list_resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = list_resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    assert!(
        task["due"].is_string(),
        "due should be set via broad parser (named date 'tomorrow')"
    );
}

#[tokio::test]
async fn test_modify_scheduled_omitted_leaves_unchanged() {
    let env = setup().await;
    let uuid = create_task(&env, "scheduled:14d Test scheduled omission").await;
    let original = read_task_scheduled(&env, &uuid)
        .await
        .expect("scheduled set after create");

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"priority": "L"}))
        .await
        .assert_status_ok();

    let after = read_task_scheduled(&env, &uuid)
        .await
        .expect("scheduled should still be set");
    assert_eq!(
        after, original,
        "scheduled must be unchanged when omitted from modify body"
    );
}

#[tokio::test]
async fn test_modify_wait_canonical_edge_cases_rejected() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Canonical edge cases").await;

    // Empty, month 13, hour 25, ISO date — all should fail canonical
    // validation via chrono's strict format parser.
    for bad in ["", "20261301T000000Z", "20260601T250000Z", "2026-06-01"] {
        let (h, v) = auth_header(&env.token);
        let resp = env
            .server
            .post(&format!("/api/tasks/{uuid}/modify"))
            .add_header(h, v)
            .json(&serde_json::json!({"wait": bad}))
            .await;
        assert_eq!(
            resp.status_code(),
            axum::http::StatusCode::BAD_REQUEST,
            "wait={bad:?} should be rejected"
        );
        assert_eq!(resp.text(), "INVALID_DATE", "wait={bad:?} body code");
    }
}

#[tokio::test]
async fn test_add_task_recognised_date_attr_unparseable_rejected() {
    // Per task-write-contract.md § Errors (primary): recognised raw-syntax
    // date attributes that fail to parse return 400 INVALID_DATE. Silent
    // drop would create a task with the date missing — contrary to the
    // strict-recognise principle for newly-added attributes.
    //
    // Only the new attributes (wait, scheduled) are validated this strictly;
    // existing `due:` continues to silently drop on bad parse (retrofit #105).
    let env = setup().await;

    for bad_raw in [
        "wait:not-a-date Buy milk",
        "scheduled:20261301T000000Z Plan retro",
    ] {
        let (h, v) = auth_header(&env.token);
        let resp = env
            .server
            .post("/api/tasks")
            .add_header(h, v)
            .json(&serde_json::json!({"raw": bad_raw}))
            .await;
        assert_eq!(
            resp.status_code(),
            axum::http::StatusCode::BAD_REQUEST,
            "raw={bad_raw:?} should be rejected"
        );
        assert_eq!(resp.text(), "INVALID_DATE", "raw={bad_raw:?} body code");
    }
}

#[tokio::test]
async fn test_add_task_missing_raw_field_returns_invalid_raw() {
    // Contract § Errors (primary): missing/empty/control-chars `raw` → INVALID_RAW.
    let env = setup().await;

    // Missing raw entirely (serde rejection)
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(resp.status_code(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text(), "INVALID_RAW");

    // Empty after trim (garde rejection — single field on AddTaskRequest
    // means any garde failure maps to INVALID_RAW).
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "   "}))
        .await;
    assert_eq!(resp.status_code(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text(), "INVALID_RAW");
}

#[tokio::test]
async fn test_unknown_token_in_raw_falls_through_to_description() {
    // Lenient-drop deviation per contract § Lenient-drop deviation for parse_raw:
    // unrecognised name:value tokens become part of the description.
    let env = setup().await;
    let uuid = create_task(&env, "recur:weekly until:eom Plan retro").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = resp.json();
    let task = tasks.iter().find(|t| t["uuid"] == uuid).unwrap();
    assert_eq!(
        task["description"], "recur:weekly until:eom Plan retro",
        "unrecognised name:value tokens stay in description"
    );
}

// --- Null-clears retrofit (server#105) ---
//
// Contract: task-write-contract.md § Clear semantics — general rule:
// for `project`, `priority`, `due`, `wait`, `scheduled` — explicit JSON
// `null` clears the field; omission leaves unchanged. Empty array semantics
// continue to apply to `tags` / `depends`. `description` is not clearable.
//
// `wait` / `scheduled` were retrofitted in #100; this set covers the
// pre-existing fields per #105.

#[tokio::test]
async fn test_modify_priority_explicit_null_clears() {
    let env = setup().await;
    let uuid = create_task(&env, "priority:H +test Clear priority via null").await;

    // Sanity: priority is set
    let (h, v) = auth_header(&env.token);
    let task: Value = env
        .server
        .get("/api/tasks")
        .add_header(h, v)
        .await
        .json::<Vec<Value>>()
        .into_iter()
        .find(|t| t["uuid"] == uuid)
        .unwrap();
    assert_eq!(task["priority"], "H");

    // Explicit null clears
    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"priority": null}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let task: Value = env
        .server
        .get("/api/tasks")
        .add_header(h, v)
        .await
        .json::<Vec<Value>>()
        .into_iter()
        .find(|t| t["uuid"] == uuid)
        .unwrap();
    assert!(
        task.get("priority").is_none() || task["priority"].is_null(),
        "priority should be cleared after explicit null modify, got {:?}",
        task.get("priority")
    );
}

#[tokio::test]
async fn test_modify_priority_omitted_leaves_unchanged() {
    let env = setup().await;
    let uuid = create_task(&env, "priority:M +test Priority omission").await;

    // Modify another field; priority omitted
    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"description": "Updated description"}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let task: Value = env
        .server
        .get("/api/tasks")
        .add_header(h, v)
        .await
        .json::<Vec<Value>>()
        .into_iter()
        .find(|t| t["uuid"] == uuid)
        .unwrap();
    assert_eq!(
        task["priority"], "M",
        "priority must be unchanged when omitted from modify body"
    );
    assert_eq!(task["description"], "Updated description");
}

#[tokio::test]
async fn test_modify_project_explicit_null_clears() {
    let env = setup().await;
    let uuid = create_task(&env, "project:WORK +test Clear project via null").await;

    let (h, v) = auth_header(&env.token);
    let task: Value = env
        .server
        .get("/api/tasks")
        .add_header(h, v)
        .await
        .json::<Vec<Value>>()
        .into_iter()
        .find(|t| t["uuid"] == uuid)
        .unwrap();
    assert_eq!(task["project"], "WORK");

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"project": null}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let task: Value = env
        .server
        .get("/api/tasks")
        .add_header(h, v)
        .await
        .json::<Vec<Value>>()
        .into_iter()
        .find(|t| t["uuid"] == uuid)
        .unwrap();
    assert!(
        task.get("project").is_none() || task["project"].is_null(),
        "project should be cleared after explicit null modify"
    );
}

#[tokio::test]
async fn test_modify_project_omitted_leaves_unchanged() {
    let env = setup().await;
    let uuid = create_task(&env, "project:WORK.ops +test Project omission").await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"priority": "L"}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let task: Value = env
        .server
        .get("/api/tasks")
        .add_header(h, v)
        .await
        .json::<Vec<Value>>()
        .into_iter()
        .find(|t| t["uuid"] == uuid)
        .unwrap();
    assert_eq!(
        task["project"], "WORK.ops",
        "project must be unchanged when omitted"
    );
    assert_eq!(task["priority"], "L");
}

#[tokio::test]
async fn test_modify_due_explicit_null_clears() {
    let env = setup().await;
    let uuid = create_task(&env, "due:tomorrow +test Clear due via null").await;

    let (h, v) = auth_header(&env.token);
    let task: Value = env
        .server
        .get("/api/tasks")
        .add_header(h, v)
        .await
        .json::<Vec<Value>>()
        .into_iter()
        .find(|t| t["uuid"] == uuid)
        .unwrap();
    assert!(
        task["due"].is_string(),
        "due should be set after parse_raw 'tomorrow'"
    );

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"due": null}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let task: Value = env
        .server
        .get("/api/tasks")
        .add_header(h, v)
        .await
        .json::<Vec<Value>>()
        .into_iter()
        .find(|t| t["uuid"] == uuid)
        .unwrap();
    assert!(
        task.get("due").is_none() || task["due"].is_null(),
        "due should be cleared after explicit null modify"
    );
}

#[tokio::test]
async fn test_modify_due_omitted_leaves_unchanged() {
    let env = setup().await;
    let uuid = create_task(&env, "due:friday +test Due omission").await;
    let (h, v) = auth_header(&env.token);
    let original: Value = env
        .server
        .get("/api/tasks")
        .add_header(h, v)
        .await
        .json::<Vec<Value>>()
        .into_iter()
        .find(|t| t["uuid"] == uuid)
        .unwrap();
    let original_due = original["due"].as_str().unwrap().to_string();

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"priority": "H"}))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let task: Value = env
        .server
        .get("/api/tasks")
        .add_header(h, v)
        .await
        .json::<Vec<Value>>()
        .into_iter()
        .find(|t| t["uuid"] == uuid)
        .unwrap();
    assert_eq!(
        task["due"], original_due,
        "due must be unchanged when omitted from modify body"
    );
}

// ===========================================================================
// task-read-contract.md — singleton + batch read endpoints (#109)
// ===========================================================================
//
// Acceptance tests mirror the contract's acceptance list 1:1, plus regression
// tests for the gotchas surfaced in the implementation review:
//   - Existence-leak parity (cross-account 404 byte-identical to unknown 404)
//   - cap-before-dedupe (101 copies of one UUID → TOO_MANY_UUIDS)
//   - request-order preservation in `found`
//   - dedupe preserves first-occurrence position
//   - no-params GET /api/tasks behaviour preserved (regression)

/// A second user sharing the same `TestEnv.server` — used to verify the
/// existence-leak rule on `GET /api/tasks/{uuid}`. Returns the second
/// user's id, token, and a UUID belonging to that user.
struct SecondUser {
    id: String,
    token: String,
    task_uuid: String,
    task_key: String,
}

async fn add_second_user_with_task(env: &TestEnv) -> SecondUser {
    let user2 = env
        .store
        .create_user(&NewUser {
            username: "tasks_user_2".to_string(),
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
    std::fs::create_dir_all(env.data_dir.join("users").join(&user2.id)).unwrap();

    // Create one task on user2's replica via the REST API.
    let (h, v) = auth_header(&token2);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "Cross-account task"}))
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let output = body["output"].as_str().unwrap();
    let task_uuid = output
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap()
        .to_string();
    let task_key = body["key"]
        .as_str()
        .expect("Phase 2 wired `key` into TaskActionResponse")
        .to_string();

    SecondUser {
        id: user2.id,
        token: token2,
        task_uuid,
        task_key,
    }
}

// --- Singleton GET /api/tasks/{uuid} ---

#[tokio::test]
async fn test_get_task_by_id_round_trip() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Single task").await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks/{uuid}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    assert_eq!(body["uuid"].as_str().unwrap(), uuid);
    assert_eq!(body["status"].as_str().unwrap(), "pending");
}

#[tokio::test]
async fn test_get_task_by_id_returns_completed_task() {
    // task-read-contract.md § Visibility rule: returns tasks at any status.
    let env = setup().await;
    let uuid = create_task(&env, "+test Will be completed").await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/done"))
        .add_header(h, v)
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks/{uuid}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["status"].as_str().unwrap(), "completed");
}

#[tokio::test]
async fn test_get_task_by_id_returns_deleted_task() {
    // task-read-contract.md § Visibility rule: returns tasks at any status
    // including soft-deleted.
    let env = setup().await;
    let uuid = create_task(&env, "+test Will be deleted").await;

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/delete"))
        .add_header(h, v)
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks/{uuid}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["status"].as_str().unwrap(), "deleted");
}

#[tokio::test]
async fn test_get_task_by_id_uppercase_uuid_returns_400() {
    // task-read-contract.md § Path parameter pins canonical lowercase
    // hyphenated form. Uppercase hex must NOT be accepted even though
    // Uuid::parse_str would happily parse it.
    let env = setup().await;
    let uuid = create_task(&env, "+test Sentinel").await;
    let upper = uuid.to_uppercase();

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks/{upper}"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_UUID"
    );
}

#[tokio::test]
async fn test_get_task_by_id_simple_form_uuid_returns_400() {
    // 32-char no-hyphen "simple" form — accepted by Uuid::parse_str but
    // not canonical per the contract.
    let env = setup().await;
    let uuid = create_task(&env, "+test Sentinel").await;
    let simple: String = uuid.chars().filter(|c| *c != '-').collect();
    assert_eq!(simple.len(), 32);

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks/{simple}"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_UUID"
    );
}

#[tokio::test]
async fn test_get_task_by_id_braced_uuid_returns_400() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Sentinel").await;
    let braced = format!("{{{uuid}}}");
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks/{braced}"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_UUID"
    );
}

#[tokio::test]
async fn test_batch_lookup_uppercase_uuid_returns_invalid_uuid() {
    // Same canonical-form rule on the batch path.
    let env = setup().await;
    let uuid = create_task(&env, "+test Sentinel").await;
    let upper = uuid.to_uppercase();

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?uuids={upper}"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_UUID"
    );
}

#[tokio::test]
async fn test_batch_lookup_returns_deleted_task() {
    // Batch visibility includes deleted, mirroring singleton § Visibility rule.
    let env = setup().await;
    let uuid = create_task(&env, "+test Will be deleted").await;
    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/delete"))
        .add_header(h, v)
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?uuids={uuid}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let found = body["found"].as_array().unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["status"].as_str().unwrap(), "deleted");
}

#[tokio::test]
async fn test_get_task_by_id_unknown_uuid_returns_404_empty_body() {
    let env = setup().await;
    create_task(&env, "+test Some task").await;

    let unknown = uuid::Uuid::new_v4();
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks/{unknown}"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
    assert!(
        resp.as_bytes().is_empty(),
        "404 must have empty body per task-read-contract.md § Wire-body convention; got {:?}",
        resp.as_bytes()
    );
}

#[tokio::test]
async fn test_get_task_by_id_invalid_uuid_returns_400_invalid_uuid() {
    let env = setup().await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks/not-a-uuid")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_UUID",
        "wire body must be the bare code per § Wire-body convention"
    );
}

#[tokio::test]
async fn test_get_task_by_id_invalid_uuid_short_circuits_before_replica() {
    // Implementation invariant: malformed UUIDs must short-circuit to
    // INVALID_UUID without surfacing replica/quarantine errors. Test by
    // omitting the user's data dir entirely — would normally produce a
    // replica error if we hit open_user_replica before validating.
    let env = setup().await;
    // Use the existing user but pass a non-canonical string. The replica
    // dir exists but parse should fail first.
    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks/abc").add_header(h, v).await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_UUID"
    );
}

#[tokio::test]
async fn test_get_task_by_id_cross_account_returns_404_existence_leak_parity() {
    // Critical: cross-account UUID and unknown UUID must be byte-identical.
    let env = setup().await;
    let user2 = add_second_user_with_task(&env).await;

    // user1 asks for user2's UUID — must be 404 with empty body.
    let (h, v) = auth_header(&env.token);
    let resp_cross = env
        .server
        .get(&format!("/api/tasks/{}", user2.task_uuid))
        .add_header(h, v)
        .await;
    resp_cross.assert_status(axum::http::StatusCode::NOT_FOUND);
    let cross_body = resp_cross.as_bytes().to_vec();

    // user1 asks for an unknown UUID — also 404 with empty body.
    let unknown = uuid::Uuid::new_v4();
    let (h, v) = auth_header(&env.token);
    let resp_unknown = env
        .server
        .get(&format!("/api/tasks/{unknown}"))
        .add_header(h, v)
        .await;
    resp_unknown.assert_status(axum::http::StatusCode::NOT_FOUND);
    let unknown_body = resp_unknown.as_bytes().to_vec();

    assert_eq!(
        cross_body, unknown_body,
        "cross-account 404 body must be byte-identical to unknown-UUID 404 body \
         (existence-leak rule); cross={cross_body:?} unknown={unknown_body:?}"
    );
    assert!(cross_body.is_empty());

    // And user2 can still read their own task — the data is real, not deleted.
    let (h, v) = auth_header(&user2.token);
    env.server
        .get(&format!("/api/tasks/{}", user2.task_uuid))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // Suppress unused-warning on user2.id when this test is the only consumer.
    let _ = user2.id;
}

#[tokio::test]
async fn test_get_task_by_id_no_auth_returns_401() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Auth check").await;

    let resp = env.server.get(&format!("/api/tasks/{uuid}")).await;
    resp.assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

// --- Singleton GET key resolution (#130 Phase 3 C11) ---

/// Helper: create a task and return both UUID and key. The key is harvested
/// from the create-response per Phase 2 wiring.
async fn create_task_with_key(env: &TestEnv, raw: &str) -> (String, String) {
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": raw}))
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let output = body["output"].as_str().unwrap();
    let uuid = output
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap()
        .to_string();
    let key = body["key"]
        .as_str()
        .expect("Phase 2 wired `key` into TaskActionResponse")
        .to_string();
    (uuid, key)
}

#[tokio::test]
async fn test_get_task_by_id_resolves_uuid_and_key_to_identical_response() {
    // GET /api/tasks/<uuid> and GET /api/tasks/<KEY> on the same task
    // must return byte-identical 200 responses.
    let env = setup().await;
    let (uuid, key) = create_task_with_key(&env, "+resolve identical").await;

    let (h1, v1) = auth_header(&env.token);
    let by_uuid = env
        .server
        .get(&format!("/api/tasks/{uuid}"))
        .add_header(h1, v1)
        .await;
    by_uuid.assert_status_ok();
    let by_uuid_bytes = by_uuid.as_bytes().to_vec();

    let (h2, v2) = auth_header(&env.token);
    let by_key = env
        .server
        .get(&format!("/api/tasks/{key}"))
        .add_header(h2, v2)
        .await;
    by_key.assert_status_ok();
    let by_key_bytes = by_key.as_bytes().to_vec();

    assert_eq!(
        by_uuid_bytes, by_key_bytes,
        "UUID-form and key-form responses must be byte-identical for the same task"
    );
}

#[tokio::test]
async fn test_get_task_by_id_resolves_lowercase_key_via_case_fold() {
    // Per the contract, prefix is case-insensitive on input.
    let env = setup().await;
    let (_, key) = create_task_with_key(&env, "+lowercase key resolution").await;
    let lowercase_key = key.to_lowercase();
    assert_ne!(lowercase_key, key, "test fixture: prefix must be uppercase");

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks/{lowercase_key}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let item: Value = resp.json();
    assert_eq!(
        item["key"].as_str().unwrap(),
        key,
        "response carries canonical (uppercase) key regardless of input case"
    );
}

#[tokio::test]
async fn test_get_task_by_id_unknown_key_returns_404_empty_body() {
    // Existence-leak rule: unknown key indistinguishable from cross-account.
    let env = setup().await;
    // Force prefix discovery so we reference a real prefix — query the user's
    // existing-but-unallocated N range.
    let (_, key) = create_task_with_key(&env, "+probe prefix").await;
    let prefix = key.rsplit_once('-').map(|(p, _)| p).expect("key has dash");
    let unknown_key = format!("{prefix}-99999");

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks/{unknown_key}"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
    assert!(
        resp.as_bytes().is_empty(),
        "unknown-key 404 body must be empty per existence-leak rule"
    );
}

#[tokio::test]
async fn test_get_task_by_id_malformed_key_returns_400_invalid_uuid() {
    // Inputs that are neither canonical UUID nor syntactically valid key
    // → 400 INVALID_UUID (plain text).
    let env = setup().await;
    for malformed in &[
        "WORK-0",        // leading-zero N
        "WORK-01",       // leading-zero N
        "1WORK-15",      // first char digit
        "WORK--1",       // negative N
        "WORK-1-2",      // dash in N
        "ABCDEFGHIJK-1", // 11-char prefix
        "not-a-uuid",    // hyphenated but not a UUID and not a valid key
        "WORK_5",        // underscore separator
    ] {
        let (h, v) = auth_header(&env.token);
        let resp = env
            .server
            .get(&format!("/api/tasks/{malformed}"))
            .add_header(h, v)
            .await;
        resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.text(),
            "INVALID_UUID",
            "malformed input {malformed:?} should produce INVALID_UUID"
        );
    }
}

#[tokio::test]
async fn test_get_task_by_id_cross_account_key_returns_404_existence_leak_parity() {
    // Critical regression lock: user-A asking for user-B's KEY must be
    // byte-identical to user-A asking for an unknown KEY. Cross-account
    // resolution must not leak existence via wire shape, status, or body.
    let env = setup().await;
    let user2 = add_second_user_with_task(&env).await;

    // user1 asks for user2's key — must be 404 with empty body.
    let (h, v) = auth_header(&env.token);
    let resp_cross = env
        .server
        .get(&format!("/api/tasks/{}", user2.task_key))
        .add_header(h, v)
        .await;
    resp_cross.assert_status(axum::http::StatusCode::NOT_FOUND);
    let cross_body = resp_cross.as_bytes().to_vec();
    assert!(cross_body.is_empty());
    // Header parity per task-read-contract.md: 404 carries no Content-Type
    // and Content-Length: 0. Pin so future framework upgrades can't
    // silently start adding a default text/plain Content-Type.
    let cross_headers = resp_cross.headers();
    assert!(
        !cross_headers.contains_key(axum::http::header::CONTENT_TYPE),
        "404 must NOT carry Content-Type (existence-leak rule); got {:?}",
        cross_headers.get(axum::http::header::CONTENT_TYPE)
    );
    if let Some(len) = cross_headers.get(axum::http::header::CONTENT_LENGTH) {
        assert_eq!(len.to_str().unwrap(), "0");
    }

    // user1 asks for an unknown key (using a prefix they own, with a
    // never-allocated N) — also 404 with empty body.
    let (own_uuid, own_key) = create_task_with_key(&env, "+own task").await;
    let _ = own_uuid;
    let prefix = own_key.rsplit_once('-').map(|(p, _)| p).unwrap();
    let unknown_key = format!("{prefix}-99999");
    let (h, v) = auth_header(&env.token);
    let resp_unknown = env
        .server
        .get(&format!("/api/tasks/{unknown_key}"))
        .add_header(h, v)
        .await;
    resp_unknown.assert_status(axum::http::StatusCode::NOT_FOUND);
    let unknown_body = resp_unknown.as_bytes().to_vec();
    assert_eq!(
        cross_body, unknown_body,
        "cross-account-key 404 body must be byte-identical to unknown-key 404 body"
    );
    let unknown_headers = resp_unknown.headers();
    assert_eq!(
        unknown_headers.get(axum::http::header::CONTENT_TYPE),
        cross_headers.get(axum::http::header::CONTENT_TYPE),
        "Content-Type parity between unknown-key and cross-account-key 404"
    );

    // And user2 can still resolve their own key — proves the data is real.
    let (h, v) = auth_header(&user2.token);
    env.server
        .get(&format!("/api/tasks/{}", user2.task_key))
        .add_header(h, v)
        .await
        .assert_status_ok();

    let _ = user2.id;
}

// --- Mutation handler key resolution (#130 Phase 3 C12) ---

#[tokio::test]
async fn test_mutation_handlers_accept_key_form_for_modify_done_undo_delete() {
    // Each of `modify`, `done`, `undo`, `delete` accepts the task key
    // form `<PREFIX>-N` and resolves to the same task as the UUID form.
    let env = setup().await;
    let (uuid, key) = create_task_with_key(&env, "+mutate via key").await;

    // modify via key
    let (h, v) = auth_header(&env.token);
    let r = env
        .server
        .post(&format!("/api/tasks/{key}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"description": "Modified by key"}))
        .await;
    r.assert_status_ok();
    // Verify the description landed by reading back via the UUID form.
    let (h, v) = auth_header(&env.token);
    let read = env
        .server
        .get(&format!("/api/tasks/{uuid}"))
        .add_header(h, v)
        .await;
    read.assert_status_ok();
    let item: Value = read.json();
    assert_eq!(item["description"], "Modified by key");

    // complete via key
    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{key}/done"))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // undo via key
    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{key}/undo"))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // delete via key
    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{key}/delete"))
        .add_header(h, v)
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn test_mutation_handlers_preserve_permissive_uuid_uppercase() {
    // Decisions Locked In iter3: mutation handlers preserve their
    // existing permissive `Uuid::parse_str` acceptance — uppercase UUIDs
    // continue to work. (Tightening to canonical-only is a gated
    // follow-up requiring contract sign-off + iOS audit.)
    let env = setup().await;
    let (uuid, _) = create_task_with_key(&env, "+permissive uppercase").await;
    let upper = uuid.to_uppercase();
    assert_ne!(upper, uuid, "test fixture: UUIDs are lowercase");

    let (h, v) = auth_header(&env.token);
    let r = env
        .server
        .post(&format!("/api/tasks/{upper}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"description": "via uppercase uuid"}))
        .await;
    r.assert_status_ok();

    let (h, v) = auth_header(&env.token);
    let read = env
        .server
        .get(&format!("/api/tasks/{uuid}"))
        .add_header(h, v)
        .await;
    let item: Value = read.json();
    assert_eq!(item["description"], "via uppercase uuid");
}

#[tokio::test]
async fn test_mutation_handlers_unknown_key_returns_404() {
    // Existence-leak rule: unknown key on a mutation endpoint maps to
    // 404 (same as unknown UUID). Body shape is the legacy mutation 404
    // (empty) — preserved for iOS compat.
    let env = setup().await;
    let (_, key) = create_task_with_key(&env, "+probe").await;
    let prefix = key.rsplit_once('-').map(|(p, _)| p).unwrap();
    let unknown_key = format!("{prefix}-99999");

    for endpoint in &["modify", "done", "undo", "delete"] {
        let (h, v) = auth_header(&env.token);
        let url = format!("/api/tasks/{unknown_key}/{endpoint}");
        let resp = if *endpoint == "modify" {
            env.server
                .post(&url)
                .add_header(h, v)
                .json(&serde_json::json!({"description": "noop"}))
                .await
        } else {
            env.server.post(&url).add_header(h, v).await
        };
        resp.assert_status(axum::http::StatusCode::NOT_FOUND);
        assert!(
            resp.as_bytes().is_empty(),
            "{endpoint} 404 body must be empty (legacy iOS mutation contract); got {:?}",
            resp.as_bytes()
        );
    }
}

#[tokio::test]
async fn test_mutation_handlers_malformed_path_returns_400() {
    // Inputs that are neither a permissive UUID nor a syntactically valid
    // key → 400 BAD_REQUEST (legacy mutation 400 has empty body —
    // preserved for iOS compat; tightening to a plain-text body is a
    // separate gated change).
    let env = setup().await;
    create_task(&env, "+anchor").await;

    for malformed in &["WORK-0", "WORK-01", "WORK--1", "ABCDEFGHIJK-1", "WORK_5"] {
        for endpoint in &["modify", "done", "undo", "delete"] {
            let (h, v) = auth_header(&env.token);
            let url = format!("/api/tasks/{malformed}/{endpoint}");
            let resp = if *endpoint == "modify" {
                env.server
                    .post(&url)
                    .add_header(h, v)
                    .json(&serde_json::json!({"description": "noop"}))
                    .await
            } else {
                env.server.post(&url).add_header(h, v).await
            };
            resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
            assert!(
                resp.as_bytes().is_empty(),
                "{endpoint} 400 body must be empty for malformed path \
                 (legacy iOS mutation contract — `INVALID_UUID` plain-text \
                 is reserved for the read singleton); got {:?}",
                resp.as_bytes()
            );
        }
    }
}

#[tokio::test]
async fn test_modify_idempotency_aliases_uuid_and_key_form_under_same_key() {
    // Regression lock: Idempotency-Key dedup tuple uses the resolved
    // canonical UUID in `request_path`, so a client that issues
    // `POST /api/tasks/<uuid>/modify` and a follow-up
    // `POST /api/tasks/<KEY>/modify` with the SAME Idempotency-Key and
    // SAME body must replay the original response (not 409 conflict).
    use axum::http::HeaderName;
    let env = setup().await;
    let (uuid, key) = create_task_with_key(&env, "+idempotency alias").await;

    let idem_key = "alias-test-11111111-1111-1111-1111-111111111111";
    let idem_header: HeaderName = HeaderName::from_static("idempotency-key");
    let body = serde_json::json!({"description": "Modified once"});

    // First request — UUID form.
    let (h, v) = auth_header(&env.token);
    let r1 = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .add_header(
            idem_header.clone(),
            HeaderValue::from_static("alias-test-11111111-1111-1111-1111-111111111111"),
        )
        .json(&body)
        .await;
    r1.assert_status_ok();
    let r1_body = r1.text();

    // Second request — KEY form, SAME Idempotency-Key, SAME body. Must
    // replay r1's response (not 409 conflict).
    let (h, v) = auth_header(&env.token);
    let r2 = env
        .server
        .post(&format!("/api/tasks/{key}/modify"))
        .add_header(h, v)
        .add_header(
            idem_header.clone(),
            HeaderValue::from_static("alias-test-11111111-1111-1111-1111-111111111111"),
        )
        .json(&body)
        .await;
    r2.assert_status_ok();
    assert_eq!(
        r2.text(),
        r1_body,
        "key-form replay with same Idempotency-Key + body must return byte-identical response \
         (alias resolution lands on canonical UUID in dedup tuple)"
    );

    // Third request — KEY form, SAME Idempotency-Key, DIFFERENT body
    // → 409 IDEMPOTENCY_KEY_CONFLICT.
    let (h, v) = auth_header(&env.token);
    let r3 = env
        .server
        .post(&format!("/api/tasks/{key}/modify"))
        .add_header(h, v)
        .add_header(
            idem_header,
            HeaderValue::from_static("alias-test-11111111-1111-1111-1111-111111111111"),
        )
        .json(&serde_json::json!({"description": "Different body"}))
        .await;
    r3.assert_status(axum::http::StatusCode::CONFLICT);
    assert_eq!(r3.text(), "IDEMPOTENCY_KEY_CONFLICT");

    let _ = idem_key;
}

#[tokio::test]
async fn test_mutation_handlers_cross_account_key_returns_404() {
    // user1 attempting to mutate user2's task via user2's key must 404
    // (existence-leak parity with cross-account UUID).
    let env = setup().await;
    let user2 = add_second_user_with_task(&env).await;

    let (h, v) = auth_header(&env.token);
    let r = env
        .server
        .post(&format!("/api/tasks/{}/done", user2.task_key))
        .add_header(h, v)
        .await;
    r.assert_status(axum::http::StatusCode::NOT_FOUND);

    // user2 can still complete their own task via their own key.
    let (h, v) = auth_header(&user2.token);
    env.server
        .post(&format!("/api/tasks/{}/done", user2.task_key))
        .add_header(h, v)
        .await
        .assert_status_ok();

    let _ = user2.id;
}

// --- Batch GET /api/tasks?uuids=<csv> ---

#[tokio::test]
async fn test_batch_lookup_positive_mixed_found_missing() {
    let env = setup().await;
    let uuid_a = create_task(&env, "+test Task A").await;
    let uuid_b = create_task(&env, "+test Task B").await;
    let unknown = uuid::Uuid::new_v4().to_string();

    // Build CSV in deliberately non-sorted, non-insertion order.
    let csv = format!("{uuid_b},{unknown},{uuid_a}");

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?uuids={csv}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    let found = body["found"].as_array().unwrap();
    let missing = body["missing"].as_array().unwrap();

    // found preserves request-order — uuid_b before uuid_a.
    assert_eq!(found.len(), 2, "two known UUIDs should resolve");
    assert_eq!(found[0]["uuid"].as_str().unwrap(), uuid_b);
    assert_eq!(found[1]["uuid"].as_str().unwrap(), uuid_a);
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].as_str().unwrap(), unknown);

    // found.len + missing.len == deduped request count
    assert_eq!(found.len() + missing.len(), 3);
}

#[tokio::test]
async fn test_batch_lookup_request_order_preserved_not_uuid_sorted() {
    // Regression: don't reach for sort_unstable_by — the contract pins
    // request-order preservation for `found`. Use UUIDs with deterministic,
    // non-sorted insertion order to detect any accidental UUID-asc sort.
    let env = setup().await;
    let uuid_a = create_task(&env, "+test First").await; // arbitrary UUID
    let uuid_b = create_task(&env, "+test Second").await;
    let uuid_c = create_task(&env, "+test Third").await;

    // Build a CSV whose order is neither insertion-order nor UUID-ascending.
    // Sort UUIDs ascending and walk middle-first — that gives an order that
    // can't accidentally match either insertion or UUID-asc.
    let mut sorted = vec![&uuid_a, &uuid_b, &uuid_c];
    sorted.sort();
    let csv = format!("{},{},{}", sorted[1], sorted[0], sorted[2]);

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?uuids={csv}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let found = body["found"].as_array().unwrap();
    let actual_order: Vec<&str> = found.iter().map(|t| t["uuid"].as_str().unwrap()).collect();
    assert_eq!(
        actual_order,
        vec![sorted[1].as_str(), sorted[0].as_str(), sorted[2].as_str()],
        "found must preserve request order, not sort by UUID"
    );
}

#[tokio::test]
async fn test_batch_lookup_dedupe_first_occurrence_position() {
    let env = setup().await;
    let uuid_a = create_task(&env, "+test A").await;
    let uuid_b = create_task(&env, "+test B").await;
    // Request: B, A, B, A — deduped should be [B, A] in first-occurrence order.
    let csv = format!("{uuid_b},{uuid_a},{uuid_b},{uuid_a}");

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?uuids={csv}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let found = body["found"].as_array().unwrap();
    assert_eq!(found.len(), 2, "duplicates should be deduped");
    assert_eq!(found[0]["uuid"].as_str().unwrap(), uuid_b);
    assert_eq!(found[1]["uuid"].as_str().unwrap(), uuid_a);
}

#[tokio::test]
async fn test_batch_lookup_too_many_uuids_cap() {
    let env = setup().await;
    create_task(&env, "+test sentinel").await;

    // 101 distinct UUIDs — exceeds default cap (100).
    let mut entries: Vec<String> = (0..101).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    let csv = entries.drain(..).collect::<Vec<_>>().join(",");

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?uuids={csv}"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "TOO_MANY_UUIDS"
    );
}

#[tokio::test]
async fn test_batch_lookup_cap_applies_before_dedupe() {
    // Regression for the literal contract example: 101 copies of one valid
    // UUID → TOO_MANY_UUIDS, not a one-element lookup. Easy to break by
    // deduping early for "efficiency."
    let env = setup().await;
    let uuid = create_task(&env, "+test sentinel").await;

    // 101 copies of the same UUID.
    let csv = std::iter::repeat(uuid.as_str())
        .take(101)
        .collect::<Vec<_>>()
        .join(",");

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?uuids={csv}"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "TOO_MANY_UUIDS",
        "cap must apply BEFORE dedupe — 101 dupes is still too many"
    );
}

#[tokio::test]
async fn test_batch_lookup_empty_after_parse_returns_empty_uuids() {
    let env = setup().await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks?uuids=").add_header(h, v).await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(std::str::from_utf8(resp.as_bytes()).unwrap(), "EMPTY_UUIDS");
}

#[tokio::test]
async fn test_batch_lookup_empty_csv_segment_returns_invalid_uuid() {
    let env = setup().await;
    let uuid = create_task(&env, "+test sentinel").await;

    // Each of these has a syntactically-invalid empty segment.
    for csv in [
        format!("{uuid},,{uuid}"),
        format!("{uuid},"),
        format!(",{uuid}"),
    ] {
        let (h, v) = auth_header(&env.token);
        let resp = env
            .server
            .get(&format!("/api/tasks?uuids={csv}"))
            .add_header(h, v)
            .await;
        resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(
            std::str::from_utf8(resp.as_bytes()).unwrap(),
            "INVALID_UUID",
            "csv {csv:?} should yield INVALID_UUID"
        );
    }
}

#[tokio::test]
async fn test_batch_lookup_malformed_uuid_returns_invalid_uuid() {
    let env = setup().await;
    let uuid = create_task(&env, "+test sentinel").await;

    let csv = format!("{uuid},not-a-uuid");
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?uuids={csv}"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_UUID"
    );
}

#[tokio::test]
async fn test_batch_lookup_mutex_with_view_returns_invalid_query_param() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Both").await;

    // Use the seeded "next" view (or just any view id — does not need to exist
    // because mutex-check fires before view lookup).
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?view=next&uuids={uuid}"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_QUERY_PARAM"
    );
}

#[tokio::test]
async fn test_batch_lookup_unknown_query_param_returns_invalid_query_param() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Unknown").await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?uuids={uuid}&unknown=x"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_QUERY_PARAM"
    );
}

#[tokio::test]
async fn test_view_with_unknown_query_param_returns_invalid_query_param() {
    // Behaviour change: clients that previously sent unknown query params on
    // the view path got 200; now they get 400. Documented in CHANGELOG.
    let env = setup().await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks?view=next&future_param=1")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_QUERY_PARAM"
    );
}

#[tokio::test]
async fn test_repeated_uuids_key_returns_invalid_query_param() {
    let env = setup().await;
    let uuid = create_task(&env, "+test Repeated").await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?uuids={uuid}&uuids={uuid}"))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_QUERY_PARAM"
    );
}

#[tokio::test]
async fn test_repeated_view_key_returns_invalid_query_param() {
    let env = setup().await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks?view=a&view=b")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        std::str::from_utf8(resp.as_bytes()).unwrap(),
        "INVALID_QUERY_PARAM"
    );
}

#[tokio::test]
async fn test_no_params_returns_pending_list_unchanged_regression() {
    // Critical backwards-compat regression: no-params GET /api/tasks must
    // continue to return the pending list. The contract was originally
    // ambiguous on this and would have broken every existing client.
    let env = setup().await;
    create_task(&env, "+test One").await;
    create_task(&env, "+test Two").await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();
    let tasks: Vec<Value> = resp.json();
    assert_eq!(tasks.len(), 2);
}

#[tokio::test]
async fn test_batch_lookup_cross_account_uuid_in_missing() {
    // Existence-leak rule extends to batch: cross-account UUID and unknown
    // UUID both surface in the `missing` array, no distinction.
    let env = setup().await;
    let uuid_a = create_task(&env, "+test mine").await;
    let user2 = add_second_user_with_task(&env).await;
    let unknown = uuid::Uuid::new_v4().to_string();

    let csv = format!("{uuid_a},{},{unknown}", user2.task_uuid);
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?uuids={csv}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let found: Vec<&str> = body["found"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["uuid"].as_str().unwrap())
        .collect();
    let missing: Vec<&str> = body["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(found, vec![uuid_a.as_str()]);
    // Both cross-account and unknown UUIDs land in `missing`, in request-order.
    assert_eq!(missing, vec![user2.task_uuid.as_str(), unknown.as_str()]);
}

// --- TaskItem.key wire-shape: null vs omission (#130 Phase 5d) ---

/// Burn an existing allocation row via direct SQL — simulates the post-
/// reaper or post-rollback "key has been burned" transient null cause
/// from `task-write-contract.md` § Wire exposure (cause 2).
async fn burn_allocation_row(env: &TestEnv, task_uuid: &str) {
    let db_path = env.data_dir.join("config.sqlite");
    let conn = tokio_rusqlite::Connection::open(&db_path).await.unwrap();
    let task_uuid_owned = task_uuid.to_string();
    conn.call(move |conn| {
        conn.execute(
            "UPDATE task_key_allocations \
             SET state = 'burned', task_uuid = NULL \
             WHERE task_uuid = ?1",
            rusqlite::params![task_uuid_owned],
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .unwrap();
}

/// Force the allocation row for `task_uuid` into `state='pending'` with
/// a stale `created_at` — simulates the lookup-time-expired pending row
/// (cause 3 of the four transient null causes) without waiting wall-clock
/// seconds for the reaper.
async fn forge_expired_pending(env: &TestEnv, task_uuid: &str, age_seconds: i64) {
    let db_path = env.data_dir.join("config.sqlite");
    let conn = tokio_rusqlite::Connection::open(&db_path).await.unwrap();
    let task_uuid_owned = task_uuid.to_string();
    conn.call(move |conn| {
        conn.execute(
            &format!(
                "UPDATE task_key_allocations \
                 SET state = 'pending', \
                     created_at = datetime('now', '-{age_seconds} seconds') \
                 WHERE task_uuid = ?1"
            ),
            rusqlite::params![task_uuid_owned],
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .unwrap();
}

/// Delete the allocation row entirely — simulates the
/// pre-migration / orphan-UDA case (causes 1 + 4 collapse to the same
/// REST projection: no row in `task_key_allocations` for the UUID).
async fn delete_allocation_row(env: &TestEnv, task_uuid: &str) {
    let db_path = env.data_dir.join("config.sqlite");
    let conn = tokio_rusqlite::Connection::open(&db_path).await.unwrap();
    let task_uuid_owned = task_uuid.to_string();
    conn.call(move |conn| {
        conn.execute(
            "DELETE FROM task_key_allocations WHERE task_uuid = ?1",
            rusqlite::params![task_uuid_owned],
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn test_task_item_key_emits_null_not_omission_when_no_allocation_row() {
    // Wire-shape regression for `task-write-contract.md` § Wire exposure:
    // `TaskItem.key` is nullable (`string | null`) — when REST has no
    // committed/non-expired-pending row, the field emits `null` rather
    // than being omitted from the JSON. iOS/obsidian clients use the
    // explicit `null` to distinguish "transient" from "absent" cleanly.
    let env = setup().await;
    let uuid = create_task(&env, "+test wire-null").await;
    burn_allocation_row(&env, &uuid).await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks/{uuid}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert!(
        body.get("key").is_some(),
        "key must be present in the JSON object (not omitted); body: {body}",
    );
    assert!(
        body["key"].is_null(),
        "key must be JSON null when no allocation row exists; got {:?}",
        body["key"],
    );
}

#[tokio::test]
async fn test_task_item_key_emits_null_when_no_allocation_row() {
    // Cause 1 (pre-migration) + cause 4 (orphan UDA) — both reduce to
    // "no allocation row for the UUID" from REST's perspective.
    let env = setup().await;
    let uuid = create_task(&env, "+test wire-null no-row").await;
    delete_allocation_row(&env, &uuid).await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks/{uuid}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert!(
        body.get("key").is_some(),
        "key must be present; body: {body}"
    );
    assert!(body["key"].is_null(), "key must be null; got {body:?}");
}

#[tokio::test]
async fn test_task_item_key_emits_null_when_pending_past_timeout() {
    // Cause 3 (expired-pending, lookup-time). Reaper has not run; the
    // row is physically `pending` in the DB. REST projects null
    // anyway, decoupling correctness from reaper scheduling.
    let env = setup().await;
    let uuid = create_task(&env, "+test wire-null expired-pending").await;
    // Default pending_timeout is 300 seconds. 600 = comfortably past.
    forge_expired_pending(&env, &uuid, 600).await;

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks/{uuid}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert!(
        body.get("key").is_some(),
        "key must be present; body: {body}"
    );
    assert!(body["key"].is_null(), "key must be null; got {body:?}");
}

#[tokio::test]
async fn test_task_item_key_emits_null_in_pending_list_when_burned() {
    // Same wire-shape rule applies to list-shape responses. Pin it so
    // a future refactor of the projection map can't silently revert
    // the omission behaviour for some endpoints but not others.
    let env = setup().await;
    let uuid = create_task(&env, "+test wire-null list").await;
    burn_allocation_row(&env, &uuid).await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/api/tasks").add_header(h, v).await;
    resp.assert_status_ok();
    let arr = resp.json::<Value>();
    let item = arr
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["uuid"] == uuid)
        .expect("task in pending list");
    assert!(
        item.get("key").is_some(),
        "key must be present (not omitted) in list response; got {item}",
    );
    assert!(item["key"].is_null(), "key must be JSON null; got {item}");
}

#[tokio::test]
async fn test_batch_lookup_response_count_matches_deduped_input() {
    let env = setup().await;
    let uuid_a = create_task(&env, "+test A").await;
    let uuid_b = create_task(&env, "+test B").await;
    let unknown1 = uuid::Uuid::new_v4().to_string();
    let unknown2 = uuid::Uuid::new_v4().to_string();

    // 6 entries with one duplicate (uuid_a twice) → 5 deduped.
    let csv = format!("{uuid_a},{uuid_b},{unknown1},{uuid_a},{unknown2},{unknown1}");
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .get(&format!("/api/tasks?uuids={csv}"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let found = body["found"].as_array().unwrap().len();
    let missing = body["missing"].as_array().unwrap().len();
    assert_eq!(
        found + missing,
        4,
        "found.len + missing.len must equal deduped input count (4 unique UUIDs)"
    );
}
