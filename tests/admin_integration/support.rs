//! Shared support for admin integration tests.

pub(crate) use crate::common;

pub(crate) use std::sync::Arc;
pub(crate) use std::time::Duration;

pub(crate) use axum::http::{header, HeaderValue, Method};
pub(crate) use axum::Router;
pub(crate) use axum_test::TestServer;
pub(crate) use chrono::DateTime;
pub(crate) use serde_json::Value;
pub(crate) use tempfile::TempDir;
pub(crate) use tower_http::cors::CorsLayer;
pub(crate) use tower_http::limit::RequestBodyLimitLayer;
pub(crate) use tower_http::trace::TraceLayer;

pub(crate) use cmdock_server::admin;
pub(crate) use cmdock_server::admin::recovery::run_startup_recovery_assessment;
pub(crate) use cmdock_server::app_config;
pub(crate) use cmdock_server::app_state::AppState;
pub(crate) use cmdock_server::config_api;
pub(crate) use cmdock_server::health;
pub(crate) use cmdock_server::runtime_policy::{
    RuntimeAccessMode, RuntimeDeleteAction, RuntimePolicy,
};
pub(crate) use cmdock_server::store::models::NewUser;
pub(crate) use cmdock_server::store::sqlite::SqliteConfigStore;
pub(crate) use cmdock_server::store::ConfigStore;
pub(crate) use cmdock_server::summary;
pub(crate) use cmdock_server::sync;
pub(crate) use cmdock_server::tasks;
pub(crate) use cmdock_server::tc_sync;
pub(crate) use cmdock_server::views;

// --- Helpers ---

pub(crate) fn auth_header(token: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

pub(crate) fn assert_rfc3339_timestamp(value: &Value) {
    let raw = value.as_str().expect("expected timestamp string");
    DateTime::parse_from_rfc3339(raw).expect("expected RFC3339 timestamp");
}

pub(crate) const ADMIN_TOKEN: &str = "admin-test-token";

pub(crate) struct TestEnv {
    pub(crate) server: TestServer,
    pub(crate) store: Arc<dyn ConfigStore>,
    pub(crate) maintenance: Arc<dyn cmdock_server::store::OperatorMaintenanceBackend>,
    pub(crate) _tmp: TempDir,
    pub(crate) user_id: String,
    pub(crate) token: String,
    pub(crate) admin_token: String,
}

pub(crate) async fn setup() -> TestEnv {
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

    // Create a user and token
    let user = store
        .create_user(&NewUser {
            username: "admin_test_user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    // Mirror production startup: assign task-key prefixes for any users
    // created via `create_user` (which doesn't auto-assign). Idempotent.
    cmdock_server::admin::prefix::backfill_missing_user_prefixes(store.as_ref())
        .await
        .unwrap();

    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();

    std::fs::create_dir_all(tmp.path().join("users").join(&user.id)).unwrap();

    let config = common::test_server_config_with_admin_token(data_dir.clone(), ADMIN_TOKEN);

    let state = AppState::new(store.clone(), sqlite_store.clone(), &config);

    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(admin::routes())
        .with_state(state.clone())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .merge(tc_sync::routes().with_state(state))
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);

    TestEnv {
        server,
        store,
        maintenance: sqlite_store.clone(),
        _tmp: tmp,
        user_id: user.id,
        token,
        admin_token: ADMIN_TOKEN.to_string(),
    }
}

pub(crate) async fn setup_with_startup_recovery_issue() -> TestEnv {
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
            username: "startup_recovery_user".to_string(),
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
    store
        .create_device(
            &user.id,
            "deadbeef-dead-beef-dead-beefdeadbeef",
            "Broken Device",
            None,
        )
        .await
        .unwrap();
    std::fs::create_dir_all(tmp.path().join("users").join(&user.id)).unwrap();

    let config = common::test_server_config_with_admin_token(data_dir.clone(), ADMIN_TOKEN);

    let state = AppState::new(store.clone(), sqlite_store.clone(), &config);
    run_startup_recovery_assessment(&state).await.unwrap();

    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(admin::routes())
        .with_state(state.clone())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .merge(tc_sync::routes().with_state(state))
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);

    TestEnv {
        server,
        store,
        maintenance: sqlite_store.clone(),
        _tmp: tmp,
        user_id: user.id,
        token,
        admin_token: ADMIN_TOKEN.to_string(),
    }
}

pub(crate) async fn setup_bootstrap() -> TestEnv {
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
            username: "admin_test_user".to_string(),
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

    let mut config = common::test_server_config_with_admin_token(data_dir.clone(), ADMIN_TOKEN);
    config.master_key = Some([42u8; 32]);

    let state = AppState::new(store.clone(), sqlite_store.clone(), &config);

    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(admin::routes())
        .with_state(state.clone())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .merge(tc_sync::routes().with_state(state))
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);

    TestEnv {
        server,
        store,
        maintenance: sqlite_store.clone(),
        _tmp: tmp,
        user_id: user.id,
        token,
        admin_token: ADMIN_TOKEN.to_string(),
    }
}

