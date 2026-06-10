mod common;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use axum::http::{header, HeaderValue, Method};
use axum::Router;
use axum_test::TestServer;
use base64::Engine;
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::Value;
use tempfile::TempDir;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use cmdock_server::admin;
use cmdock_server::app_state::AppState;
use cmdock_server::crypto;
use cmdock_server::health;
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::tasks;
use cmdock_server::tc_sync;
use cmdock_server::webhooks;
use cmdock_server::webhooks::delivery::{
    WebhookDispatchRequest, WebhookDispatchResult, WebhookTransport,
};
use cmdock_server::webhooks::security::WebhookDnsResolver;

fn auth_header(token: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

struct TestEnv {
    server: TestServer,
    _tmp: TempDir,
    state: AppState,
    store: Arc<dyn ConfigStore>,
    token: String,
    admin_token: String,
    user_id: String,
    transport: Arc<FakeWebhookTransport>,
}

const ADMIN_TOKEN: &str = "operator-secret";
fn test_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    for (idx, byte) in key.iter_mut().enumerate() {
        *byte = idx as u8;
    }
    key
}

#[derive(Debug, Clone)]
struct RecordedDispatch {
    url: String,
    signature: String,
    request_id: String,
    body: Value,
}

#[derive(Debug, Default)]
struct FakeWebhookTransport {
    requests: Mutex<Vec<RecordedDispatch>>,
    outcomes: Mutex<Vec<TransportOutcome>>,
}

impl FakeWebhookTransport {
    fn with_outcomes(outcomes: Vec<TransportOutcome>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            outcomes: Mutex::new(outcomes),
        }
    }

    fn recorded(&self) -> Vec<RecordedDispatch> {
        self.requests.lock().unwrap().clone()
    }
}

#[derive(Debug, Clone)]
enum TransportOutcome {
    Success(u16),
    Failure(String),
}

#[derive(Debug, Default)]
struct FakeWebhookDnsResolver {
    hosts: HashMap<String, Vec<IpAddr>>,
}

impl FakeWebhookDnsResolver {
    fn new(hosts: HashMap<String, Vec<IpAddr>>) -> Self {
        Self { hosts }
    }
}

#[async_trait]
impl WebhookDnsResolver for FakeWebhookDnsResolver {
    async fn resolve(&self, host: &str) -> anyhow::Result<Vec<IpAddr>> {
        self.hosts
            .get(host)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no fake DNS record for {host}"))
    }
}

#[async_trait]
impl WebhookTransport for FakeWebhookTransport {
    async fn dispatch(
        &self,
        request: WebhookDispatchRequest,
    ) -> anyhow::Result<WebhookDispatchResult> {
        self.requests.lock().unwrap().push(RecordedDispatch {
            url: request.url,
            signature: request.signature,
            request_id: request.request_id,
            body: serde_json::from_slice(&request.body)?,
        });
        let outcome = self.outcomes.lock().unwrap().pop();
        match outcome.unwrap_or(TransportOutcome::Success(204)) {
            TransportOutcome::Success(status) => Ok(WebhookDispatchResult { status }),
            TransportOutcome::Failure(message) => Err(anyhow::anyhow!(message)),
        }
    }
}

async fn setup() -> TestEnv {
    setup_with_transport_resolver_and_retry_delays(
        Arc::new(FakeWebhookTransport::default()),
        Arc::new(FakeWebhookDnsResolver::new(default_hosts())),
        vec![
            Duration::from_secs(1),
            Duration::from_secs(10),
            Duration::from_secs(60),
        ],
    )
    .await
}

async fn setup_with_transport_and_retry_delays(
    transport: Arc<FakeWebhookTransport>,
    retry_delays: Vec<Duration>,
) -> TestEnv {
    setup_with_transport_resolver_and_retry_delays(
        transport,
        Arc::new(FakeWebhookDnsResolver::new(default_hosts())),
        retry_delays,
    )
    .await
}

fn default_hosts() -> HashMap<String, Vec<IpAddr>> {
    HashMap::from([
        (
            "hooks.example.invalid".to_string(),
            vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
        ),
        (
            "private.example.invalid".to_string(),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        ),
    ])
}

async fn setup_with_transport_resolver_and_retry_delays(
    transport: Arc<FakeWebhookTransport>,
    resolver: Arc<dyn WebhookDnsResolver>,
    retry_delays: Vec<Duration>,
) -> TestEnv {
    setup_full(transport, resolver, retry_delays, None).await
}

/// As [`setup_with_transport_resolver_and_retry_delays`], but pins the async
/// webhook dispatch capacity (#156) so a test can saturate it deterministically.
/// The override MUST be applied before the router captures its `AppState` clone
/// — the handler reads the tracker through that clone's shared `Arc`, so a
/// post-`setup` field swap on `env.state` would not affect requests.
async fn setup_with_dispatch_capacity(capacity: usize) -> TestEnv {
    setup_full(
        Arc::new(FakeWebhookTransport::default()),
        Arc::new(FakeWebhookDnsResolver::new(default_hosts())),
        vec![
            Duration::from_secs(1),
            Duration::from_secs(10),
            Duration::from_secs(60),
        ],
        Some(capacity),
    )
    .await
}

