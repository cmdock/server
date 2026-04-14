mod common;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use axum::http::{header, HeaderValue};
use axum::Router;
use chrono::{Duration, TimeZone, Utc};
use cmdock_server::app_state::AppState;
use cmdock_server::health;
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::tasks;
use cmdock_server::webhooks;
use cmdock_server::webhooks::delivery::{
    WebhookDispatchRequest, WebhookDispatchResult, WebhookTransport,
};
use cmdock_server::webhooks::scheduler;
use cmdock_server::webhooks::security::WebhookDnsResolver;
use serde_json::Value;
use taskchampion::{Operations, Status};
use tempfile::TempDir;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

#[derive(Debug, Clone)]
struct RecordedDispatch {
    event: String,
}

#[derive(Debug, Default)]
struct FakeWebhookTransport {
    requests: Mutex<Vec<RecordedDispatch>>,
}

impl FakeWebhookTransport {
    fn events(&self, event: &str) -> Vec<RecordedDispatch> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.event == event)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl WebhookTransport for FakeWebhookTransport {
    async fn dispatch(
        &self,
        request: WebhookDispatchRequest,
    ) -> anyhow::Result<WebhookDispatchResult> {
        let body: Value = serde_json::from_slice(&request.body)?;
        self.requests.lock().unwrap().push(RecordedDispatch {
            event: body["event"].as_str().unwrap_or_default().to_string(),
        });
        Ok(WebhookDispatchResult { status: 204 })
    }
}

#[derive(Debug)]
struct FakeWebhookDnsResolver;

#[async_trait]
impl WebhookDnsResolver for FakeWebhookDnsResolver {
    async fn resolve(&self, host: &str) -> anyhow::Result<Vec<IpAddr>> {
        Ok(HashMap::from([(
            "hooks.example.invalid".to_string(),
            vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
        )])
        .remove(host)
        .unwrap_or_default())
    }
}

struct TestEnv {
    state: AppState,
    server: axum_test::TestServer,
    user_id: String,
    _tmp: TempDir,
    token: String,
    transport: Arc<FakeWebhookTransport>,
}

fn auth_header(token: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

fn test_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = (i as u8).wrapping_mul(17).wrapping_add(11);
    }
    key
}

