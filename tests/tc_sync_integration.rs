//! Integration tests for the TaskChampion sync protocol endpoints.

mod common;

use std::sync::Arc;

use axum::http::{header, HeaderValue};
use axum::Router;
use axum_test::TestServer;
use tempfile::TempDir;
use uuid::Uuid;

use cmdock_server::app_state::AppState;
use cmdock_server::health;
use cmdock_server::runtime_policy::{RuntimeAccessMode, RuntimeDeleteAction, RuntimePolicy};
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::tc_sync;

/// Create a client_id header for a registered sync client.
fn client_id_header(client_id: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::HeaderName::from_static("x-client-id"),
        HeaderValue::from_str(client_id).unwrap(),
    )
}

const HS_CT: &str = "application/vnd.taskchampion.history-segment";
const SNAP_CT: &str = "application/vnd.taskchampion.snapshot";

async fn setup_with_store() -> (TestServer, String, TempDir, Arc<dyn ConfigStore>, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "syncuser".to_string(),
            password_hash: String::new(),
        })
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

    let config = common::test_server_config(tmp.path().to_path_buf());
    let state = AppState::new(store.clone(), &config);

    let app = Router::new()
        .merge(health::routes())
        .merge(tc_sync::routes())
        .with_state(state);

    let server = TestServer::new(app);
    (server, client_id, tmp, store, user.id)
}

/// Returns (server, client_id_string, tmp_dir)
async fn setup() -> (TestServer, String, tempfile::TempDir) {
    let (server, client_id, tmp, _store, _user_id) = setup_with_store().await;
    (server, client_id, tmp)
}

#[tokio::test]
async fn test_add_first_version() {
    let (server, token, _tmp) = setup().await;
    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&token);

    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"history-segment-data".to_vec().into())
        .await;

    resp.assert_status_ok();
    let vid_hdr = resp.header("X-Version-Id");
    Uuid::parse_str(vid_hdr.to_str().unwrap()).unwrap();
}

#[tokio::test]
async fn test_requires_client_id() {
    let (server, _token, _tmp) = setup().await;
    let nil = Uuid::nil();

    // No X-Client-Id header → 400
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .content_type(HS_CT)
        .bytes(b"data".to_vec().into())
        .await;

    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_get_child_version() {
    let (server, token, _tmp) = setup().await;
    let nil = Uuid::nil();

    // Add a version
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"data-v1".to_vec().into())
        .await;
    resp.assert_status_ok();
    let v1_id = resp.header("X-Version-Id").to_str().unwrap().to_string();

    // Get child of nil
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch, cv)
        .await;

    resp.assert_status_ok();
    assert_eq!(resp.header("X-Version-Id").to_str().unwrap(), v1_id);
    assert_eq!(
        resp.header("X-Parent-Version-Id").to_str().unwrap(),
        nil.to_string()
    );
    assert_eq!(
        resp.header("Content-Type").to_str().unwrap(),
        "application/vnd.taskchampion.history-segment"
    );
    assert_eq!(resp.header("Cache-Control").to_str().unwrap(), "no-store");
    assert_eq!(resp.into_bytes().as_ref(), b"data-v1");
}

#[tokio::test]
async fn test_get_child_version_up_to_date() {
    let (server, token, _tmp) = setup().await;

    // Add a version so NIL is known
    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"v1".to_vec().into())
        .await;
    resp.assert_status_ok();
    let v1 = resp.header("X-Version-Id").to_str().unwrap().to_string();

    // Get child of v1 (latest) → 404 (up to date)
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{v1}"))
        .add_header(ch, cv)
        .await;

    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_child_version_empty_server_unknown_parent() {
    let (server, token, _tmp) = setup().await;

    // Empty server, unknown parent → 404 (not 410, because server would accept first sync)
    let unknown = Uuid::new_v4();
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{unknown}"))
        .add_header(ch, cv)
        .await;

    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_child_version_gone_with_data() {
    let (server, token, _tmp) = setup().await;
    let nil = Uuid::nil();

    // Add a version so server has data
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"v1".to_vec().into())
        .await;
    resp.assert_status_ok();

    // Now ask for child of unknown parent → 410 GONE (server has data, parent unknown)
    let unknown = Uuid::new_v4();
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{unknown}"))
        .add_header(ch, cv)
        .await;

    resp.assert_status(axum::http::StatusCode::GONE);
}

#[tokio::test]
async fn test_runtime_policy_blocked_device_cannot_sync() {
    let (server, client_id, _tmp, store, user_id) = setup_with_store().await;
    let policy = RuntimePolicy {
        runtime_access: RuntimeAccessMode::Block,
        delete_action: RuntimeDeleteAction::Allow,
    };
    store
        .upsert_runtime_policy(
            &user_id,
            "block-v1",
            &policy,
            Some("block-v1"),
            Some(&policy),
            Some("2026-04-03 12:00:00"),
        )
        .await
        .unwrap();

    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&client_id);
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"blocked".to_vec().into())
        .await;

    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
    assert!(resp.text().contains("Runtime access blocked by policy"));
}

#[tokio::test]
async fn test_version_conflict() {
    let (server, token, _tmp) = setup().await;
    let nil = Uuid::nil();

    // Add first version
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"v1".to_vec().into())
        .await;
    resp.assert_status_ok();
    let v1_id = resp.header("X-Version-Id").to_str().unwrap().to_string();

    // Try with wrong parent → 409
    let wrong = Uuid::new_v4();
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{wrong}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"v2-conflict".to_vec().into())
        .await;

    resp.assert_status(axum::http::StatusCode::CONFLICT);
    assert_eq!(resp.header("X-Parent-Version-Id").to_str().unwrap(), v1_id);
}