async fn setup_full(
    transport: Arc<FakeWebhookTransport>,
    resolver: Arc<dyn WebhookDnsResolver>,
    retry_delays: Vec<Duration>,
    dispatch_capacity: Option<usize>,
) -> TestEnv {
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
            username: "webhook_user".to_string(),
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

    let mut config = common::test_server_config_with_admin_token(data_dir, ADMIN_TOKEN);
    config.master_key = Some(test_key());
    let mut state = AppState::with_webhook_transport_and_retry_delays(
        store.clone(),
        sqlite_store.clone(),
        &config,
        transport.clone(),
        resolver,
        retry_delays,
    );
    if let Some(capacity) = dispatch_capacity {
        state.webhook_dispatch =
            Arc::new(cmdock_server::webhooks::dispatch::WebhookDispatchTracker::new(capacity));
    }
    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(webhooks::routes())
        .merge(admin::routes())
        .merge(tc_sync::routes())
        .with_state(state.clone())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::PATCH,
                    Method::DELETE,
                ])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    TestEnv {
        server: TestServer::new(app),
        _tmp: tmp,
        state,
        store,
        token,
        admin_token: ADMIN_TOKEN.to_string(),
        user_id: user.id,
        transport,
    }
}

async fn create_webhook(env: &TestEnv, body: Value) -> Value {
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/webhooks")
        .add_header(h, v)
        .json(&body)
        .await;
    resp.assert_status(StatusCode::CREATED);
    resp.json()
}

async fn create_admin_webhook(env: &TestEnv, body: Value) -> Value {
    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .post("/admin/webhooks")
        .add_header(h, v)
        .json(&body)
        .await;
    resp.assert_status(StatusCode::CREATED);
    resp.json()
}

/// Wait for all async webhook dispatch spawned by prior mutations to finish
/// (#149 moved target lookup + delivery off the synchronous response path).
/// Call after each task mutation — before the next mutation or any delivery
/// assertion — so delivery-log assertions (including "no delivery happened")
/// and delivery ordering are deterministic under async dispatch.
async fn settle_webhooks(env: &TestEnv) {
    assert!(
        env.state
            .webhook_dispatch
            .await_quiescent(std::time::Duration::from_secs(10))
            .await,
        "webhook dispatch did not quiesce within 10s"
    );
}

#[tokio::test]
async fn test_webhook_crud_round_trip() {
    let env = setup().await;

    let created = create_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/hooks",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created", "task.modified"],
            "modifiedFields": ["priority"],
            "name": "Ops hook"
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["url"], "https://hooks.example.invalid/hooks");
    assert_eq!(created["enabled"], true);
    assert_eq!(created["name"], "Ops hook");
    assert!(created.get("secret").is_none());

    let (h, v) = auth_header(&env.token);
    let listed: Vec<Value> = env
        .server
        .get("/api/webhooks")
        .add_header(h, v)
        .await
        .json();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], id);

    let (h, v) = auth_header(&env.token);
    let detail: Value = env
        .server
        .get(&format!("/api/webhooks/{id}"))
        .add_header(h, v)
        .await
        .json();
    assert_eq!(detail["id"], id);
    assert_eq!(detail["deliveries"], serde_json::json!([]));

    let (h, v) = auth_header(&env.token);
    let updated = env
        .server
        .put(&format!("/api/webhooks/{id}"))
        .add_header(h, v)
        .json(&serde_json::json!({
            "url": "https://hooks.example.invalid/updated",
            "events": ["task.*"],
            "modifiedFields": ["priority", "status"],
            "name": "Updated hook",
            "enabled": false
        }))
        .await;
    updated.assert_status_ok();
    let updated: Value = updated.json();
    assert_eq!(updated["url"], "https://hooks.example.invalid/updated");
    assert_eq!(updated["enabled"], false);

    let (h, v) = auth_header(&env.token);
    env.server
        .delete(&format!("/api/webhooks/{id}"))
        .add_header(h, v)
        .await
        .assert_status_no_content();
}

#[tokio::test]
async fn test_webhook_validation_errors() {
    let env = setup().await;

    let (h, v) = auth_header(&env.token);
    let invalid_url = env
        .server
        .post("/api/webhooks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "url": "http://127.0.0.1:9/hooks",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created"]
        }))
        .await;
    invalid_url.assert_status_bad_request();
    let body: Value = invalid_url.json();
    assert_eq!(body["code"], "INVALID_URL");

    // Empty field names are rejected; UDA names ("notAField") are accepted
    let invalid_fields = env
        .server
        .post("/api/webhooks")
        .add_header(h, v)
        .json(&serde_json::json!({
            "url": "https://hooks.example.invalid/hooks",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.modified"],
            "modifiedFields": [""]
        }))
        .await;
    invalid_fields.assert_status_bad_request();
    let body: Value = invalid_fields.json();
    assert_eq!(body["code"], "INVALID_MODIFIED_FIELDS");

    let private_dns = env
        .server
        .post("/api/webhooks")
        .add_header(auth_header(&env.token).0, auth_header(&env.token).1)
        .json(&serde_json::json!({
            "url": "https://private.example.invalid/hooks",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created"]
        }))
        .await;
    private_dns.assert_status_bad_request();
    let body: Value = private_dns.json();
    assert_eq!(body["code"], "INVALID_URL");
}

