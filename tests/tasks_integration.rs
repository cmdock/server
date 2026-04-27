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
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "tasks_user".to_string(),
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

    let state = AppState::new(store.clone(), &config);

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
    ];
    for key in obj.keys() {
        assert!(
            known_keys.contains(&key.as_str()),
            "unexpected key '{key}' in TaskItem without UDAs"
        );
    }
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