#[tokio::test]
async fn test_version_chain() {
    let (server, token, _tmp) = setup().await;
    let nil = Uuid::nil();

    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"v1".to_vec().into())
        .await;
    resp.assert_status_ok();
    let v1 = resp.header("X-Version-Id").to_str().unwrap().to_string();

    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{v1}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"v2".to_vec().into())
        .await;
    resp.assert_status_ok();

    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{v1}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status_ok();
    assert_eq!(resp.into_bytes().as_ref(), b"v2");
}

#[tokio::test]
async fn test_snapshot_roundtrip() {
    let (server, token, _tmp) = setup().await;
    let nil = Uuid::nil();

    // Add a version first (snapshot requires valid version_id)
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"v1".to_vec().into())
        .await;
    resp.assert_status_ok();
    let vid = resp.header("X-Version-Id").to_str().unwrap().to_string();
    let vid_uuid = Uuid::parse_str(&vid).unwrap();

    // No snapshot initially
    let (ch, cv) = client_id_header(&token);
    let resp = server.get("/v1/client/snapshot").add_header(ch, cv).await;
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);

    // Add snapshot for existing version
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-snapshot/{vid_uuid}"))
        .add_header(ch, cv)
        .content_type(SNAP_CT)
        .bytes(b"snap-data".to_vec().into())
        .await;
    resp.assert_status_ok();

    // Get snapshot
    let (ch, cv) = client_id_header(&token);
    let resp = server.get("/v1/client/snapshot").add_header(ch, cv).await;
    resp.assert_status_ok();
    assert_eq!(resp.header("X-Version-Id").to_str().unwrap(), vid);
    assert_eq!(
        resp.header("Content-Type").to_str().unwrap(),
        "application/vnd.taskchampion.snapshot"
    );
    assert_eq!(resp.header("Cache-Control").to_str().unwrap(), "no-store");
    assert_eq!(resp.into_bytes().as_ref(), b"snap-data");
}

#[tokio::test]
async fn test_snapshot_invalid_version() {
    let (server, token, _tmp) = setup().await;
    let fake_vid = Uuid::new_v4();

    // Snapshot for nonexistent version → 400
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-snapshot/{fake_vid}"))
        .add_header(ch, cv)
        .content_type(SNAP_CT)
        .bytes(b"snap".to_vec().into())
        .await;

    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_invalid_client_id() {
    let (server, _token, _tmp) = setup().await;
    let nil = Uuid::nil();

    // Invalid X-Client-Id → 400
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(
            header::HeaderName::from_static("x-client-id"),
            HeaderValue::from_static("not-a-uuid"),
        )
        .content_type(HS_CT)
        .bytes(b"data".to_vec().into())
        .await;

    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_wrong_content_type() {
    let (server, token, _tmp) = setup().await;
    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&token);

    // Wrong content-type → 415 Unsupported Media Type
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type("application/json")
        .bytes(b"data".to_vec().into())
        .await;

    resp.assert_status(axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn test_snapshot_wrong_content_type() {
    let (server, token, _tmp) = setup().await;
    let nil = Uuid::nil();

    // Add a version first
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"v1".to_vec().into())
        .await;
    resp.assert_status_ok();
    let vid = resp.header("X-Version-Id").to_str().unwrap().to_string();

    // Wrong content-type on snapshot → 415
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-snapshot/{vid}"))
        .add_header(ch, cv)
        .content_type("application/json")
        .bytes(b"snap".to_vec().into())
        .await;

    resp.assert_status(axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn test_first_version_non_nil_parent_rejected() {
    // First version must be rooted at NIL — non-NIL parent returns 409 Conflict
    // with X-Parent-Version-Id pointing to NIL (the expected parent).
    let (server, token, _tmp) = setup().await;
    let non_nil = Uuid::new_v4();
    let (ch, cv) = client_id_header(&token);

    let resp = server
        .post(&format!("/v1/client/add-version/{non_nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(b"first-version".to_vec().into())
        .await;

    resp.assert_status(axum::http::StatusCode::CONFLICT);
    let header_val = resp.header("X-Parent-Version-Id");
    let expected_parent = header_val.to_str().unwrap();
    assert_eq!(expected_parent, Uuid::nil().to_string());
}

#[tokio::test]
async fn test_client_id_required_on_all_endpoints() {
    let (server, _token, _tmp) = setup().await;
    let nil = Uuid::nil();

    // GET get-child-version without X-Client-Id → 400
    let resp = server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

    // GET snapshot without X-Client-Id → 400
    let resp = server.get("/v1/client/snapshot").await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

    // POST add-snapshot without X-Client-Id → 400
    let vid = Uuid::new_v4();
    let resp = server
        .post(&format!("/v1/client/add-snapshot/{vid}"))
        .content_type(SNAP_CT)
        .bytes(b"snap".to_vec().into())
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_content_type_with_params() {
    let (server, token, _tmp) = setup().await;
    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&token);

    // Content-Type with charset param should be accepted
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type("application/vnd.taskchampion.history-segment; charset=utf-8")
        .bytes(b"data".to_vec().into())
        .await;

    resp.assert_status_ok();
}

#[tokio::test]
async fn test_sync_requires_auth() {
    let (server, _token, _tmp) = setup().await;
    let nil = Uuid::nil();

    // No X-Client-Id at all → 400
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .content_type(HS_CT)
        .bytes(b"data".to_vec().into())
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

    // Valid UUID but unregistered client_id → 403 (no info leak about whether ID exists)
    let unregistered = Uuid::new_v4();
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(
            header::HeaderName::from_static("x-client-id"),
            HeaderValue::from_str(&unregistered.to_string()).unwrap(),
        )
        .content_type(HS_CT)
        .bytes(b"data".to_vec().into())
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
}
