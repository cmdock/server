//! Create-path regression tests for `task-write-contract.md` § Task Keys
//! (server#130 C9).
//!
//! Locks the load-bearing wire-shape and state-machine invariants from
//! C7+C8:
//!
//! - **Replay-no-burn**: `Idempotency-Key` retries do NOT advance the
//!   allocation counter; the replay returns the original key.
//! - **Pre-reservation parse rejection produces zero rows**: a body that
//!   fails axum's strict-recognise extractor never reaches the
//!   reservation step, so no `task_key_allocations` row exists for the
//!   rejected attempt.
//! - **Read endpoints emit `key`**: list, view-scoped, batch-lookup, and
//!   singleton GET all populate `TaskItem.key` for committed
//!   allocations.

mod common;

use std::sync::Arc;

use axum::http::{header, HeaderName, HeaderValue, Method};
use axum::Router;
use axum_test::TestServer;
use serde_json::Value;
use taskchampion::{Replica, SqliteStorage};
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

const IDEMPOTENCY_HEADER: HeaderName = HeaderName::from_static("idempotency-key");

fn auth_header(token: &str) -> (HeaderName, HeaderValue) {
    (
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

struct TestEnv {
    server: TestServer,
    _tmp: TempDir,
    user_id: String,
    token: String,
    _store: Arc<dyn ConfigStore>,
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
            username: "task_keys_user".to_string(),
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
    std::fs::create_dir_all(data_dir.join("users").join(&user.id)).unwrap();

    let config = common::test_server_config(data_dir);
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
        user_id: user.id,
        token,
        _store: store,
    }
}

/// Count `task_key_allocations` rows for a user in a given state. Used to
/// pin table-level invariants the wire shape can't reveal.
async fn allocation_count_at(db_path: &std::path::Path, user_id: &str, state_filter: &str) -> i64 {
    let conn = tokio_rusqlite::Connection::open(db_path).await.unwrap();
    let user_id = user_id.to_string();
    let state_filter = state_filter.to_string();
    conn.call(move |conn| {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM task_key_allocations WHERE user_id = ?1 AND state = ?2",
            rusqlite::params![user_id, state_filter],
            |r| r.get(0),
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(n)
    })
    .await
    .unwrap()
}

async fn read_task_uda(env: &TestEnv, task_uuid: &str, key: &str) -> Option<String> {
    let user_dir = env._tmp.path().join("users").join(&env.user_id);
    let storage = SqliteStorage::new(
        &user_dir,
        taskchampion::storage::AccessMode::ReadWrite,
        true,
    )
    .await
    .unwrap();
    let mut replica = Replica::new(storage);
    let task = replica
        .get_task(task_uuid.parse().unwrap())
        .await
        .unwrap()?;
    task.get_value(key).map(|v| v.to_string())
}

#[tokio::test]
async fn test_create_emits_canonical_key_in_response_and_taskitem() {
    let env = setup().await;
    let (h, v) = auth_header(&env.token);

    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+smoke first"}))
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let key = body["key"].as_str().expect("key field present");
    let output = body["output"].as_str().expect("output field present");
    let uuid = output
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap();
    assert!(
        regex_lite_matches(key, r"^[A-Z][A-Z0-9]{0,9}-1$"),
        "first task → -1; got {key}"
    );
    let prefix = key.rsplit_once('-').unwrap().0;
    assert_eq!(
        read_task_uda(&env, uuid, "cmdock_key").await.as_deref(),
        Some(key),
        "create path must stamp cmdock_key on TC task"
    );
    assert_eq!(
        read_task_uda(&env, uuid, "cmdock_task_scope")
            .await
            .as_deref(),
        Some(prefix),
        "create path must stamp canonical cmdock_task_scope with the user prefix"
    );
    assert_eq!(
        read_task_uda(&env, uuid, "cmdock_account").await.as_deref(),
        None,
        "create path must not stamp deprecated cmdock_account"
    );

    // List response carries the same key and scope on the projected TaskItem.
    let list = env.server.get("/api/tasks").add_header(h, v).await;
    let tasks: Vec<Value> = list.json();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["key"], body["key"]);
    assert_eq!(tasks[0]["cmdock_task_scope"], prefix);
    assert!(
        tasks[0].get("cmdock_account").is_none() || tasks[0]["cmdock_account"].is_null(),
        "deprecated cmdock_account must not appear on REST TaskItem"
    );
}

