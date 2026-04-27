//! Integration tests for the Prometheus /metrics endpoint.
//!
//! Verifies that metrics are collected and exposed correctly after
//! exercising various API endpoints.

mod common;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use async_trait::async_trait;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware;
use axum::Router;
use axum_test::TestServer;
use chrono::TimeZone;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use cmdock_server::admin;
use cmdock_server::admin::recovery::run_startup_recovery_assessment;
use cmdock_server::app_config;
use cmdock_server::app_state::AppState;
use cmdock_server::config_api;
use cmdock_server::health;
use cmdock_server::me;
use cmdock_server::metrics;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::summary;
use cmdock_server::sync;
use cmdock_server::tasks;
use cmdock_server::views;
use cmdock_server::webhooks;
use cmdock_server::webhooks::delivery::{
    WebhookDispatchRequest, WebhookDispatchResult, WebhookTransport,
};
use cmdock_server::webhooks::scheduler;
use cmdock_server::webhooks::security::WebhookDnsResolver;

const ADMIN_TOKEN: &str = "metrics-admin-token";

fn metrics_test_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

#[derive(Debug, Default)]
struct FakeWebhookTransport;

#[async_trait]
impl WebhookTransport for FakeWebhookTransport {
    async fn dispatch(
        &self,
        _request: WebhookDispatchRequest,
    ) -> anyhow::Result<WebhookDispatchResult> {
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

fn auth_header(token: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

async fn setup_test_server_with_metrics(
) -> (TestServer, Arc<dyn ConfigStore>, String, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();
    std::fs::create_dir_all(data_dir.join("backups")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&cmdock_server::store::models::NewUser {
            username: "metricsuser".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();

    let config = common::test_server_config_with_admin_token(data_dir.clone(), ADMIN_TOKEN);

    let metrics_handle = metrics::setup_metrics();
    let state = AppState::new(store.clone(), &config);

    let app = Router::new()
        .route(
            "/metrics",
            axum::routing::get(metrics::metrics_handler).with_state(metrics_handle),
        )
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(me::routes())
        .merge(admin::routes())
        .with_state(state)
        .layer(middleware::from_fn(metrics::metrics_middleware))
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);
    std::mem::forget(tmp);

    (server, store, token, ADMIN_TOKEN.to_string(), user.id)
}

async fn setup_test_server_with_missing_backup_dir_metrics() -> TestServer {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let config = common::test_server_config_with_admin_token(data_dir.clone(), ADMIN_TOKEN);
    let metrics_handle = metrics::setup_metrics();
    let state = AppState::new(store, &config);

    let app = Router::new()
        .route(
            "/metrics",
            axum::routing::get(metrics::metrics_handler).with_state(metrics_handle),
        )
        .merge(health::routes())
        .with_state(state)
        .layer(middleware::from_fn(metrics::metrics_middleware))
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);
    std::mem::forget(tmp);
    server
}

async fn setup_test_server_with_startup_recovery_metrics() -> (TestServer, String) {
    let tmp = tempfile::tempdir().unwrap();
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
        .create_user(&cmdock_server::store::models::NewUser {
            username: "startupmetrics".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();
    store
        .create_device(
            &user.id,
            "feedface-feed-face-feed-facefeedface",
            "Broken Device",
            None,
        )
        .await
        .unwrap();
    std::fs::create_dir_all(data_dir.join("users").join(&user.id)).unwrap();

    let config = common::test_server_config_with_admin_token(data_dir.clone(), ADMIN_TOKEN);

    let metrics_handle = metrics::setup_metrics();
    let state = AppState::new(store, &config);
    run_startup_recovery_assessment(&state).await.unwrap();

    let app = Router::new()
        .route(
            "/metrics",
            axum::routing::get(metrics::metrics_handler).with_state(metrics_handle),
        )
        .merge(admin::routes())
        .with_state(state)
        .layer(middleware::from_fn(metrics::metrics_middleware))
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);
    std::mem::forget(tmp);
    (server, user.id)
}

async fn setup_test_server_with_webhook_metrics() -> (TestServer, String, AppState) {
    let tmp = tempfile::tempdir().unwrap();
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
        .create_user(&cmdock_server::store::models::NewUser {
            username: "webhookmetrics".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();
    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();

    let config = common::test_server_config_with_master_key(data_dir.clone(), [7u8; 32]);
    let metrics_handle = metrics::setup_metrics();
    let state = AppState::with_webhook_transport_and_retry_delays(
        store,
        &config,
        Arc::new(FakeWebhookTransport),
        Arc::new(FakeWebhookDnsResolver),
        vec![
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(10),
        ],
    );

    let app = Router::new()
        .route(
            "/metrics",
            axum::routing::get(metrics::metrics_handler).with_state(metrics_handle),
        )
        .merge(tasks::routes())
        .merge(webhooks::routes())
        .merge(health::routes())
        .with_state(state.clone())
        .layer(middleware::from_fn(metrics::metrics_middleware))
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);
    std::mem::forget(tmp);
    (server, token, state)
}

#[tokio::test]
async fn test_metrics_endpoint_returns_prometheus_format() {
    let _guard = metrics_test_guard();
    let (server, _store, _token, _admin_token, _user_id) = setup_test_server_with_metrics().await;

    let response = server.get("/metrics").await;
    response.assert_status_ok();

    let body = response.text();

    // Should contain Prometheus-format metrics
    assert!(
        body.contains("# TYPE") || body.contains("# HELP"),
        "Metrics should be in Prometheus exposition format"
    );
}

#[tokio::test]
async fn test_metrics_track_http_requests() {
    let _guard = metrics_test_guard();
    let (server, _store, _token, _admin_token, _user_id) = setup_test_server_with_metrics().await;

    // Hit healthz a few times
    server.get("/healthz").await.assert_status_ok();
    server.get("/healthz").await.assert_status_ok();

    // Check metrics
    let metrics_body = server.get("/metrics").await.text();

    assert!(
        metrics_body.contains("http_requests_total"),
        "Should have http_requests_total counter"
    );
    assert!(
        metrics_body.contains("http_request_duration_seconds"),
        "Should have http_request_duration_seconds histogram"
    );
}

#[tokio::test]
async fn test_metrics_track_auth_operations() {
    let _guard = metrics_test_guard();
    let (server, _store, token, _admin_token, _user_id) = setup_test_server_with_metrics().await;
    let (hdr_name, hdr_val) = auth_header(&token);

    // Make an authenticated request
    server
        .get("/api/tasks")
        .add_header(hdr_name, hdr_val)
        .await
        .assert_status_ok();

    let metrics_body = server.get("/metrics").await.text();

    assert!(
        metrics_body.contains("config_db_queries_total"),
        "Should track config DB queries after auth"
    );
    assert!(
        metrics_body.contains("config_db_query_duration_seconds"),
        "Should track config DB query duration"
    );
}

#[tokio::test]
async fn test_metrics_track_connect_config_consumption() {
    let _guard = metrics_test_guard();
    let (server, store, _token, _admin_token, user_id) = setup_test_server_with_metrics().await;
    let token = store
        .create_connect_config_token(&user_id, "2099-01-01 00:00:00", 18)
        .await
        .unwrap()
        .token;
    let (hdr_name, hdr_val) = auth_header(&token);
    server
        .get("/api/me")
        .add_header(hdr_name, hdr_val)
        .await
        .assert_status_ok();

    let metrics_body = server.get("/metrics").await.text();
    assert!(
        metrics_body.contains("connect_config_consumes_total"),
        "Should expose connect-config consumption metrics"
    );
    assert!(
        metrics_body.contains("connect_config_consumes_total{result=\"first_use\"}"),
        "Should record first successful connect-config use"
    );
}

#[tokio::test]
async fn test_metrics_track_replica_operations() {
    let _guard = metrics_test_guard();
    let (server, _store, token, _admin_token, _user_id) = setup_test_server_with_metrics().await;
    let (hdr_name, hdr_val) = auth_header(&token);

    // Add a task (triggers replica open + create)
    server
        .post("/api/tasks")
        .add_header(hdr_name, hdr_val)
        .json(&serde_json::json!({"raw": "+metrics_test Test task"}))
        .await
        .assert_status_ok();

    let metrics_body = server.get("/metrics").await.text();

    assert!(
        metrics_body.contains("replica_open_duration_seconds"),
        "Should track replica open duration"
    );
    assert!(
        metrics_body.contains("replica_operations_total"),
        "Should track replica operations"
    );
}

#[tokio::test]
async fn test_metrics_track_in_flight_requests() {
    let _guard = metrics_test_guard();
    let (server, _store, _token, _admin_token, _user_id) = setup_test_server_with_metrics().await;

    // Hit a tracked endpoint first (healthz) so the gauge gets set
    server.get("/healthz").await.assert_status_ok();

    let metrics_body = server.get("/metrics").await.text();

    assert!(
        metrics_body.contains("http_requests_in_flight"),
        "Should have in-flight gauge"
    );
}

#[tokio::test]
async fn test_metrics_include_process_metrics() {
    let _guard = metrics_test_guard();
    let (server, _store, _token, _admin_token, _user_id) = setup_test_server_with_metrics().await;

    let metrics_body = server.get("/metrics").await.text();

    assert!(
        metrics_body.contains("process_cpu_seconds_total"),
        "Should have process CPU metric"
    );
    assert!(
        metrics_body.contains("process_resident_memory_bytes"),
        "Should have process memory metric"
    );
    assert!(
        metrics_body.contains("process_threads"),
        "Should have process threads metric"
    );
}

#[tokio::test]
async fn test_metrics_include_disk_space_gauges() {
    let _guard = metrics_test_guard();
    let (server, _store, _token, _admin_token, _user_id) = setup_test_server_with_metrics().await;

    let metrics_body = server.get("/metrics").await.text();

    assert!(
        metrics_body.contains("disk_total_bytes{scope=\"data_dir\"}"),
        "Should expose total bytes for data_dir"
    );
    assert!(
        metrics_body.contains("disk_free_bytes{scope=\"data_dir\"}"),
        "Should expose free bytes for data_dir"
    );
    assert!(
        metrics_body.contains("disk_available_bytes{scope=\"backup_dir\"}"),
        "Should expose available bytes for backup_dir"
    );
    assert!(
        metrics_body.contains("disk_read_only{scope=\"data_dir\"}"),
        "Should expose read-only gauge for data_dir"
    );
}

#[tokio::test]
async fn test_metrics_record_missing_backup_dir_errors() {
    let _guard = metrics_test_guard();
    let server = setup_test_server_with_missing_backup_dir_metrics().await;

    let metrics_body = server.get("/metrics").await.text();

    assert!(
        metrics_body.contains("disk_metric_collection_errors_total{scope=\"backup_dir\"}"),
        "Should record collection errors for a missing backup_dir"
    );
}

#[tokio::test]
async fn test_metrics_no_auth_required() {
    let _guard = metrics_test_guard();
    let (server, _store, _token, _admin_token, _user_id) = setup_test_server_with_metrics().await;

    // /metrics should be accessible without auth (like /healthz)
    let response = server.get("/metrics").await;
    response.assert_status_ok();
}

#[tokio::test]
async fn test_metrics_track_recovery_transitions() {
    let _guard = metrics_test_guard();
    let (server, _store, _token, admin_token, user_id) = setup_test_server_with_metrics().await;
    let (admin_hdr_name, admin_hdr_val) = auth_header(&admin_token);

    let status: serde_json::Value = server
        .get("/admin/status")
        .add_header(admin_hdr_name.clone(), admin_hdr_val.clone())
        .await
        .json();
    assert_eq!(status["quarantined_users"], 0);

    server
        .post(&format!("/admin/user/{user_id}/offline"))
        .add_header(admin_hdr_name.clone(), admin_hdr_val.clone())
        .await
        .assert_status_ok();

    server
        .post(&format!("/admin/user/{user_id}/online"))
        .add_header(admin_hdr_name, admin_hdr_val)
        .await
        .assert_status_ok();

    let metrics_body = server.get("/metrics").await.text();
    assert!(
        metrics_body.contains("recovery_quarantined_users"),
        "Should expose recovery_quarantined_users gauge"
    );
    assert!(
        metrics_body.contains("recovery_transitions_total"),
        "Should expose recovery_transitions_total counter"
    );
}

#[tokio::test]
async fn test_metrics_expose_outbound_http_metrics() {
    let _guard = metrics_test_guard();
    let (server, _store, _token, _admin_token, _user_id) = setup_test_server_with_metrics().await;

    metrics::record_outbound_http_request("anthropic", "success", 0.8);
    metrics::record_outbound_http_request("anthropic", "transport_error", 1.2);
    metrics::record_outbound_http_failure("anthropic", "connect");

    let metrics_body = server.get("/metrics").await.text();
    assert!(
        metrics_body.contains("outbound_http_requests_total"),
        "Should expose outbound_http_requests_total"
    );
    assert!(
        metrics_body.contains("outbound_http_request_duration_seconds"),
        "Should expose outbound_http_request_duration_seconds"
    );
    assert!(
        metrics_body.contains("outbound_http_failures_total"),
        "Should expose outbound_http_failures_total"
    );
    assert!(
        metrics_body.contains("target=\"anthropic\""),
        "Should label outbound metrics by target"
    );
}

#[tokio::test]
async fn test_metrics_track_startup_auto_offline_summary() {
    let _guard = metrics_test_guard();
    let (server, user_id) = setup_test_server_with_startup_recovery_metrics().await;

    let metrics_body = server.get("/metrics").await.text();
    assert!(
        metrics_body.contains("recovery_assessments_total"),
        "Should expose recovery_assessments_total counter"
    );
    assert!(
        metrics_body.contains("status=\"needs_operator_attention\",source=\"startup\""),
        "Should record startup assessment classification"
    );
    assert!(
        metrics_body.contains("recovery_startup_users_needs_operator_attention"),
        "Should expose startup summary gauge"
    );
    assert!(
        metrics_body.contains("recovery_quarantined_users"),
        "Should expose quarantined-user gauge after startup auto-offline"
    );

    let (admin_hdr_name, admin_hdr_val) = auth_header(ADMIN_TOKEN);
    let status: serde_json::Value = server
        .get("/admin/status")
        .add_header(admin_hdr_name, admin_hdr_val)
        .await
        .json();
    assert_eq!(status["quarantined_users"], 1);
    assert_eq!(
        status["startup_recovery"]["newly_offlined_users"][0],
        user_id
    );
}

#[tokio::test]
async fn test_metrics_expose_webhook_delivery_metrics() {
    let _guard = metrics_test_guard();
    let (server, token, _state) = setup_test_server_with_webhook_metrics().await;
    let (hdr_name, hdr_val) = auth_header(&token);

    server
        .post("/api/webhooks")
        .add_header(hdr_name.clone(), hdr_val.clone())
        .json(&serde_json::json!({
            "url": "https://hooks.example.invalid/metrics",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.created"]
        }))
        .await
        .assert_status(StatusCode::CREATED);

    server
        .post("/api/tasks")
        .add_header(hdr_name, hdr_val)
        .json(&serde_json::json!({"raw": "+metrics webhook delivery"}))
        .await
        .assert_status_ok();

    let metrics_body = server.get("/metrics").await.text();
    assert!(
        metrics_body.contains("webhook_deliveries_total"),
        "Should expose webhook delivery counters"
    );
    assert!(
        metrics_body.contains("webhook_delivery_duration_seconds"),
        "Should expose webhook delivery latency histograms"
    );
    assert!(
        metrics_body.contains("event=\"task.created\",status=\"delivered\""),
        "Should label webhook delivery metrics by event and status"
    );
}

#[tokio::test]
async fn test_metrics_expose_webhook_scheduler_metrics() {
    let _guard = metrics_test_guard();
    let (server, token, state) = setup_test_server_with_webhook_metrics().await;
    let (hdr_name, hdr_val) = auth_header(&token);

    server
        .post("/api/webhooks")
        .add_header(hdr_name.clone(), hdr_val.clone())
        .json(&serde_json::json!({
            "url": "https://hooks.example.invalid/scheduler",
            "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
            "events": ["task.due"]
        }))
        .await
        .assert_status(StatusCode::CREATED);

    server
        .post("/api/tasks")
        .add_header(hdr_name, hdr_val)
        .json(&serde_json::json!({"raw": "due:20260408T000000Z +metrics webhook scheduler"}))
        .await
        .assert_status_ok();

    scheduler::poll_once(
        &state,
        chrono::Utc.with_ymd_and_hms(2026, 4, 7, 8, 0, 0).unwrap(),
    )
    .await
    .unwrap();

    let metrics_body = server.get("/metrics").await.text();
    assert!(
        metrics_body.contains("webhook_scheduler_runs_total"),
        "Should expose webhook scheduler counters"
    );
    assert!(
        metrics_body.contains("webhook_scheduler_run_duration_seconds"),
        "Should expose webhook scheduler latency histograms"
    );
    assert!(
        metrics_body.contains("result=\"ok\""),
        "Should label webhook scheduler metrics by result"
    );
}