#[tokio::test]
async fn test_task_events_record_delivery_logs_and_modified_field_filtering() {
    let env = setup().await;

    let created = create_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/hooks",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created", "task.modified"],
            "modifiedFields": ["priority"]
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap();

    let (h, v) = auth_header(&env.token);
    let add = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+test create webhook fixture"}))
        .await;
    add.assert_status_ok();
    let body: Value = add.json();
    let uuid = body["output"]
        .as_str()
        .unwrap()
        .trim_start_matches("Created task ")
        .trim_end_matches('.')
        .to_string();
    settle_webhooks(&env).await;

    let (h, v) = auth_header(&env.token);
    let detail: Value = env
        .server
        .get(&format!("/api/webhooks/{id}"))
        .add_header(h.clone(), v.clone())
        .await
        .json();
    let deliveries = detail["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0]["event"], "task.created");
    assert_eq!(deliveries[0]["status"], "delivered");

    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"description": "only description changed"}))
        .await
        .assert_status_ok();
    settle_webhooks(&env).await;

    let detail: Value = env
        .server
        .get(&format!("/api/webhooks/{id}"))
        .add_header(h.clone(), v.clone())
        .await
        .json();
    assert_eq!(detail["deliveries"].as_array().unwrap().len(), 1);

    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"priority": "H"}))
        .await
        .assert_status_ok();
    settle_webhooks(&env).await;

    let (h, v) = auth_header(&env.token);
    let detail: Value = env
        .server
        .get(&format!("/api/webhooks/{id}"))
        .add_header(h, v)
        .await
        .json();
    let deliveries = detail["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 2);
    assert_eq!(deliveries[0]["event"], "task.modified");
    assert_eq!(deliveries[1]["event"], "task.created");
}

#[tokio::test]
async fn test_test_endpoint_records_webhook_test_delivery() {
    let env = setup().await;

    let created = create_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/hooks",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created"]
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap();

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/webhooks/{id}/test"))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["delivery"]["event"], "webhook.test");
    assert_eq!(body["delivery"]["status"], "delivered");

    let recorded = env.transport.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].body["event"], "webhook.test");
    assert!(
        recorded[0].body.get("task_scope_id").is_none(),
        "synthetic webhook.test payloads must remain unscoped"
    );
}

#[tokio::test]
async fn test_admin_webhooks_receive_task_events_and_can_be_disabled() {
    let env = setup().await;

    let created = create_admin_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/admin",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created"],
            "name": "Global ops hook"
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    let (h, v) = auth_header(&env.admin_token);
    let listed: Vec<Value> = env
        .server
        .get("/admin/webhooks")
        .add_header(h.clone(), v.clone())
        .await
        .json();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], id);

    env.server
        .post("/api/tasks")
        .add_header(auth_header(&env.token).0, auth_header(&env.token).1)
        .json(&serde_json::json!({"raw": "+adminhook first"}))
        .await
        .assert_status_ok();
    settle_webhooks(&env).await;

    let detail: Value = env
        .server
        .get(&format!("/admin/webhooks/{id}"))
        .add_header(h.clone(), v.clone())
        .await
        .json();
    assert_eq!(detail["deliveries"].as_array().unwrap().len(), 1);
    assert_eq!(detail["deliveries"][0]["event"], "task.created");

    let recorded = env.transport.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].body["event"], "task.created");
    assert_eq!(recorded[0].body["user_id"], env.user_id);

    let disabled: Value = env
        .server
        .patch(&format!("/admin/webhooks/{id}"))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"enabled": false}))
        .await
        .json();
    assert_eq!(disabled["enabled"], false);

    env.server
        .post("/api/tasks")
        .add_header(auth_header(&env.token).0, auth_header(&env.token).1)
        .json(&serde_json::json!({"raw": "+adminhook second"}))
        .await
        .assert_status_ok();
    settle_webhooks(&env).await;

    let detail: Value = env
        .server
        .get(&format!("/admin/webhooks/{id}"))
        .add_header(h.clone(), v.clone())
        .await
        .json();
    assert_eq!(detail["deliveries"].as_array().unwrap().len(), 1);

    let test_response: Value = env
        .server
        .post(&format!("/admin/webhooks/{id}/test"))
        .add_header(h.clone(), v.clone())
        .await
        .json();
    assert_eq!(test_response["delivery"]["event"], "webhook.test");
    let recorded = env.transport.recorded();
    let test_delivery = recorded
        .iter()
        .find(|record| record.body["event"] == "webhook.test")
        .expect("admin webhook.test delivery should be dispatched");
    assert!(
        test_delivery.body.get("task_scope_id").is_none(),
        "synthetic admin webhook.test payloads must remain unscoped"
    );

    env.server
        .delete(&format!("/admin/webhooks/{id}"))
        .add_header(h, v)
        .await
        .assert_status_no_content();
}