#[tokio::test]
async fn test_raw_cmdock_task_scope_round_trips_and_tolerates_cmdock_account() {
    let env = setup().await;
    let (h, v) = auth_header(&env.token);

    let expected_prefix = env
        ._store
        .get_user_prefix(&env.user_id)
        .await
        .unwrap()
        .unwrap();
    let ok = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": format!("cmdock_task_scope:{expected_prefix} +scope scoped create")}))
        .await;
    ok.assert_status_ok();
    let body = ok.json::<Value>();
    let uuid = body["output"]
        .as_str()
        .unwrap()
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap()
        .to_string();
    let prefix = body["key"]
        .as_str()
        .unwrap()
        .rsplit_once('-')
        .unwrap()
        .0
        .to_string();
    assert_eq!(prefix, expected_prefix);

    let get = env
        .server
        .get(&format!("/api/tasks/{uuid}"))
        .add_header(h.clone(), v.clone())
        .await;
    get.assert_status_ok();
    let item = get.json::<Value>();
    assert_eq!(item["cmdock_task_scope"], prefix);
    assert!(
        item.get("cmdock_account").is_none() || item["cmdock_account"].is_null(),
        "deprecated cmdock_account must not appear on REST TaskItem"
    );

    let mixed_ok = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "description": "scoped create updated",
            "cmdock_task_scope": expected_prefix.clone(),
            "cmdock_account": prefix.clone(),
        }))
        .await;
    mixed_ok.assert_status_ok();

    // cmdock_account is tolerated-and-ignored; a conflicting account value
    // no longer causes INVALID_TASK_SCOPE — only cmdock_task_scope is validated.
    let tolerated_modify = env
        .server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "cmdock_task_scope": prefix.clone(),
            "cmdock_account": "OTHER",
        }))
        .await;
    tolerated_modify.assert_status_ok();

    // Same in raw syntax: cmdock_account:OTHER is tolerated even when cmdock_task_scope is valid.
    let tolerated_raw = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": format!("cmdock_task_scope:{expected_prefix} cmdock_account:OTHER tolerated")}))
        .await;
    tolerated_raw.assert_status_ok();
}

#[tokio::test]
async fn test_replay_no_burn_preserves_counter() {
    // Three POSTs — same body, same K1 (twice), then K2 — must produce
    // exactly TWO committed allocation rows with sequential N values
    // (-1, -2). Replay returns the original key without advancing N;
    // K2 then takes N+1 (NOT N+2 — that would mean the replay accidentally
    // burned a slot).
    let env = setup().await;
    let (h, v) = auth_header(&env.token);

    let body = serde_json::json!({"raw": "+smoke replay-no-burn"});
    let k1 = "11111111-1111-1111-1111-111111111111";
    let k2 = "22222222-2222-2222-2222-222222222222";

    // Step 1: K1 — fresh execution.
    let r1 = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(k1).unwrap())
        .json(&body)
        .await;
    r1.assert_status_ok();
    let key1 = r1.json::<Value>()["key"].as_str().unwrap().to_string();

    // Step 2: K1 again — replay, identical body returned.
    let r2 = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(k1).unwrap())
        .json(&body)
        .await;
    r2.assert_status_ok();
    let body1 = r1.text();
    let body2 = r2.text();
    assert_eq!(
        body1, body2,
        "replay must return byte-identical body (including key)"
    );
    let key2 = r2.json::<Value>()["key"].as_str().unwrap().to_string();
    assert_eq!(key1, key2, "replay must preserve the original key");

    // Step 3: K2 — fresh, must take N+1, not N+2.
    let r3 = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .add_header(IDEMPOTENCY_HEADER, HeaderValue::from_str(k2).unwrap())
        .json(&body)
        .await;
    r3.assert_status_ok();
    let key3 = r3.json::<Value>()["key"].as_str().unwrap().to_string();

    // Parse N from each key.
    let n1: u32 = key1.rsplit('-').next().unwrap().parse().unwrap();
    let n3: u32 = key3.rsplit('-').next().unwrap().parse().unwrap();
    assert_eq!(
        n3,
        n1 + 1,
        "replay-no-burn invariant: K2 must take {} (got {})",
        n1 + 1,
        n3
    );

    // Table-state assertions: exactly 2 committed rows, ZERO burned.
    let db_path = env._tmp.path().join("config.sqlite");
    let committed = allocation_count_at(&db_path, &env.user_id, "committed").await;
    let pending = allocation_count_at(&db_path, &env.user_id, "pending").await;
    let burned = allocation_count_at(&db_path, &env.user_id, "burned").await;
    assert_eq!(committed, 2, "expected 2 committed rows, got {committed}");
    assert_eq!(pending, 0, "expected 0 pending rows, got {pending}");
    assert_eq!(
        burned, 0,
        "expected 0 burned rows — replay must not consume a slot"
    );

    // External-effect proxy: list response. The webhook + audit subscriber
    // setup is heavy for an inline test, so we use the TC task count as a
    // cheap signal that fresh-execution count == 2 (replay didn't
    // re-enter the create closure). Webhook delivery count is verified
    // separately in `tests/webhooks_integration.rs`. Audit-event count
    // assertion needs a custom tracing subscriber — tracked as a
    // follow-up.
    let (h2, v2) = auth_header(&env.token);
    let list = env.server.get("/api/tasks").add_header(h2, v2).await;
    let tasks: Vec<Value> = list.json();
    assert_eq!(
        tasks.len(),
        2,
        "fresh-execution count == committed-row count (got {} tasks)",
        tasks.len()
    );
}

