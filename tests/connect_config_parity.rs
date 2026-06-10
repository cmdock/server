//! Baseline parity tests for connect-config issuance and consumption (#123).
//!
//! Captures the *current* shape of audit events, telemetry classification,
//! and TTL semantics across HTTP and CLI flows. The forthcoming
//! `ConnectConfigService` consolidation must keep these tests green — any
//! drift between this baseline and the post-refactor code is a regression.
//!
//! Test surface today:
//!   1. HTTP and CLI emit `connect_config.generate` audit events with an
//!      identical shared field set.
//!   2. First use of a connect-config credential emits exactly one
//!      `connect_config.consume` audit event; subsequent uses do not.
//!   3. HTTP issuance defaults to a 60-minute TTL.
//!   4. CLI honours `expires_minutes` overrides.
//!   5. Use of a non-connect-config bearer token produces no
//!      `connect_config.consume` audit event.

mod common;

use std::sync::{Arc, Mutex, OnceLock};

use axum::http::{header, HeaderValue, Method};
use axum::Router;
use axum_test::TestServer;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{Map, Value};
use tempfile::TempDir;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use tracing::field::{Field, Visit};
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use cmdock_server::admin;
use cmdock_server::admin::cli::{AdminCommand, ConnectConfigAction};
use cmdock_server::app_config;
use cmdock_server::app_state::AppState;
use cmdock_server::config::ServerConfig;
use cmdock_server::config_api;
use cmdock_server::health;
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::summary;
use cmdock_server::sync;
use cmdock_server::tasks;
use cmdock_server::tc_sync;
use cmdock_server::views;

// ---------------------------------------------------------------------------
// Audit capture layer
// ---------------------------------------------------------------------------

/// Process-global buffer of every `target: "audit"` event observed by the
/// test harness. Tests filter by `user_id` so they can run in parallel.
static AUDIT_BUF: OnceLock<Arc<Mutex<Vec<Value>>>> = OnceLock::new();

fn install_audit_capture() -> Arc<Mutex<Vec<Value>>> {
    AUDIT_BUF
        .get_or_init(|| {
            let buf = Arc::new(Mutex::new(Vec::<Value>::new()));
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
    buf: Arc<Mutex<Vec<Value>>>,
}

impl<S> Layer<S> for AuditCaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
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
            Value::String(format!("{:?}", value)),
        );
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        let n = serde_json::Number::from_f64(value).unwrap_or_else(|| serde_json::Number::from(0));
        self.0.insert(field.name().to_string(), Value::Number(n));
    }
}