#[tokio::test]
async fn test_sync_completed_delivery_payload_and_no_change_short_circuit() {
    let env = setup().await;
    let personal_scope = env
        .store
        .get_personal_task_scope_for_user(&env.user_id)
        .await
        .unwrap()
        .expect("test user should have a Personal Task Scope");

    let created = create_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/sync",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["sync.completed"]
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    cmdock_server::webhooks::delivery::emit_sync_event(
        &env.state,
        &env.user_id,
        cmdock_server::store::models::WebhookSyncSummary {
            tasks_changed: 3,
            created: 1,
            completed: 1,
            deleted: 0,
            modified: 1,
        },
        Some("req_sync_1".to_string()),
    )
    .await;

    let detail: Value = env
        .server
        .get(&format!("/api/webhooks/{id}"))
        .add_header(auth_header(&env.token).0, auth_header(&env.token).1)
        .await
        .json();
    assert_eq!(detail["deliveries"].as_array().unwrap().len(), 1);
    assert_eq!(detail["deliveries"][0]["event"], "sync.completed");

    let recorded = env.transport.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].body["event"], "sync.completed");
    assert_eq!(
        recorded[0].body["task_scope_id"].as_str(),
        Some(personal_scope.id.as_str())
    );
    assert_eq!(
        recorded[0].body["sync"],
        serde_json::json!({
            "tasks_changed": 3,
            "created": 1,
            "completed": 1,
            "deleted": 0,
            "modified": 1
        })
    );
    assert_eq!(recorded[0].body["request_id"], "req_sync_1");

    cmdock_server::webhooks::delivery::emit_sync_event(
        &env.state,
        &env.user_id,
        cmdock_server::store::models::WebhookSyncSummary {
            tasks_changed: 0,
            created: 0,
            completed: 0,
            deleted: 0,
            modified: 0,
        },
        Some("req_sync_2".to_string()),
    )
    .await;

    let detail: Value = env
        .server
        .get(&format!("/api/webhooks/{id}"))
        .add_header(auth_header(&env.token).0, auth_header(&env.token).1)
        .await
        .json();
    assert_eq!(detail["deliveries"].as_array().unwrap().len(), 1);
    assert_eq!(env.transport.recorded().len(), 1);
}

#[tokio::test]
async fn test_time_driven_webhook_payloads_include_task_scope() {
    let env = setup().await;
    let personal_scope = env
        .store
        .get_personal_task_scope_for_user(&env.user_id)
        .await
        .unwrap()
        .expect("test user should have a Personal Task Scope");

    create_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/time-driven",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.due", "task.overdue"]
        }),
    )
    .await;

    let now = Utc::now();
    let due_soon = (now + ChronoDuration::hours(1))
        .format("%Y%m%dT%H%M%SZ")
        .to_string();
    let overdue = (now - ChronoDuration::hours(1))
        .format("%Y%m%dT%H%M%SZ")
        .to_string();

    let (h, v) = auth_header(&env.token);
    let due_add = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+test due webhook scope"}))
        .await;
    due_add.assert_status_ok();
    let due_body: Value = due_add.json();
    let due_uuid = due_body["output"]
        .as_str()
        .unwrap()
        .trim_start_matches("Created task ")
        .trim_end_matches('.')
        .to_string();
    env.server
        .post(&format!("/api/tasks/{due_uuid}/modify"))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"due": due_soon}))
        .await
        .assert_status_ok();

    let overdue_add = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+test overdue webhook scope"}))
        .await;
    overdue_add.assert_status_ok();
    let overdue_body: Value = overdue_add.json();
    let overdue_uuid = overdue_body["output"]
        .as_str()
        .unwrap()
        .trim_start_matches("Created task ")
        .trim_end_matches('.')
        .to_string();
    env.server
        .post(&format!("/api/tasks/{overdue_uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({"due": overdue}))
        .await
        .assert_status_ok();

    cmdock_server::webhooks::scheduler::poll_once(&env.state, now)
        .await
        .unwrap();

    let recorded = env.transport.recorded();
    assert_eq!(recorded.len(), 2);
    let by_event: HashMap<_, _> = recorded
        .iter()
        .map(|record| (record.body["event"].as_str().unwrap().to_string(), record))
        .collect();
    for event in ["task.due", "task.overdue"] {
        let record = by_event
            .get(event)
            .unwrap_or_else(|| panic!("missing {event} delivery"));
        assert_eq!(
            record.body["task_scope_id"].as_str(),
            Some(personal_scope.id.as_str()),
            "{event} payload must carry the Personal Task Scope id"
        );
    }
}

#[tokio::test]
async fn test_delivery_rejects_stored_webhook_that_resolves_private() {
    let resolver = Arc::new(FakeWebhookDnsResolver::new(HashMap::from([(
        "pivot.example.invalid".to_string(),
        vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
    )])));
    let env = setup_with_transport_resolver_and_retry_delays(
        Arc::new(FakeWebhookTransport::default()),
        resolver,
        vec![Duration::ZERO, Duration::ZERO, Duration::ZERO],
    )
    .await;

    let user = env
        .store
        .get_user_by_token(&env.token)
        .await
        .unwrap()
        .unwrap();
    let secret_enc = base64::engine::general_purpose::STANDARD.encode(
        crypto::encrypt_secret(b"abcdefghijklmnopqrstuvwxyz0123456789", &test_key()).unwrap(),
    );
    let webhook = env
        .store
        .create_webhook(
            &cmdock_server::store::models::NewWebhookRecord {
                id: "wh_testprivate".to_string(),
                user_id: user.id,
                url: "https://pivot.example.invalid/hook".to_string(),
                events: vec!["task.created".to_string()],
                modified_fields: None,
                name: Some("Injected private".to_string()),
                enabled: true,
                secret_enc,
            },
            100,
        )
        .await
        .unwrap();

    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post(&format!("/api/webhooks/{}/test", webhook.id))
        .add_header(h.clone(), v.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["delivery"]["status"], "failed");
    assert!(body["delivery"]["failureReason"]
        .as_str()
        .unwrap()
        .contains("private or local address"));
    assert!(
        env.transport.recorded().is_empty(),
        "transport should not run for SSRF-blocked deliveries"
    );
}

#[tokio::test]
async fn test_successful_delivery_records_signature_and_payload() {
    let env = setup().await;

    let created = create_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/cmdock",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created"]
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap();

    let (h, v) = auth_header(&env.token);
    let add = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+test webhook success path"}))
        .await;
    add.assert_status_ok();
    settle_webhooks(&env).await;

    let detail: Value = env
        .server
        .get(&format!("/api/webhooks/{id}"))
        .add_header(h, v)
        .await
        .json();
    let deliveries = detail["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0]["status"], "delivered");
    assert_eq!(deliveries[0]["responseStatus"], 204);

    let recorded = env.transport.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].url, "https://hooks.example.invalid/cmdock");
    assert!(recorded[0].signature.as_str().starts_with("sha256="));
    assert!(recorded[0].request_id.as_str().starts_with("req_"));
    assert_eq!(recorded[0].body["event"], "task.created");
    assert!(recorded[0].body["delivery_id"]
        .as_str()
        .unwrap()
        .starts_with("del_"));
}