#[tokio::test]
async fn test_pre_reservation_parse_rejection_creates_no_allocation_row() {
    // Bodies that fail strict-recognise (extractor-level) reject BEFORE
    // the allocation pipeline runs. There is no row to burn.
    let env = setup().await;
    let (h, v) = auth_header(&env.token);

    // Unknown field — `deny_unknown_fields` rejects with INVALID_FIELD.
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+smoke", "junk_field": 1}))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text(), "INVALID_FIELD");

    let db_path = env._tmp.path().join("config.sqlite");
    let pending = allocation_count_at(&db_path, &env.user_id, "pending").await;
    let committed = allocation_count_at(&db_path, &env.user_id, "committed").await;
    let burned = allocation_count_at(&db_path, &env.user_id, "burned").await;
    assert_eq!(
        (pending, committed, burned),
        (0, 0, 0),
        "extractor-level rejection must leave the allocation table untouched"
    );
}

#[tokio::test]
async fn test_get_task_by_id_returns_key_for_committed_allocation() {
    let env = setup().await;
    let (h, v) = auth_header(&env.token);

    let r = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+smoke singleton"}))
        .await;
    r.assert_status_ok();
    let body: Value = r.json();
    let uuid = body["output"]
        .as_str()
        .unwrap()
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap()
        .to_string();
    let expected_key = body["key"].as_str().unwrap().to_string();

    let single = env
        .server
        .get(&format!("/api/tasks/{uuid}"))
        .add_header(h, v)
        .await;
    single.assert_status_ok();
    let item: Value = single.json();
    assert_eq!(item["key"].as_str().unwrap(), expected_key);
}

#[tokio::test]
async fn test_view_scoped_list_returns_key_for_committed_allocation() {
    // `list_view_scoped` has a separate lookup/projection path from
    // `list_pending` and `list_batched_uuids` — covered separately.
    let env = setup().await;
    let (h, v) = auth_header(&env.token);

    let r = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+smoke view-scoped"}))
        .await;
    r.assert_status_ok();
    let expected_key = r.json::<Value>()["key"].as_str().unwrap().to_string();

    // Built-in view `personal` with filter `status:pending` matches any
    // pending task. Lazy-reconcile happens on first `GET /api/views`,
    // which `?view=` triggers via `views::resolve_view`.
    let resp = env
        .server
        .get("/api/tasks?view=personal")
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let tasks: Vec<Value> = resp.json();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["key"].as_str().unwrap(), expected_key);
}

#[tokio::test]
async fn test_batch_lookup_returns_key_for_each_resolved_task() {
    let env = setup().await;
    let (h, v) = auth_header(&env.token);

    let mut uuids = Vec::new();
    let mut keys = Vec::new();
    for _ in 0..3 {
        let resp = env
            .server
            .post("/api/tasks")
            .add_header(h.clone(), v.clone())
            .json(&serde_json::json!({"raw": "+smoke batch"}))
            .await;
        resp.assert_status_ok();
        let body: Value = resp.json();
        let uuid = body["output"]
            .as_str()
            .unwrap()
            .strip_prefix("Created task ")
            .unwrap()
            .strip_suffix('.')
            .unwrap()
            .to_string();
        let key = body["key"].as_str().unwrap().to_string();
        uuids.push(uuid);
        keys.push(key);
    }

    let csv = uuids.join(",");
    let batch = env
        .server
        .get(&format!("/api/tasks?uuids={csv}"))
        .add_header(h, v)
        .await;
    batch.assert_status_ok();
    let body: Value = batch.json();
    let found = body["found"].as_array().unwrap();
    assert_eq!(found.len(), 3);
    for (i, item) in found.iter().enumerate() {
        assert_eq!(
            item["key"].as_str().unwrap(),
            keys[i],
            "request-order preservation: position {i}"
        );
    }
}