fn audit_events_for(user_id: &str) -> Vec<Value> {
    let buf = install_audit_capture();
    let buf = buf.lock().unwrap();
    buf.iter()
        .filter(|ev| ev.get("user_id").and_then(Value::as_str) == Some(user_id))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Test environment
// ---------------------------------------------------------------------------

const ADMIN_TOKEN: &str = "parity-admin-token";

struct ParityEnv {
    server: TestServer,
    store: Arc<dyn ConfigStore>,
    config: ServerConfig,
    data_dir: std::path::PathBuf,
    _tmp: TempDir,
    user_id: String,
    api_token: String,
    admin_token: String,
}

async fn setup_parity(username: &str) -> ParityEnv {
    install_audit_capture();

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
            username: username.to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();
    cmdock_server::admin::prefix::backfill_missing_user_prefixes(store.as_ref())
        .await
        .unwrap();
    let api_token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();
    std::fs::create_dir_all(data_dir.join("users").join(&user.id)).unwrap();

    let mut config = common::test_server_config_with_admin_token(data_dir.clone(), ADMIN_TOKEN);
    config.master_key = Some([42u8; 32]);
    config.server.public_base_url = Some("https://tasks.example.com".to_string());

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

    ParityEnv {
        server,
        store,
        config,
        data_dir,
        _tmp: tmp,
        user_id: user.id,
        api_token,
        admin_token: ADMIN_TOKEN.to_string(),
    }
}

fn auth_header(token: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

fn parse_audit_expires_at(value: &str) -> DateTime<Utc> {
    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .unwrap_or_else(|err| panic!("bad expires_at {value:?}: {err}"));
    DateTime::from_naive_utc_and_offset(naive, Utc)
}

async fn issue_via_http(env: &ParityEnv, name: Option<&str>) -> Value {
    let (h, v) = auth_header(&env.admin_token);
    let body = if let Some(n) = name {
        serde_json::json!({ "name": n })
    } else {
        serde_json::json!({})
    };
    let resp = env
        .server
        .post(&format!("/admin/user/{}/connect-config", env.user_id))
        .add_header(h, v)
        .json(&body)
        .await;
    resp.assert_status_ok();
    resp.json()
}

async fn issue_via_cli(env: &ParityEnv, name: Option<&str>, expires_minutes: u32) {
    cmdock_server::admin::cli::run(
        AdminCommand::ConnectConfig {
            action: ConnectConfigAction::Create {
                user_id: env.user_id.clone(),
                server_url: Some("https://tasks.example.com".to_string()),
                name: name.map(|s| s.to_string()),
                expires_minutes,
                no_qr: true,
                scheme: "cmdock".to_string(),
            },
        },
        &env.data_dir,
        Some(&env.config),
    )
    .await
    .expect("CLI connect-config issuance succeeds");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Phase 1.1 #1: HTTP and CLI both emit a `connect_config.generate` audit
/// event, and the shared field set (`source`, `client_ip`, `user_id`,
/// `token_id`, `credential_hash_prefix`, `expires_at`) is identical in
/// shape (keys present and string-typed) on both flows.
#[tokio::test]
async fn cli_and_http_emit_identical_audit_event_names_and_fields() {
    let env = setup_parity("parity_user_1").await;

    issue_via_http(&env, Some("Simon's iPhone")).await;
    // CLI path builds a full connect URL — names push the URL over the
    // 250-byte architecture budget (connect-config-contract.md § Size Budget).
    // This test validates audit field shapes, not name handling; omit name.
    issue_via_cli(&env, None, 60).await;

    let events = audit_events_for(&env.user_id);
    let generates: Vec<&Value> = events
        .iter()
        .filter(|ev| ev.get("action").and_then(Value::as_str) == Some("connect_config.generate"))
        .collect();
    assert_eq!(
        generates.len(),
        2,
        "expected one connect_config.generate per flow, got {}: {:#?}",
        generates.len(),
        generates
    );

    let http = generates
        .iter()
        .find(|ev| ev.get("source").and_then(Value::as_str) == Some("api"))
        .expect("HTTP audit event with source=api missing");
    let cli = generates
        .iter()
        .find(|ev| ev.get("source").and_then(Value::as_str) == Some("cli"))
        .expect("CLI audit event with source=cli missing");

    for field in [
        "source",
        "client_ip",
        "user_id",
        "token_id",
        "credential_hash_prefix",
        "expires_at",
    ] {
        assert!(
            http.get(field).and_then(Value::as_str).is_some(),
            "HTTP event missing string field {field}: {http:#?}"
        );
        assert!(
            cli.get(field).and_then(Value::as_str).is_some(),
            "CLI event missing string field {field}: {cli:#?}"
        );
    }
    // client_ip intentionally differs across flows: HTTP extracts from
    // headers (today: "unknown" in TestServer), CLI hardcodes "local". The
    // post-refactor service must keep this asymmetry, so capture it as
    // baseline rather than asserting equality.
    assert_eq!(http["user_id"], cli["user_id"]);
    // Per-source distinguishing fields (intentionally divergent today —
    // captured as baseline so the consolidation knows what to preserve).
    assert!(
        cli.get("url_length").and_then(Value::as_u64).is_some()
            || cli.get("url_length").and_then(Value::as_i64).is_some(),
        "CLI event should carry url_length: {cli:#?}"
    );
    // HTTP carries request_id (Option<String>, recorded via Debug).
    assert!(
        http.get("request_id").is_some(),
        "HTTP event should carry request_id: {http:#?}"
    );
}

/// Phase 1.1 #2: First use of a connect-config credential emits exactly
/// one `connect_config.consume` audit event; a second use of the same
/// credential does not emit another `connect_config.consume` event.
///
/// Note: today, repeat-use only records a metric — there is no audit
/// event for repeat-use. Asserting the *absence* of a second event makes
/// the post-refactor service preserve this behaviour explicitly.
#[tokio::test]
async fn first_use_then_repeat_use_emits_correct_audit_actions() {
    let env = setup_parity("parity_user_2").await;
    let body = issue_via_http(&env, None).await;
    let credential = body["credential"].as_str().unwrap().to_string();

    // Use an authenticated endpoint so AuthUser runs and the connect-config
    // consume telemetry fires. /api/healthz is unauthenticated.
    let (h, v) = auth_header(&credential);
    env.server
        .get("/api/views")
        .add_header(h.clone(), v.clone())
        .await
        .assert_status_ok();
    env.server
        .get("/api/views")
        .add_header(h, v)
        .await
        .assert_status_ok();

    let events = audit_events_for(&env.user_id);
    let consumes: Vec<&Value> = events
        .iter()
        .filter(|ev| ev.get("action").and_then(Value::as_str) == Some("connect_config.consume"))
        .collect();
    assert_eq!(
        consumes.len(),
        1,
        "expected exactly one connect_config.consume event, got {}: {:#?}",
        consumes.len(),
        consumes
    );
    let consume = consumes[0];
    assert_eq!(consume["source"], "api");
    assert_eq!(consume["user_id"], env.user_id);
    assert!(
        consume.get("token_id").and_then(Value::as_str).is_some(),
        "consume event missing token_id: {consume:#?}"
    );
    assert!(
        consume
            .get("credential_hash_prefix")
            .and_then(Value::as_str)
            .is_some(),
        "consume event missing credential_hash_prefix: {consume:#?}"
    );
}

/// Phase 1.1 #3: HTTP issuance defaults to a 60-minute TTL. We assert
/// the expires_at recorded in the audit event is within
/// [60min - 2min, 60min + 2min] of the current wall clock.
#[tokio::test]
async fn default_ttl_is_60_minutes_via_http() {
    let env = setup_parity("parity_user_3").await;
    let issued_at = Utc::now();
    let _body = issue_via_http(&env, None).await;

    let events = audit_events_for(&env.user_id);
    let generate = events
        .iter()
        .find(|ev| ev.get("action").and_then(Value::as_str) == Some("connect_config.generate"))
        .expect("connect_config.generate event missing");
    let expires_at = parse_audit_expires_at(generate["expires_at"].as_str().unwrap());
    let delta = (expires_at - issued_at).num_seconds();
    assert!(
        delta >= 58 * 60 && delta <= 62 * 60,
        "expected ~60min TTL, got {delta}s (issued={issued_at} expires={expires_at})"
    );
}

/// Phase 1.1 #4: CLI honours `--expires-minutes` overrides.
#[tokio::test]
async fn cli_can_override_ttl_via_expires_minutes() {
    let env = setup_parity("parity_user_4").await;
    let issued_at = Utc::now();
    issue_via_cli(&env, None, 10).await;

    let events = audit_events_for(&env.user_id);
    let generate = events
        .iter()
        .find(|ev| {
            ev.get("action").and_then(Value::as_str) == Some("connect_config.generate")
                && ev.get("source").and_then(Value::as_str) == Some("cli")
        })
        .expect("CLI connect_config.generate event missing");
    let expires_at = parse_audit_expires_at(generate["expires_at"].as_str().unwrap());
    let delta = (expires_at - issued_at).num_seconds();
    assert!(
        delta >= 8 * 60 && delta <= 12 * 60,
        "expected ~10min TTL from CLI override, got {delta}s"
    );
}

/// Phase 1.1 #5: A regular API token (not a connect-config credential)
/// authenticating against the server does not produce a
/// `connect_config.consume` audit event.
#[tokio::test]
async fn non_connect_config_token_use_is_classified_correctly() {
    let env = setup_parity("parity_user_5").await;

    let (h, v) = auth_header(&env.api_token);
    env.server
        .get("/api/views")
        .add_header(h, v)
        .await
        .assert_status_ok();

    // Touch the service too — `api_token` exists, so `record_use` is
    // reached but should classify as NotConnectConfig (label mismatch).
    let svc = cmdock_server::admin::services::connect_config::ConnectConfigService::new(
        env.store.clone(),
    );
    let outcome = svc
        .record_use(
            &env.api_token,
            &cmdock_server::admin::services::connect_config::UseContext {
                client_ip: "test".to_string(),
                request_id: None,
                request_path: "/test".to_string(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        cmdock_server::admin::services::connect_config::UseOutcome::NotConnectConfig
    ));

    let events = audit_events_for(&env.user_id);
    let consumes: Vec<&Value> = events
        .iter()
        .filter(|ev| ev.get("action").and_then(Value::as_str) == Some("connect_config.consume"))
        .collect();
    assert!(
        consumes.is_empty(),
        "non-connect-config token should not emit connect_config.consume; got: {consumes:#?}"
    );
}

/// Phase 1.1 #6 (#123 codex iter1 I2 follow-up): the HTTP audit envelope
/// records the *normalized* `name` field, not the raw input. Whitespace-
/// only names must surface as `None` (Debug-rendered) — the pre-refactor
/// HTTP adapter ran `normalize_optional_display_name` inline before
/// emitting; after consolidation the service does it once at the boundary
/// for both flows.
///
/// CLI does not audit-log `name` (the `connect_config.generate` event
/// shape intentionally diverges per source — see
/// `cli_and_http_emit_identical_audit_event_names_and_fields`). The
/// service's normalization is observable on the CLI path through the
/// connect URL (no `name=` query parameter when normalized to `None`),
/// covered separately.
#[tokio::test]
async fn http_audit_records_normalized_display_name() {
    let env = setup_parity("parity_user_6").await;

    issue_via_http(&env, Some("   ")).await;
    issue_via_http(&env, Some("\t \n")).await;
    issue_via_http(&env, Some("Real Name")).await;

    let events = audit_events_for(&env.user_id);
    let generates: Vec<&Value> = events
        .iter()
        .filter(|ev| ev.get("action").and_then(Value::as_str) == Some("connect_config.generate"))
        .filter(|ev| ev.get("source").and_then(Value::as_str) == Some("api"))
        .collect();
    assert_eq!(generates.len(), 3, "expected three HTTP generate events");

    // Audit visitor stringifies `name = ?Option<String>` via Debug:
    //   None              → "None"
    //   Some("Real Name") → "Some(\"Real Name\")"
    let names: Vec<&str> = generates
        .iter()
        .map(|ev| ev.get("name").and_then(Value::as_str).unwrap_or(""))
        .collect();
    assert_eq!(
        names.iter().filter(|n| **n == "None").count(),
        2,
        "two whitespace-only names should normalize to None; got {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("Real Name")),
        "non-whitespace name should survive normalization; got {names:?}"
    );
}

/// Phase 1.1 #7 (#123 codex iter1 I4 follow-up): explicit shape parity
/// across CLI and HTTP audit events for the connect-config-shared fields.
/// Pins token-id format (`cc_<16-hex>`), credential_hash_prefix length
/// (8), and expires_at format ("YYYY-MM-DD HH:MM:SS"). Stronger than
/// `cli_and_http_emit_identical_audit_event_names_and_fields`, which
/// only checks field presence.
#[tokio::test]
async fn cli_and_http_audit_field_shapes_match_contract() {
    let env = setup_parity("parity_user_7").await;

    issue_via_http(&env, Some("Phone")).await;
    // CLI path builds a full connect URL — names push the URL over the
    // 250-byte architecture budget. This test validates token/prefix/date
    // field shapes, not name handling; omit name on the CLI call.
    issue_via_cli(&env, None, 60).await;

    let events = audit_events_for(&env.user_id);
    let generates: Vec<&Value> = events
        .iter()
        .filter(|ev| ev.get("action").and_then(Value::as_str) == Some("connect_config.generate"))
        .collect();
    assert_eq!(generates.len(), 2);

    for ev in &generates {
        let source = ev.get("source").and_then(Value::as_str).unwrap_or("?");
        let token_id = ev
            .get("token_id")
            .and_then(Value::as_str)
            .expect("token_id");
        assert!(
            token_id.starts_with("cc_") && token_id.len() == 19,
            "{source}: token_id should be `cc_<16-hex>`, got {token_id:?}"
        );
        assert!(
            token_id[3..].chars().all(|c| c.is_ascii_hexdigit()),
            "{source}: token_id suffix should be hex, got {token_id:?}"
        );

        let prefix = ev
            .get("credential_hash_prefix")
            .and_then(Value::as_str)
            .expect("credential_hash_prefix");
        assert_eq!(
            prefix.len(),
            8,
            "{source}: credential_hash_prefix should be 8 chars, got {prefix:?}"
        );
        assert!(
            prefix.chars().all(|c| c.is_ascii_hexdigit()),
            "{source}: credential_hash_prefix should be hex, got {prefix:?}"
        );

        let expires_at = ev
            .get("expires_at")
            .and_then(Value::as_str)
            .expect("expires_at");
        // Canonical format `YYYY-MM-DD HH:MM:SS` — 19 chars, hyphens at
        // 4/7, space at 10, colons at 13/16.
        assert_eq!(
            expires_at.len(),
            19,
            "{source}: expires_at should be 19 chars, got {expires_at:?}"
        );
        assert_eq!(&expires_at[4..5], "-");
        assert_eq!(&expires_at[7..8], "-");
        assert_eq!(&expires_at[10..11], " ");
        assert_eq!(&expires_at[13..14], ":");
        assert_eq!(&expires_at[16..17], ":");
    }
}