#[tokio::test]
async fn test_delivery_retries_preserve_event_id_and_attempt_history() {
    let transport = Arc::new(FakeWebhookTransport::with_outcomes(vec![
        TransportOutcome::Success(204),
        TransportOutcome::Failure("temporary transport failure".to_string()),
        TransportOutcome::Success(503),
    ]));
    let env = setup_with_transport_and_retry_delays(
        transport.clone(),
        vec![Duration::ZERO, Duration::ZERO, Duration::ZERO],
    )
    .await;

    let created = create_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/retry",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created"]
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap();

    let (h, v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+test webhook retry path"}))
        .await
        .assert_status_ok();
    settle_webhooks(&env).await;

    let detail: Value = env
        .server
        .get(&format!("/api/webhooks/{id}"))
        .add_header(h, v)
        .await
        .json();
    let deliveries = detail["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 3);
    let mut attempts: Vec<u64> = deliveries
        .iter()
        .map(|delivery| delivery["attempt"].as_u64().unwrap())
        .collect();
    attempts.sort_unstable();
    assert_eq!(attempts, vec![1, 2, 3]);

    let recorded = transport.recorded();
    assert_eq!(recorded.len(), 3);
    let event_ids: std::collections::HashSet<_> = recorded
        .iter()
        .map(|request| request.body["event_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(event_ids.len(), 1);

    let delivery_ids: std::collections::HashSet<_> = recorded
        .iter()
        .map(|request| request.body["delivery_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(delivery_ids.len(), 3);
    assert_eq!(
        detail["consecutiveFailures"].as_u64().unwrap(),
        0,
        "successful retry should reset logical failure count"
    );
}

#[tokio::test]
async fn test_retries_increment_consecutive_failures_once_per_logical_delivery() {
    let transport = Arc::new(FakeWebhookTransport::with_outcomes(vec![
        TransportOutcome::Failure("attempt 12".to_string()),
        TransportOutcome::Failure("attempt 11".to_string()),
        TransportOutcome::Failure("attempt 10".to_string()),
        TransportOutcome::Failure("attempt 9".to_string()),
        TransportOutcome::Failure("attempt 8".to_string()),
        TransportOutcome::Failure("attempt 7".to_string()),
        TransportOutcome::Failure("attempt 6".to_string()),
        TransportOutcome::Failure("attempt 5".to_string()),
        TransportOutcome::Failure("attempt 4".to_string()),
        TransportOutcome::Failure("attempt 3".to_string()),
        TransportOutcome::Failure("attempt 2".to_string()),
        TransportOutcome::Failure("attempt 1".to_string()),
    ]));
    let env = setup_with_transport_and_retry_delays(
        transport,
        vec![Duration::ZERO, Duration::ZERO, Duration::ZERO],
    )
    .await;

    let created = create_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/failure",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created"]
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap();

    let (h, v) = auth_header(&env.token);
    for idx in 0..3 {
        let response = env
            .server
            .post(&format!("/api/webhooks/{id}/test"))
            .add_header(h.clone(), v.clone())
            .await;
        response.assert_status_ok();
        let body: Value = response.json();
        assert_eq!(body["delivery"]["status"], "failed");
        assert_eq!(
            body["delivery"]["attempt"], 4,
            "final attempt should be returned"
        );
        assert_eq!(
            body["delivery"]["failureReason"].as_str().unwrap(),
            format!("attempt {}", (idx + 1) * 4)
        );
    }

    let detail: Value = env
        .server
        .get(&format!("/api/webhooks/{id}"))
        .add_header(h, v)
        .await
        .json();
    assert_eq!(detail["consecutiveFailures"], 3);
    assert_eq!(detail["enabled"], true);
    assert_eq!(detail["deliveries"].as_array().unwrap().len(), 12);
}

#[tokio::test]
async fn test_delivery_write_purges_logs_older_than_default_retention() {
    let env = setup().await;

    let created = create_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/retention",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created"]
        }),
    )
    .await;
    let webhook_id = created["id"].as_str().unwrap().to_string();

    env.store
        .record_webhook_delivery(&cmdock_server::store::models::WebhookDeliveryRecord {
            delivery_id: "del_oldretention".to_string(),
            webhook_id: webhook_id.clone(),
            event_id: "evt_oldretention".to_string(),
            event: "task.created".to_string(),
            timestamp: "2026-03-01T00:00:00Z".to_string(),
            status: "failed".to_string(),
            response_status: None,
            attempt: 1,
            failure_reason: Some("stale".to_string()),
        })
        .await
        .unwrap();

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/webhooks/{webhook_id}/test"))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status_ok();

    let detail: Value = env
        .server
        .get(&format!("/api/webhooks/{webhook_id}"))
        .add_header(h, v)
        .await
        .json();
    let deliveries = detail["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 1);
    assert_ne!(deliveries[0]["deliveryId"], "del_oldretention");
}

/// Regression for cmdock/server#105: a `modifiedFields: ["project"]` filter
/// must trigger a `task.modified` delivery when an explicit JSON `null`
/// clears the project. Before the retrofit, `{"project": null}` was a
/// silent no-op and produced no delivery; after the retrofit it produces
/// a `project` field-level change and the filtered delivery fires.
#[tokio::test]
async fn test_modified_fields_filter_fires_on_null_clears_retrofit() {
    let env = setup().await;

    let created = create_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/hooks",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.modified"],
            "modifiedFields": ["project"]
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap();

    // Create the task with a project set so we have something to clear.
    let (h, v) = auth_header(&env.token);
    let add = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "project:WORK +test Clear via null"}))
        .await;
    add.assert_status_ok();
    let body: Value = add.json();
    let uuid = body["output"]
        .as_str()
        .unwrap()
        .trim_start_matches("Created task ")
        .trim_end_matches('.')
        .to_string();
    settle_webhooks(&env).await;

    // First modify: change description only — project unchanged. Should NOT
    // produce a delivery (the modifiedFields filter excludes it).
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"description": "only description changed"}))
        .await
        .assert_status_ok();
    // Settle so the negative assertion below (zero deliveries) is deterministic
    // under async dispatch — the dispatch is spawned even when it produces no
    // delivery, so we must wait for it to finish before asserting zero.
    settle_webhooks(&env).await;

    let detail: Value = env
        .server
        .get(&format!("/api/webhooks/{id}"))
        .add_header(h.clone(), v.clone())
        .await
        .json();
    assert_eq!(
        detail["deliveries"].as_array().unwrap().len(),
        0,
        "description-only modify must not fire a project-filtered webhook"
    );

    // Second modify: explicit null on project — clears it. After #105 this
    // is a real field-level change and must fire the project-filtered
    // delivery.
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"project": null}))
        .await
        .assert_status_ok();
    settle_webhooks(&env).await;

    let (h, v) = auth_header(&env.token);
    let detail: Value = env
        .server
        .get(&format!("/api/webhooks/{id}"))
        .add_header(h, v)
        .await
        .json();
    let deliveries = detail["deliveries"].as_array().unwrap();
    assert_eq!(
        deliveries.len(),
        1,
        "null-clears on project must fire one task.modified delivery against \
         the project-filtered webhook"
    );
    assert_eq!(deliveries[0]["event"], "task.modified");
}