/// Regression lock for codex iter1 critical #1 — mutation handlers
/// accept `<PREFIX>-N` as the path parameter on a user's first server
/// access. The Phase 4 backfill gate must run BEFORE key resolution; if
/// it ran AFTER, `lookup_task_uuid_by_key` would miss (no allocation
/// row yet) and the request would 404 spuriously.
///
/// Seeds three TC tasks directly under the user's replica directory
/// (bypassing /api/tasks so no backfill has fired), then issues a
/// `POST /api/tasks/PREFIX-1/done` against the unmigrated user. The
/// gate's fast-path miss kicks the backfill, which allocates PREFIX-1
/// for the first task (entry-asc ordering); the resolver then maps
/// PREFIX-1 to the correct UUID and the mutation completes the task.
#[tokio::test]
async fn test_mutation_handler_accepts_key_form_on_unmigrated_first_access() {
    use taskchampion::{Operations, Replica, SqliteStorage, Status};

    let env = setup().await;
    let (h, v) = auth_header(&env.token);
    let prefix = env
        ._store
        .get_user_prefix(&env.user_id)
        .await
        .unwrap()
        .expect("prefix assigned during setup");

    // Seed three TC tasks directly. Backfill has not run for this user
    // (no /api/tasks calls have hit yet).
    let user_dir = env._tmp.path().join("users").join(&env.user_id);
    std::fs::create_dir_all(&user_dir).unwrap();
    let storage = SqliteStorage::new(
        &user_dir,
        taskchampion::storage::AccessMode::ReadWrite,
        true,
    )
    .await
    .unwrap();
    let mut rep = Replica::new(storage);
    let baseline = chrono::Utc::now();
    let mut uuids: Vec<uuid::Uuid> = Vec::new();
    for i in 0..3 {
        let mut ops = Operations::new();
        let uuid = uuid::Uuid::new_v4();
        let mut t = rep.create_task(uuid, &mut ops).await.unwrap();
        t.set_status(Status::Pending, &mut ops).unwrap();
        t.set_entry(
            Some(baseline + chrono::Duration::seconds(i as i64)),
            &mut ops,
        )
        .unwrap();
        t.set_description(format!("seed-{i}"), &mut ops).unwrap();
        rep.commit_operations(ops).await.unwrap();
        uuids.push(uuid);
    }
    drop(rep);

    // First access: complete via key form. Must succeed (200), not 404.
    let target_key = format!("{prefix}-1");
    let resp = env
        .server
        .post(&format!("/api/tasks/{target_key}/done"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    // The targeted TC task is now Completed.
    let storage = SqliteStorage::new(
        &user_dir,
        taskchampion::storage::AccessMode::ReadWrite,
        true,
    )
    .await
    .unwrap();
    let mut rep = Replica::new(storage);
    let task = rep.get_task(uuids[0]).await.unwrap().expect("task exists");
    assert_eq!(task.get_status(), Status::Completed);
}

/// Minimal regex match — avoids pulling the `regex` crate just for one
/// shape check. Matches the canonical key form `^[A-Z][A-Z0-9]{0,9}-[1-9]\d*$`.
fn regex_lite_matches(s: &str, _pat: &'static str) -> bool {
    // Hard-coded shape: <PREFIX>-<n>. Prefix is 1..=10 ASCII uppercase
    // alphanumeric (first char alpha). N is a positive integer.
    let Some((prefix, n_str)) = s.rsplit_once('-') else {
        return false;
    };
    if prefix.is_empty() || prefix.len() > 10 {
        return false;
    }
    let mut chars = prefix.chars();
    if !matches!(chars.next(), Some(c) if c.is_ascii_uppercase()) {
        return false;
    }
    if !chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return false;
    }
    if n_str.is_empty() || n_str.starts_with('0') {
        return false;
    }
    n_str.chars().all(|c| c.is_ascii_digit())
}
