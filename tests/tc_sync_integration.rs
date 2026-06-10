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
use cmdock_server::merged_sync_gateway::codec::{encode_history_segment, WireOp, WireVersion};
use cmdock_server::runtime_policy::{RuntimeAccessMode, RuntimeDeleteAction, RuntimePolicy};
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::tc_sync;
use cmdock_server::tc_sync::storage::SyncStorage;

/// Create a client_id header for a registered sync client.
fn client_id_header(client_id: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::HeaderName::from_static("x-client-id"),
        HeaderValue::from_str(client_id).unwrap(),
    )
}

const HS_CT: &str = "application/vnd.taskchampion.history-segment";
const SNAP_CT: &str = "application/vnd.taskchampion.snapshot";

fn valid_history_segment() -> Vec<u8> {
    encode_history_segment(&WireVersion {
        operations: vec![WireOp::Create {
            uuid: Uuid::new_v4(),
        }],
    })
    .unwrap()
}

async fn setup_with_store() -> (TestServer, String, TempDir, Arc<dyn ConfigStore>, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let sqlite_store = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite_store.clone();
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "syncuser".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();
    cmdock_server::admin::prefix::backfill_missing_user_prefixes(store.as_ref())
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
    let state = AppState::new(store.clone(), sqlite_store.clone(), &config);

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
        .bytes(valid_history_segment().into())
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
        .bytes(valid_history_segment().into())
        .await;

    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_get_child_version() {
    let (server, token, _tmp) = setup().await;
    let nil = Uuid::nil();

    // Add a version
    let body = valid_history_segment();
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(body.clone().into())
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
    assert_eq!(resp.into_bytes().as_ref(), body.as_slice());
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
        .bytes(valid_history_segment().into())
        .await;
    resp.assert_status_ok();
    let v1 = resp.header("X-Version-Id").to_str().unwrap().to_string();

    // The gateway may append a corrective/projection child after the inbound
    // version. Follow it once if present, then the resulting tip is up to date.
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{v1}"))
        .add_header(ch, cv)
        .await;
    let latest = if resp.status_code() == axum::http::StatusCode::OK {
        resp.header("X-Version-Id").to_str().unwrap().to_string()
    } else {
        resp.assert_status(axum::http::StatusCode::NOT_FOUND);
        v1
    };

    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{latest}"))
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
        .bytes(valid_history_segment().into())
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
        .bytes(valid_history_segment().into())
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
        .bytes(valid_history_segment().into())
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
        .bytes(valid_history_segment().into())
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
        .bytes(valid_history_segment().into())
        .await;
    resp.assert_status_ok();
    let v1 = resp.header("X-Version-Id").to_str().unwrap().to_string();

    let v2_body = valid_history_segment();
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{v1}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(v2_body.clone().into())
        .await;
    resp.assert_status_ok();

    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{v1}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status_ok();
    assert_eq!(resp.into_bytes().as_ref(), v2_body.as_slice());
}

#[tokio::test]
async fn stale_parent_conflict_retry_succeeds_gate3_sequential_replay() {
    // ADR-0012 Gate 3: a client that submits a stale parent receives 409 with
    // X-Parent-Version-Id pointing at the true chain tip, then succeeds on
    // retry using that header value as the new parent.
    let (server, token, _tmp) = setup().await;
    let nil = Uuid::nil();

    // Submit v1 at NIL parent.
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(valid_history_segment().into())
        .await;
    resp.assert_status_ok();
    let v1_id = resp.header("X-Version-Id").to_str().unwrap().to_string();

    // The gateway may append a corrective projection child after v1. Follow
    // the chain once to find the actual current tip.
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{v1_id}"))
        .add_header(ch, cv)
        .await;
    let current_tip = if resp.status_code() == axum::http::StatusCode::OK {
        resp.header("X-Version-Id").to_str().unwrap().to_string()
    } else {
        resp.assert_status(axum::http::StatusCode::NOT_FOUND);
        v1_id.clone()
    };

    // Attempt v2 with stale NIL parent → 409, X-Parent-Version-Id = current tip.
    let v2_body = valid_history_segment();
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(v2_body.clone().into())
        .await;
    resp.assert_status(axum::http::StatusCode::CONFLICT);
    let corrected_parent = resp
        .header("X-Parent-Version-Id")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        corrected_parent, current_tip,
        "409 must carry X-Parent-Version-Id pointing at the current chain tip"
    );

    // Retry v2 with the corrected parent → 200 OK.
    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-version/{corrected_parent}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(v2_body.into())
        .await;
    resp.assert_status_ok();
    Uuid::parse_str(resp.header("X-Version-Id").to_str().unwrap()).unwrap();
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
        .bytes(valid_history_segment().into())
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
async fn fresh_clone_from_snapshot_replays_retained_post_snapshot_history() {
    let (server, token, tmp, _store, user_id) = setup_with_store().await;
    let user_dir = tmp.path().join("users").join(&user_id);
    let storage = SyncStorage::open_merged(&user_dir).unwrap();
    let v1 = storage.add_version(Uuid::nil(), b"v1").unwrap().unwrap();
    let v2 = storage.add_version(v1, b"v2").unwrap().unwrap();
    let v3 = storage
        .add_version(v2, b"v3-after-snapshot")
        .unwrap()
        .unwrap();

    let (ch, cv) = client_id_header(&token);
    let resp = server
        .post(&format!("/v1/client/add-snapshot/{v2}"))
        .add_header(ch, cv)
        .content_type(SNAP_CT)
        .bytes(b"snapshot-at-v2".to_vec().into())
        .await;
    resp.assert_status_ok();

    assert_eq!(storage.garbage_collect_older_than_snapshot(0).unwrap(), 2);

    let (ch, cv) = client_id_header(&token);
    let resp = server.get("/v1/client/snapshot").add_header(ch, cv).await;
    resp.assert_status_ok();
    assert_eq!(
        resp.header("X-Version-Id").to_str().unwrap(),
        v2.to_string()
    );
    assert_eq!(resp.into_bytes().as_ref(), b"snapshot-at-v2");

    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{v2}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status_ok();
    assert_eq!(
        resp.header("X-Version-Id").to_str().unwrap(),
        v3.to_string()
    );
    assert_eq!(resp.into_bytes().as_ref(), b"v3-after-snapshot");
}

#[tokio::test]
async fn stale_client_before_gc_boundary_gets_gone_and_snapshot_remains_available() {
    let (server, token, tmp, _store, user_id) = setup_with_store().await;
    let user_dir = tmp.path().join("users").join(&user_id);
    let storage = SyncStorage::open_merged(&user_dir).unwrap();
    let v1 = storage.add_version(Uuid::nil(), b"v1").unwrap().unwrap();
    let v2 = storage.add_version(v1, b"v2").unwrap().unwrap();
    let _v3 = storage.add_version(v2, b"v3").unwrap().unwrap();
    storage.add_snapshot(v2, b"snapshot-at-v2").unwrap();
    storage.garbage_collect_older_than_snapshot(0).unwrap();

    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{v1}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status(axum::http::StatusCode::GONE);

    let (ch, cv) = client_id_header(&token);
    let resp = server.get("/v1/client/snapshot").add_header(ch, cv).await;
    resp.assert_status_ok();
    assert_eq!(
        resp.header("X-Version-Id").to_str().unwrap(),
        v2.to_string()
    );
}

#[tokio::test]
async fn snapshot_before_delete_keeps_delete_version_until_post_delete_snapshot() {
    let (server, token, tmp, _store, user_id) = setup_with_store().await;
    let user_dir = tmp.path().join("users").join(&user_id);
    let storage = SyncStorage::open_merged(&user_dir).unwrap();
    let v1 = storage
        .add_version(Uuid::nil(), b"create")
        .unwrap()
        .unwrap();
    let v2 = storage
        .add_version(v1, b"delete-or-corrective")
        .unwrap()
        .unwrap();

    storage.add_snapshot(v1, b"pre-delete-snapshot").unwrap();
    storage.garbage_collect_older_than_snapshot(0).unwrap();

    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{v1}"))
        .add_header(ch, cv)
        .await;
    resp.assert_status_ok();
    assert_eq!(
        resp.header("X-Version-Id").to_str().unwrap(),
        v2.to_string()
    );
    assert_eq!(resp.into_bytes().as_ref(), b"delete-or-corrective");

    storage.add_snapshot(v2, b"post-delete-snapshot").unwrap();
    storage.garbage_collect_older_than_snapshot(0).unwrap();
    assert!(!storage.version_exists(v2).unwrap());
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
        .bytes(valid_history_segment().into())
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
        .bytes(valid_history_segment().into())
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
        .bytes(valid_history_segment().into())
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
        .bytes(valid_history_segment().into())
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
async fn add_version_records_gateway_journal_and_uses_merged_storage() {
    let (server, token, tmp, _store, user_id) = setup_with_store().await;
    let nil = Uuid::nil();
    let body = valid_history_segment();
    let (ch, cv) = client_id_header(&token);

    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(body.clone().into())
        .await;
    resp.assert_status_ok();
    let version_id = Uuid::parse_str(resp.header("X-Version-Id").to_str().unwrap()).unwrap();

    let db = rusqlite::Connection::open(tmp.path().join("config.sqlite")).unwrap();
    let (state, inbound_len): (String, i64) = db
        .query_row(
            "SELECT state, length(inbound_history_segment) FROM merged_sync_journal WHERE user_id = ?1",
            [&user_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "finalized");
    assert_eq!(inbound_len, body.len() as i64);

    let merged = SyncStorage::open_merged(&tmp.path().join("users").join(&user_id)).unwrap();
    assert!(merged.version_exists(version_id).unwrap());
    assert!(!tmp
        .path()
        .join("users")
        .join(&user_id)
        .join("sync.sqlite")
        .exists());
}

#[tokio::test]
async fn old_personal_sync_storage_is_not_served_after_cutover() {
    let (server, token, tmp, _store, user_id) = setup_with_store().await;
    let nil = Uuid::nil();
    let old_sentinel = b"old-personal-chain-sentinel".to_vec();
    let user_dir = tmp.path().join("users").join(&user_id);
    let old = SyncStorage::open(&user_dir).unwrap();
    let old_version = old.add_version(nil, &old_sentinel).unwrap().unwrap();

    let (ch, cv) = client_id_header(&token);
    let resp = server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch, cv)
        .await;

    // The gateway may project source truth and return a merged-chain version,
    // or it may be empty. It must never expose the old direct personal chain.
    if resp.status_code() == axum::http::StatusCode::OK {
        assert_ne!(
            resp.header("X-Version-Id").to_str().unwrap(),
            old_version.to_string()
        );
        assert_ne!(resp.into_bytes().as_ref(), old_sentinel.as_slice());
    } else {
        resp.assert_status(axum::http::StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn new_device_identity_writes_gateway_chain_only() {
    let (server, token, tmp, _store, user_id) = setup_with_store().await;
    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&token);

    let resp = server
        .post(&format!("/v1/client/add-version/{nil}"))
        .add_header(ch, cv)
        .content_type(HS_CT)
        .bytes(valid_history_segment().into())
        .await;

    resp.assert_status_ok();
    assert!(tmp
        .path()
        .join("users")
        .join(&user_id)
        .join("merged/sync.sqlite")
        .exists());
    assert!(!tmp
        .path()
        .join("users")
        .join(&user_id)
        .join("sync.sqlite")
        .exists());
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
        .bytes(valid_history_segment().into())
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
        .bytes(valid_history_segment().into())
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
        .bytes(valid_history_segment().into())
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
}