#[tokio::test]
async fn test_webhook_payload_includes_task_key_and_task_scope_for_create_modify_complete_delete() {
    // Regression lock for #130 Phase 3 + task-scope S7: webhook payloads carry
    // the canonical task `key` on the task snapshot and the logical
    // `task_scope_id` event-envelope field on task.created, task.modified,
    // task.completed, and task.deleted events. The key surfaces from
    // `task_to_item`'s optional
    // key map; service.rs builds the task_item with the committed allocation
    // row's canonical key. If a future refactor accidentally drops the key map
    // or Task Scope envelope lookup on a mutation path, this test fires.
    let env = setup().await;
    let personal_scope = env
        .store
        .get_personal_task_scope_for_user(&env.user_id)
        .await
        .unwrap()
        .expect("test user should have a Personal Task Scope");

    let created = create_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/key-regression",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created", "task.modified", "task.completed", "task.deleted"]
        }),
    )
    .await;
    assert_eq!(created["enabled"], true);

    // 1. task.created
    let (h, v) = auth_header(&env.token);
    let add = env
        .server
        .post("/api/tasks")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"raw": "+test webhook key payload"}))
        .await;
    add.assert_status_ok();
    let add_body: Value = add.json();
    let uuid = add_body["output"]
        .as_str()
        .unwrap()
        .strip_prefix("Created task ")
        .unwrap()
        .strip_suffix('.')
        .unwrap()
        .to_string();
    let expected_key = add_body["key"].as_str().unwrap().to_string();

    // 2. task.modified
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({"description": "Updated"}))
        .await
        .assert_status_ok();

    // 3. task.completed
    env.server
        .post(&format!("/api/tasks/{uuid}/done"))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status_ok();

    // 4. task.deleted
    env.server
        .post(&format!("/api/tasks/{uuid}/delete"))
        .add_header(h, v)
        .await
        .assert_status_ok();

    // Wait for all four spawned dispatches to finish before asserting the set
    // (#149: delivery is async; this assert is order-independent — grouped by
    // event below — so one settle covers all four).
    settle_webhooks(&env).await;
    let recorded = env.transport.recorded();
    assert_eq!(
        recorded.len(),
        4,
        "expected 4 deliveries (created, modified, completed, deleted)"
    );

    let mut by_event: HashMap<String, &RecordedDispatch> = HashMap::new();
    for r in &recorded {
        let event = r.body["event"].as_str().unwrap().to_string();
        by_event.insert(event, r);
    }
    for event in &[
        "task.created",
        "task.modified",
        "task.completed",
        "task.deleted",
    ] {
        let r = by_event
            .get(*event)
            .unwrap_or_else(|| panic!("missing webhook delivery for {event}"));
        assert_eq!(
            r.body["task_scope_id"].as_str(),
            Some(personal_scope.id.as_str()),
            "{event} payload must carry the Personal Task Scope id"
        );
        let task_obj = &r.body["task"];
        assert!(
            task_obj.is_object(),
            "{event} payload missing `task` object"
        );
        let actual_key = task_obj["key"].as_str().unwrap_or_else(|| {
            panic!("{event} payload `task` missing `key` field — regression for #130 Phase 3")
        });
        assert_eq!(
            actual_key, expected_key,
            "{event} payload key must match canonical allocation"
        );
    }
}