pub(crate) async fn setup_empty() -> TestEnv {
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

    let config = common::test_server_config_with_admin_token(data_dir.clone(), ADMIN_TOKEN);
    let state = AppState::new(store.clone(), sqlite_store.clone(), &config);

    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(admin::routes())
        .with_state(state.clone())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .merge(tc_sync::routes().with_state(state))
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);

    TestEnv {
        server,
        store,
        maintenance: sqlite_store.clone(),
        _tmp: tmp,
        user_id: String::new(),
        token: String::new(),
        admin_token: ADMIN_TOKEN.to_string(),
    }
}

pub(crate) async fn setup_with_backup_retention(retention_count: usize) -> TestEnv {
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
            username: "admin_test_user".to_string(),
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

    let mut config = common::test_server_config_with_admin_token(data_dir.clone(), ADMIN_TOKEN);
    config.backup_retention_count = retention_count;

    let state = AppState::new(store.clone(), sqlite_store.clone(), &config);

    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(admin::routes())
        .with_state(state.clone())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .merge(tc_sync::routes().with_state(state))
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let server = TestServer::new(app);

    TestEnv {
        server,
        store,
        maintenance: sqlite_store.clone(),
        _tmp: tmp,
        user_id: user.id,
        token,
        admin_token: ADMIN_TOKEN.to_string(),
    }
}

pub(crate) async fn setup_connect_config() -> TestEnv {
    let mut env = setup().await;
    let mut config =
        common::test_server_config_with_admin_token(env._tmp.path().to_path_buf(), ADMIN_TOKEN);
    config.master_key = Some([42u8; 32]);
    config.server.public_base_url = Some("https://tasks.example.com".to_string());

    let state = AppState::new(env.store.clone(), env.maintenance.clone(), &config);
    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(admin::routes())
        .with_state(state.clone())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .merge(tc_sync::routes().with_state(state))
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );
    env.server = TestServer::new(app);
    env
}

pub(crate) fn client_id_header(client_id: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::HeaderName::from_static("x-client-id"),
        HeaderValue::from_str(client_id).unwrap(),
    )
}

pub(crate) fn backup_root(env: &TestEnv) -> std::path::PathBuf {
    env._tmp.path().join("backups")
}

pub(crate) fn backup_manifest_path(env: &TestEnv, timestamp: &str) -> std::path::PathBuf {
    backup_root(env).join(timestamp).join("manifest.json")
}

pub(crate) fn write_minimal_sqlite(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS sanity_check (id INTEGER PRIMARY KEY);")
        .unwrap();
}

// --- Audit capture (gate6 audit-field assertions) ---
//
// Process-global buffer of every `target: "audit"` tracing event observed by
// the test harness. Mirrors the proven pattern in
// `tests/connect_config_parity.rs`. Tests filter by `user_id` so they can run
// in parallel without cross-contamination.

use std::sync::{Mutex as StdMutex, OnceLock};

use serde_json::Map;
use tracing::field::{Field, Visit};
use tracing::Subscriber;
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

static AUDIT_BUF: OnceLock<Arc<StdMutex<Vec<Value>>>> = OnceLock::new();

pub(crate) fn install_audit_capture() -> Arc<StdMutex<Vec<Value>>> {
    AUDIT_BUF
        .get_or_init(|| {
            let buf = Arc::new(StdMutex::new(Vec::<Value>::new()));
            let layer = AuditCaptureLayer {
                buf: Arc::clone(&buf),
            };
            let subscriber = tracing_subscriber::registry().with(layer);
            tracing::subscriber::set_global_default(subscriber)
                .expect("install audit-capture subscriber once per test binary");
            buf
        })
        .clone()
}

struct AuditCaptureLayer {
    buf: Arc<StdMutex<Vec<Value>>>,
}

impl<S> Layer<S> for AuditCaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: LayerContext<'_, S>) {
        if event.metadata().target() != "audit" {
            return;
        }
        let mut visitor = JsonVisitor(Map::new());
        event.record(&mut visitor);
        if let Ok(mut buf) = self.buf.lock() {
            buf.push(Value::Object(visitor.0));
        }
    }
}

struct JsonVisitor(Map<String, Value>);

impl Visit for JsonVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0
            .insert(field.name().to_string(), Value::String(value.to_string()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0
            .insert(field.name().to_string(), Value::Number(value.into()));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0
            .insert(field.name().to_string(), Value::Number(value.into()));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), Value::Bool(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_string(),
            Value::String(format!("{value:?}")),
        );
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        let n = serde_json::Number::from_f64(value).unwrap_or_else(|| serde_json::Number::from(0));
        self.0.insert(field.name().to_string(), Value::Number(n));
    }
}

pub(crate) fn audit_events_for(user_id: &str) -> Vec<Value> {
    let buf = install_audit_capture();
    let buf = buf.lock().unwrap();
    buf.iter()
        .filter(|ev| ev.get("user_id").and_then(Value::as_str) == Some(user_id))
        .cloned()
        .collect()
}