fn tw_due(dt: chrono::DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

async fn setup() -> TestEnv {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();
    let db_path = data_dir.join("config.sqlite");
    let store = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();
    let user = store
        .create_user(&NewUser {
            username: "scheduler_user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();
    let token = store
        .create_api_token(&user.id, Some("default"))
        .await
        .unwrap();

    std::fs::create_dir_all(data_dir.join("users").join(&user.id)).unwrap();

    let config = common::test_server_config_with_master_key(data_dir, test_key());
    let transport = Arc::new(FakeWebhookTransport::default());
    let state = AppState::with_webhook_transport_and_retry_delays(
        store.clone(),
        &config,
        transport.clone(),
        Arc::new(FakeWebhookDnsResolver),
        vec![
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(60),
        ],
    );
    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(webhooks::routes())
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    TestEnv {
        state,
        server: axum_test::TestServer::new(app),
        user_id: user.id,
        _tmp: tmp,
        token,
        transport,
    }
}

async fn create_webhook(env: &TestEnv, events: &[&str]) {
    let (h, v) = auth_header(&env.token);
    env.server
        .post("/api/webhooks")
        .add_header(h, v)
        .json(&serde_json::json!({
            "url": "https://hooks.example.invalid/cmdock",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": events,
        }))
        .await
        .assert_status(StatusCode::CREATED);
}

async fn create_task(env: &TestEnv, raw: &str) -> Uuid {
    let (h, v) = auth_header(&env.token);
    let response = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({ "raw": raw }))
        .await;
    response.assert_status_ok();
    let body: Value = response.json();
    let output = body["output"].as_str().unwrap();
    Uuid::parse_str(
        output
            .split_whitespace()
            .last()
            .unwrap()
            .trim_end_matches('.'),
    )
    .unwrap()
}

#[tokio::test]
async fn test_scheduler_emits_task_due_once_per_due_date() {
    let env = setup().await;
    let now = Utc.with_ymd_and_hms(2026, 4, 7, 8, 0, 0).unwrap();
    create_webhook(&env, &["task.due"]).await;
    let due = now + Duration::hours(1);
    create_task(&env, &format!("due:{} scheduler due task", tw_due(due))).await;

    scheduler::poll_once(&env.state, now).await.unwrap();
    scheduler::poll_once(&env.state, now + Duration::minutes(1))
        .await
        .unwrap();

    assert_eq!(env.transport.events("task.due").len(), 1);
}

#[tokio::test]
async fn test_scheduler_emits_task_overdue_once_per_due_date() {
    let env = setup().await;
    let now = Utc.with_ymd_and_hms(2026, 4, 7, 8, 0, 0).unwrap();
    create_webhook(&env, &["task.overdue"]).await;
    let due = now - Duration::hours(1);
    create_task(&env, &format!("due:{} scheduler overdue task", tw_due(due))).await;

    scheduler::poll_once(&env.state, now).await.unwrap();
    scheduler::poll_once(&env.state, now + Duration::minutes(1))
        .await
        .unwrap();

    assert_eq!(env.transport.events("task.overdue").len(), 1);
}

#[tokio::test]
async fn test_scheduler_rearms_after_due_date_change() {
    let env = setup().await;
    let now = Utc.with_ymd_and_hms(2026, 4, 7, 8, 0, 0).unwrap();
    create_webhook(&env, &["task.due"]).await;
    let due = now + Duration::hours(1);
    let uuid = create_task(
        &env,
        &format!("due:{} scheduler due change task", tw_due(due)),
    )
    .await;

    scheduler::poll_once(&env.state, now).await.unwrap();

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/modify"))
        .add_header(h, v)
        .json(&serde_json::json!({
            "due": tw_due(now + Duration::hours(2))
        }))
        .await
        .assert_status_ok();

    scheduler::poll_once(&env.state, now + Duration::minutes(1))
        .await
        .unwrap();

    assert_eq!(env.transport.events("task.due").len(), 2);
}

#[tokio::test]
async fn test_scheduler_rearms_after_complete_and_undo() {
    let env = setup().await;
    let now = Utc.with_ymd_and_hms(2026, 4, 7, 8, 0, 0).unwrap();
    create_webhook(&env, &["task.due"]).await;
    let due = now + Duration::hours(1);
    let uuid = create_task(&env, &format!("due:{} scheduler undo task", tw_due(due))).await;

    scheduler::poll_once(&env.state, now).await.unwrap();

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/done"))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status_ok();
    env.server
        .post(&format!("/api/tasks/{uuid}/undo"))
        .add_header(h, v)
        .await
        .assert_status_ok();

    scheduler::poll_once(&env.state, now + Duration::minutes(1))
        .await
        .unwrap();

    assert_eq!(env.transport.events("task.due").len(), 2);
}

#[tokio::test]
async fn test_scheduler_rearms_after_delete_and_reopen() {
    let env = setup().await;
    let now = Utc.with_ymd_and_hms(2026, 4, 7, 8, 0, 0).unwrap();
    create_webhook(&env, &["task.due"]).await;
    let due = now + Duration::hours(1);
    let uuid = create_task(&env, &format!("due:{} scheduler delete task", tw_due(due))).await;

    scheduler::poll_once(&env.state, now).await.unwrap();

    let (h, v) = auth_header(&env.token);
    env.server
        .post(&format!("/api/tasks/{uuid}/delete"))
        .add_header(h, v)
        .await
        .assert_status_ok();

    let replica = env
        .state
        .replica_manager
        .get_replica(&env.user_id)
        .await
        .unwrap();
    let mut replica = replica.lock().await;
    let mut task = replica.get_task(uuid).await.unwrap().unwrap();
    let mut ops = Operations::new();
    task.set_status(Status::Pending, &mut ops).unwrap();
    replica.commit_operations(ops).await.unwrap();
    drop(replica);

    scheduler::poll_once(&env.state, now + Duration::minutes(1))
        .await
        .unwrap();

    assert_eq!(env.transport.events("task.due").len(), 2);
}