/// #156: when async webhook dispatch is at capacity, `finalize_success` SHEDS
/// the event — a permanent drop (no retry, no delivery), distinct from a slow
/// delivery that eventually lands. Capacity 1 + one held permit simulates a
/// single in-flight dispatch occupying the only slot; the next mutation must
/// shed without ever reaching the transport. A positive control (slot freed →
/// delivery resumes) proves the drop was capacity-driven, not a broken pipeline.
#[tokio::test]
async fn test_webhook_dispatch_sheds_at_capacity_without_delivery() {
    let env = setup_with_dispatch_capacity(1).await;

    create_webhook(
        &env,
        serde_json::json!({
            "url": "https://hooks.example.invalid/hooks",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created"],
        }),
    )
    .await;

    // Occupy the single dispatch slot — a faithful stand-in for an in-flight
    // delivery that has not yet completed. Held across the shedding mutation.
    let hold = env
        .state
        .webhook_dispatch
        .try_enter()
        .expect("capacity-1 tracker admits the first entry");
    assert_eq!(
        env.state.webhook_dispatch.in_flight(),
        1,
        "the only dispatch slot is occupied"
    );

    // With dispatch at capacity, a create mutation sheds its webhook event. The
    // mutation itself still succeeds — shedding is off the correctness path.
    let (h, v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+shed shed me"}))
        .await
        .assert_status_ok();

    // The shed is synchronous within `finalize_success`, so by the time the 2xx
    // returns the event is already dropped — no extra permit was taken.
    assert_eq!(
        env.state.webhook_dispatch.in_flight(),
        1,
        "a shed must not consume a permit beyond the held one"
    );

    // Free the slot and quiesce; the shed event left nothing to deliver.
    drop(hold);
    settle_webhooks(&env).await;
    assert!(
        env.transport.recorded().is_empty(),
        "a shed event must never reach the transport, got {:?}",
        env.transport.recorded()
    );

    // Positive control: with the slot free a fresh create dispatches and
    // delivers — confirming the earlier drop was capacity, not a dead pipeline.
    let (h, v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+shed deliver me"}))
        .await
        .assert_status_ok();
    settle_webhooks(&env).await;
    assert_eq!(
        env.transport.recorded().len(),
        1,
        "with capacity free the next create must be delivered"
    );
}

/// #155: the per-user webhook cap (`MAX_WEBHOOKS_PER_USER`) is enforced
/// ATOMICALLY inside `create_webhook` (count + insert on the single writer
/// lane), replacing the old handler-side count-then-insert precheck (a TOCTOU
/// that could let concurrent creates overshoot the cap). This is an
/// invariant-lock, not a race reproducer: the single writer serializes creates,
/// so the discriminating assertion is that exactly `MAX` rows persist (never
/// `MAX + 1`) no matter how the concurrent creates interleave.
#[tokio::test(flavor = "multi_thread")]
async fn test_webhook_cap_holds_at_n_under_concurrent_creates() {
    let env = setup().await;
    let max = cmdock_server::webhooks::api::MAX_WEBHOOKS_PER_USER;
    let over = 4usize;

    // Distinct URLs (same resolvable host, different paths) so over-cap creates
    // reject with LIMIT_REACHED, not the (user_id, url) UNIQUE → DUPLICATE_URL.
    let mut handles = Vec::new();
    for i in 0..(max + over) {
        let (h, v) = auth_header(&env.token);
        handles.push(
            env.server
                .post("/api/webhooks")
                .add_header(h, v)
                .json(&serde_json::json!({
                    "url": format!("https://hooks.example.invalid/cap-{i}"),
                    "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
                    "events": ["task.created"]
                })),
        );
    }

    let results = futures::future::join_all(
        handles
            .into_iter()
            .map(std::future::IntoFuture::into_future),
    )
    .await;

    let mut created = 0usize;
    let mut limit_reached = 0usize;
    for resp in &results {
        match resp.status_code() {
            StatusCode::CREATED => created += 1,
            StatusCode::UNPROCESSABLE_ENTITY => {
                limit_reached += 1;
                let body: Value = resp.json();
                assert_eq!(
                    body["code"], "LIMIT_REACHED",
                    "over-cap creates must reject with LIMIT_REACHED, not e.g. DUPLICATE_URL"
                );
            }
            other => panic!("unexpected status {other} from concurrent webhook create"),
        }
    }

    assert_eq!(created, max, "exactly the cap may be created");
    assert_eq!(
        limit_reached, over,
        "every create past the cap must be rejected as LIMIT_REACHED"
    );

    // Discriminating invariant: the persisted count is EXACTLY the cap — not
    // max+1 (a TOCTOU overshoot the old precheck allowed), not fewer.
    let stored = env.store.list_webhooks(&env.user_id).await.unwrap();
    assert_eq!(
        stored.len(),
        max,
        "stored webhook count must equal the cap exactly"
    );
}

/// #155 admin-path counterpart: the global admin-webhook cap is enforced
/// atomically inside `create_admin_webhook` (separate SQL path +
/// `map_admin_store_error`), so concurrent admin creates also hold at exactly N.
#[tokio::test(flavor = "multi_thread")]
async fn test_admin_webhook_cap_holds_at_n_under_concurrent_creates() {
    let env = setup().await;
    let max = cmdock_server::webhooks::api::MAX_WEBHOOKS_PER_USER;
    let over = 4usize;

    let mut handles = Vec::new();
    for i in 0..(max + over) {
        let (h, v) = auth_header(&env.admin_token);
        handles.push(env.server.post("/admin/webhooks").add_header(h, v).json(
            &serde_json::json!({
                "url": format!("https://hooks.example.invalid/admin-cap-{i}"),
                "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
                "events": ["task.created"]
            }),
        ));
    }

    let results = futures::future::join_all(
        handles
            .into_iter()
            .map(std::future::IntoFuture::into_future),
    )
    .await;

    let mut created = 0usize;
    let mut limit_reached = 0usize;
    for resp in &results {
        match resp.status_code() {
            StatusCode::CREATED => created += 1,
            StatusCode::UNPROCESSABLE_ENTITY => {
                limit_reached += 1;
                let body: Value = resp.json();
                assert_eq!(body["code"], "LIMIT_REACHED");
            }
            other => panic!("unexpected status {other} from concurrent admin webhook create"),
        }
    }

    assert_eq!(created, max, "exactly the cap may be created");
    assert_eq!(
        limit_reached, over,
        "every admin create past the cap is LIMIT_REACHED"
    );
    let stored = env.store.list_admin_webhooks().await.unwrap();
    assert_eq!(
        stored.len(),
        max,
        "stored admin webhook count must equal the cap exactly"
    );
}

/// #155 precedence: enforcement moved from a handler precheck (BEFORE
/// `normalize_registration`) to the store (AFTER). So a request that is both
/// over-cap AND malformed now fails URL validation first — `INVALID_URL` wins
/// over `LIMIT_REACHED`. This locks the intentional behaviour change.
#[tokio::test]
async fn test_over_cap_with_invalid_url_returns_invalid_url_not_limit_reached() {
    let env = setup().await;
    let max = cmdock_server::webhooks::api::MAX_WEBHOOKS_PER_USER;

    for i in 0..max {
        create_webhook(
            &env,
            serde_json::json!({
                "url": format!("https://hooks.example.invalid/fill-{i}"),
                "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
                "events": ["task.created"]
            }),
        )
        .await;
    }

    // At cap AND an SSRF/invalid URL: validation runs before the cap check.
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/webhooks")
        .add_header(h, v)
        .json(&serde_json::json!({
            "url": "http://127.0.0.1:9/hooks",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created"]
        }))
        .await;
    resp.assert_status_bad_request();
    let body: Value = resp.json();
    assert_eq!(
        body["code"], "INVALID_URL",
        "URL validation must precede the store-side cap check (#155 precedence flip)"
    );
}
