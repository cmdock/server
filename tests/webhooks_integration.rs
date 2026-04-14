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
            username: "webhook_user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();
    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();
    std::fs::create_dir_all(tmp.path().join("users").join(&user.id)).unwrap();

    let mut config = common::test_server_config_with_admin_token(data_dir, ADMIN_TOKEN);
    config.master_key = Some(test_key());
    let state = AppState::with_webhook_transport_and_retry_delays(
        store.clone(),
        &config,
        transport.clone(),
        resolver,
        retry_delays,
    );
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

    let invalid_fields = env
        .server
        .post("/api/webhooks")
        .add_header(h, v)
        .json(&serde_json::json!({
            "url": "https://hooks.example.invalid/hooks",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.modified"],
            "modifiedFields": ["notAField"]
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

    env.server
        .delete(&format!("/admin/webhooks/{id}"))
        .add_header(h, v)
        .await
        .assert_status_no_content();
}

#[tokio::test]
async fn test_sync_completed_delivery_payload_and_no_change_short_circuit() {
    let env = setup().await;

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
        .create_webhook(&cmdock_server::store::models::NewWebhookRecord {
            id: "wh_testprivate".to_string(),
            user_id: user.id,
            url: "https://pivot.example.invalid/hook".to_string(),
            events: vec!["task.created".to_string()],
            modified_fields: None,
            name: Some("Injected private".to_string()),
            enabled: true,
            secret_enc,
        })
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
