#![allow(dead_code)] // Scaffold — stubs will be implemented incrementally

// jemalloc global allocator — reduces sustained-load RSS by avoiding glibc
// arena retention. See the Gate 6 endurance investigation in release issue #76.
//
// **Platform matrix:**
// - Linux + glibc (production target, Docker image): jemalloc + tuned
//   malloc_conf below. This is where the Gate 6 budgets are calibrated.
// - Apple / Android / DragonFly: jemalloc with defaults. `tikv-jemalloc-sys`
//   unconditionally forces the prefixed `_rjem_malloc_conf` symbol on these
//   targets in its build.rs, so an unprefixed `malloc_conf` export would be
//   silently ignored here. Skipping the tuning is correct.
// - Linux + musl and any other non-MSVC target: jemalloc with defaults.
//   The crate emits unprefixed symbols there, but the tuning is still gated
//   out as a conservative default because cmdock-server is only calibrated
//   against Linux + glibc. Broaden the cfg below if another target becomes
//   a production deployment.
// - Windows MSVC: system allocator. jemalloc is not supported on MSVC.
//
// The compiled-in malloc_conf tunes jemalloc for a server workload:
//   background_thread:true  — dedicated background thread for page decay
//                             so dirty pages get returned without needing
//                             an allocation to trigger it (critical under
//                             sustained load where workload-driven decay
//                             is rare)
//   dirty_decay_ms:1000     — return dirty pages to the OS after 1s idle
//                             (default 10_000 ms is too slow for this
//                             profile)
//   muzzy_decay_ms:0        — return muzzy pages immediately
//   narenas:2               — two arenas; balances per-thread caching
//                             against RSS under sustained allocation churn
//
// Operators can override any of these at startup via the MALLOC_CONF env var.

// Global allocator swap: everywhere jemalloc builds (i.e. not MSVC).
#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

// malloc_conf tuning: only on targets where tikv-jemalloc-sys reliably
// emits the unprefixed `malloc_conf` symbol. Linux + glibc is the
// production target; elsewhere jemalloc uses its own defaults.
//
// The FFI type in tikv-jemalloc-sys is `Option<&'static c_char>`, so the
// export must match exactly — a plain `&[u8]` happens to work on common
// targets but is ABI-fragile because a fat slice (ptr+len, 16 bytes) does
// not match the allocator's expected pointer-sized symbol. The union
// transmute below is the canonical pattern from tikv-jemallocator's own
// test suite (see background_thread_enabled.rs in that crate).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(non_upper_case_globals)]
#[export_name = "malloc_conf"]
pub static malloc_conf: Option<&'static libc::c_char> = {
    union Transmute {
        bytes: &'static u8,
        c_char: &'static libc::c_char,
    }
    // Safety: the literal is static, null-terminated, and `u8` has the same
    // layout as `libc::c_char` on every supported platform. The union trick
    // is the canonical pattern from tikv-jemallocator's own test suite.
    Some(unsafe {
        Transmute {
            bytes: &b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0,narenas:2\0"[0],
        }
        .c_char
    })
};

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::{header, Method};
use axum::middleware;
use axum::Router;
use clap::{Parser, Subcommand};
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use cmdock_server::admin;
use cmdock_server::app_config;
use cmdock_server::app_state::AppState;
use cmdock_server::audit;
use cmdock_server::config;
use cmdock_server::config_api;
use cmdock_server::devices;
use cmdock_server::geofences;
use cmdock_server::health;
use cmdock_server::me;
use cmdock_server::metrics;
use cmdock_server::runtime_recovery;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::summary;
use cmdock_server::sync;
use cmdock_server::tasks;
use cmdock_server::tc_sync;
use cmdock_server::views;
use cmdock_server::webhooks;

/// Adds bearer token security scheme to the OpenAPI spec.
struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.security_schemes.insert(
            "bearer".to_string(),
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("token")
                    .description(Some("Bearer token (SHA-256 hashed before storage)"))
                    .build(),
            ),
        );
        components.security_schemes.insert(
            "operatorBearer".to_string(),
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("token")
                    .description(Some("Operator bearer token for /admin/* endpoints"))
                    .build(),
            ),
        );
    }
}

/// OpenAPI documentation for the TaskChampion Server REST API.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "TaskChampion Server API",
        version = "0.1.0",
        description = "Task management server powered by TaskChampion. \
            Provides a REST API for iOS TaskApp and Taskwarrior CLI sync.\n\n\
            ## Authentication\n\
            All endpoints except `/healthz` require a bearer token in the `Authorization` header.\n\n\
            ## Device Provisioning\n\
            TaskChampion sync uses per-device `client_id` credentials. Users create a canonical \
            sync identity once, then register individual devices to obtain device-specific \
            credentials. Revoke is the normal removal path; delete is reserved for permanent \
            cleanup of an already-revoked device record.\n\n\
            ## Error Responses\n\
            Error responses (401, 404, 500) return a plain text body with a short message \
            (e.g. `Missing Authorization header`). They do not return JSON.\n\n\
            ## Legacy Compatibility\n\
            Task mutation endpoints use POST (not PUT/PATCH/DELETE) for backwards \
            compatibility with the iOS TaskApp client.\n\n\
            ## Observability\n\
            Prometheus metrics available at `/metrics`. Process metrics, HTTP request \
            histograms, replica operations, filter evaluation, and LLM call tracking.",
        contact(name = "Simon Inglis"),
    ),
    paths(
        health::handlers::healthz,
        tasks::handlers::list_tasks,
        tasks::handlers::add_task,
        tasks::handlers::complete_task,
        tasks::handlers::undo_task,
        tasks::handlers::delete_task,
        tasks::handlers::modify_task,
        views::handlers::list_views,
        views::handlers::upsert_view,
        views::handlers::delete_view,
        config_api::handlers::get_config,
        config_api::handlers::upsert_config,
        config_api::handlers::delete_config_item,
        sync::handlers::sync,
        app_config::handlers::get_app_config,
        app_config::handlers::upsert_shopping_config,
        app_config::handlers::delete_shopping_config,
        app_config::handlers::list_contexts,
        app_config::handlers::upsert_context,
        app_config::handlers::delete_context,
        app_config::handlers::list_stores,
        app_config::handlers::upsert_store,
        app_config::handlers::delete_store,
        app_config::handlers::upsert_preset,
        app_config::handlers::delete_preset,
        geofences::handlers::list_geofences,
        geofences::handlers::upsert_geofence,
        geofences::handlers::delete_geofence,
        summary::handlers::get_summary,
        devices::handlers::list_devices,
        devices::handlers::register_device,
        devices::handlers::revoke_device,
        devices::handlers::rename_device,
        webhooks::handlers::list_webhooks,
        webhooks::handlers::get_webhook,
        webhooks::handlers::create_webhook,
        webhooks::handlers::update_webhook,
        webhooks::handlers::delete_webhook,
        webhooks::handlers::test_webhook,
        me::handlers::get_me,
        admin::bootstrap::bootstrap_user_device,
        admin::bootstrap::acknowledge_bootstrap_request,
        admin::sync_identity::get_sync_identity,
        admin::sync_identity::ensure_sync_identity,
        admin::runtime_policy::get_runtime_policy,
        admin::runtime_policy::apply_runtime_policy,
        admin::devices::list_devices,
        admin::devices::create_device,
        admin::devices::get_device,
        admin::devices::rename_device,
        admin::devices::revoke_device,
        admin::devices::unrevoke_device,
        admin::devices::delete_device,
        admin::handlers::server_status,
        admin::users::list_users,
        admin::webhooks::list_webhooks,
        admin::webhooks::get_webhook,
        admin::webhooks::create_webhook,
        admin::webhooks::update_webhook,
        admin::webhooks::delete_webhook,
        admin::webhooks::test_webhook,
        admin::backup::create_backup,
        admin::backup::list_backups,
        admin::backup::restore_backup,
        admin::user_diagnostics::user_stats,
        admin::user_lifecycle::delete_user,
        admin::runtime_ops::evict_replica,
        admin::runtime_ops::checkpoint_replica,
        admin::runtime_ops::quarantine_user,
        admin::runtime_ops::unquarantine_user,
    ),
    components(
        schemas(
            health::handlers::HealthResponse,
            tasks::models::TaskItem,
            tasks::models::TaskActionResponse,
            tasks::models::AddTaskRequest,
            tasks::models::ModifyTaskRequest,
            views::handlers::ViewConfig,
            views::handlers::UpsertViewRequest,
            config_api::handlers::ConfigResponse,
            config_api::handlers::ConfigUpsertRequest,
            app_config::handlers::AppConfigResponse,
            app_config::handlers::ContextConfig,
            app_config::handlers::ViewConfigFull,
            app_config::handlers::PresetConfig,
            app_config::handlers::StoreConfig,
            app_config::handlers::ShoppingConfig,
            app_config::handlers::UpsertContextRequest,
            app_config::handlers::UpsertStoreRequest,
            app_config::handlers::UpsertPresetRequest,
            geofences::handlers::GeofenceConfig,
            geofences::handlers::UpsertGeofenceRequest,
            summary::handlers::SummaryResponse,
            devices::handlers::DeviceResponse,
            devices::handlers::RegisterDeviceResponse,
            devices::handlers::RegisterDeviceRequest,
            devices::handlers::RenameDeviceRequest,
            webhooks::api::CreateWebhookRequest,
            webhooks::api::UpdateWebhookRequest,
            webhooks::api::WebhookResponse,
            webhooks::api::WebhookDeliveryResponse,
            webhooks::api::WebhookDetailResponse,
            webhooks::api::WebhookTestResponse,
            webhooks::api::WebhookErrorResponse,
            me::handlers::MeResponse,
            admin::bootstrap::BootstrapUserDeviceRequestBody,
            admin::bootstrap::BootstrapUserDeviceResponse,
            admin::openapi::BootstrapStatusSchema,
            admin::openapi::DeviceStatusSchema,
            admin::sync_identity::OperatorSyncIdentityResponse,
            admin::sync_identity::EnsureOperatorSyncIdentityResponse,
            admin::runtime_policy::ApplyRuntimePolicyRequest,
            admin::runtime_policy::OperatorRuntimePolicyResponse,
            admin::devices::OperatorDeviceResponse,
            admin::devices::OperatorCreateDeviceRequest,
            admin::devices::OperatorCreateDeviceResponse,
            admin::devices::OperatorRenameDeviceRequest,
            cmdock_server::runtime_policy::RuntimePolicy,
            cmdock_server::runtime_policy::RuntimeAccessMode,
            cmdock_server::runtime_policy::RuntimeDeleteAction,
            cmdock_server::runtime_policy::RuntimePolicyEnforcementState,
            admin::handlers::ServerStatus,
            admin::users::AdminUserSummary,
            admin::webhooks::UpdateAdminWebhookRequest,
            admin::backup::BackupCreateQuery,
            admin::backup::BackupRestoreRequest,
            admin::backup::BackupCreateResponse,
            admin::backup::BackupSummaryResponse,
            admin::backup::BackupListResponse,
            admin::backup::BackupRestoreResponse,
            admin::backup::BackupRestoreReplicaResponse,
            admin::backup::BackupErrorResponse,
            admin::users::UserStats,
            admin::users::IntegrityResult,
            admin::runtime_ops::AdminActionResponse,
            admin::users::DeleteUserResponse,
            cmdock_server::recovery::RecoveryStatus,
            cmdock_server::recovery::UserRecoveryAssessment,
            runtime_recovery::StartupRecoverySnapshot,
        ),
    ),
    modifiers(&SecurityAddon),
    security(
        ("bearer" = [])
    ),
    tags(
        (name = "health", description = "Health check"),
        (name = "tasks", description = "Task CRUD operations"),
        (name = "views", description = "View definitions (filter presets)"),
        (name = "config", description = "Generic configuration (backwards compat)"),
        (name = "sync", description = "Sync operations"),
        (name = "app-config", description = "App configuration (mega endpoint + CRUD)"),
        (name = "geofences", description = "Typed geofence resource"),
        (name = "summary", description = "LLM-generated task summaries"),
        (name = "devices", description = "Device registry and per-device sync credential provisioning"),
        (name = "webhooks", description = "User-scoped webhook registration and delivery history"),
        (name = "me", description = "Authenticated runtime identity"),
        (name = "admin", description = "Operator bootstrap, diagnostics, and recovery endpoints"),
    )
)]
struct ApiDoc;

#[derive(Parser)]
#[command(
    name = "cmdock-server",
    about = "cmdock task management server powered by TaskChampion"
)]
struct Cli {
    /// Path to config file
    #[arg(long, default_value = "config.toml", global = true)]
    config: PathBuf,

    /// Data directory override (admin commands only — bypasses config file)
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Run database migrations and exit
    #[arg(long)]
    migrate: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP server (default when no subcommand given)
    Serve,
    /// Print the generated OpenAPI document and exit
    Openapi {
        /// Write the OpenAPI JSON to a file instead of stdout
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Administrative operations (user/token/backup management)
    Admin {
        #[command(subcommand)]
        action: admin::cli::AdminCommand,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(Command::Openapi { output }) = cli.command {
        let spec = serde_json::to_string_pretty(&ApiDoc::openapi())?;
        if let Some(path) = output {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, spec)?;
        } else {
            println!("{spec}");
        }
        return Ok(());
    }

    // Admin commands use minimal logging (no tracing noise)
    let is_admin = matches!(cli.command, Some(Command::Admin { .. }));

    // Load audit config from config file (best-effort — falls back to disabled)
    let audit_config = config::ServerConfig::load(&cli.config)
        .ok()
        .map(|c| c.audit)
        .unwrap_or_default();

    // App layer: suppress audit events (handled by audit layer).
    // CMDOCK_LOG_LEVEL overrides the default log level (e.g., "debug", "trace").
    // RUST_LOG takes precedence over both (standard tracing-subscriber behaviour).
    let app_filter = if is_admin {
        EnvFilter::new("warn")
    } else {
        let default_level =
            std::env::var("CMDOCK_LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
        EnvFilter::from_default_env()
            .add_directive(format!("cmdock_server={default_level}").parse()?)
    };
    let suppress_audit = Targets::new()
        .with_default(tracing::Level::TRACE)
        .with_target("audit", tracing_subscriber::filter::LevelFilter::OFF);
    let app_layer = tracing_subscriber::fmt::layer()
        .with_filter(app_filter)
        .with_filter(suppress_audit);

    // Audit layer: separate JSON output for audit events (both server and CLI)
    let audit_layer = if audit_config.enabled {
        match audit::setup_audit_layer(&audit_config) {
            Ok(layer) => layer,
            Err(e) => {
                eprintln!("FATAL: Audit logging is enabled but failed to initialise: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(app_layer)
        .with(audit_layer)
        .init();

    // Admin subcommand — can use --data-dir to bypass config file
    if let Some(Command::Admin { action }) = cli.command {
        let config = config::ServerConfig::load(&cli.config).ok();
        let data_dir = if let Some(dir) = cli.data_dir {
            dir
        } else if let Some(config) = config.as_ref() {
            config.server.data_dir.clone()
        } else {
            return Err(anyhow::anyhow!(
                "failed to load config file {}; pass --data-dir or fix the config",
                cli.config.display()
            ));
        };
        std::fs::create_dir_all(&data_dir)?;
        std::fs::create_dir_all(data_dir.join("users"))?;
        return admin::cli::run(action, &data_dir, config.as_ref()).await;
    }

    let config = config::ServerConfig::load(&cli.config)?;

    // Ensure data directories exist
    std::fs::create_dir_all(&config.server.data_dir)?;
    std::fs::create_dir_all(config.server.data_dir.join("users"))?;

    // Initialize Prometheus metrics
    let metrics_handle = metrics::setup_metrics();

    // Initialize config store (SQLite for now, Postgres later)
    let db_path = config.server.data_dir.join("config.sqlite");
    let store: Arc<dyn ConfigStore> =
        Arc::new(SqliteConfigStore::new(&db_path.to_string_lossy()).await?);

    store.run_migrations().await?;

    if cli.migrate {
        tracing::info!("Migrations complete.");
        return Ok(());
    }

    let state = AppState::new(store, &config);
    let startup_recovery =
        admin::services::recovery::RecoveryCoordinator::for_running_state(&state)
            .startup_assessment()
            .await?;
    tracing::info!(
        total_users = startup_recovery.total_users,
        healthy_users = startup_recovery.healthy_users,
        rebuildable_users = startup_recovery.rebuildable_users,
        needs_operator_attention_users = startup_recovery.needs_operator_attention_users,
        already_offline_users = startup_recovery.already_offline_users,
        newly_offlined_users = ?startup_recovery.newly_offlined_users,
        orphan_user_dirs = ?startup_recovery.orphan_user_dirs,
        "Startup recovery assessment complete"
    );

    // Start background reaper for idle sync storage connections (5 min TTL, 60s sweep)
    state.sync_storage_manager.start_reaper();

    // REST routes with 1 MiB body limit
    let rest_routes = Router::new()
        // Metrics endpoint (no auth, no metrics on itself)
        .route(
            "/metrics",
            axum::routing::get(metrics::metrics_handler).with_state(metrics_handle),
        )
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(geofences::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(devices::routes())
        .merge(webhooks::routes())
        .merge(me::routes())
        .merge(admin::routes())
        .merge(SwaggerUi::new("/swagger-ui").url("/api-doc/openapi.json", ApiDoc::openapi()))
        .with_state(state.clone())
        .layer(RequestBodyLimitLayer::new(1024 * 1024)); // 1 MiB for REST API

    // TaskChampion sync protocol routes with their own 10 MiB limit (applied inside tc_sync::routes())
    let sync_routes = tc_sync::routes().with_state(state);

    let app = Router::new()
        .merge(rest_routes)
        .merge(sync_routes)
        .layer(middleware::from_fn(metrics::metrics_middleware))
        .layer(TraceLayer::new_for_http())
        // Request timeout — returns 408 if request processing exceeds 30s
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(30),
        ))
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    tracing::info!("Listening on {bind_addr}");
    tracing::info!("Swagger UI at http://{bind_addr}/swagger-ui/");
    tracing::info!("Prometheus metrics at http://{bind_addr}/metrics");

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    // Graceful shutdown on SIGTERM/SIGINT — drain in-flight requests.
    // Some restricted environments do not permit Unix signal registration.
    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = ctrl_c => tracing::info!("Received SIGINT, shutting down gracefully..."),
                    _ = sigterm.recv() => tracing::info!("Received SIGTERM, shutting down gracefully..."),
                }
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "SIGTERM handler unavailable; falling back to SIGINT-only graceful shutdown"
                );
                ctrl_c.await.ok();
                tracing::info!("Received SIGINT, shutting down gracefully...");
            }
        }
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    tracing::info!("Server shut down cleanly.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_property<'a>(
        spec: &'a serde_json::Value,
        schema: &str,
        property: &str,
    ) -> &'a serde_json::Value {
        &spec["components"]["schemas"][schema]["properties"][property]
    }

    #[test]
    fn operator_openapi_schemas_include_examples_enums_and_formats() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();

        assert_eq!(
            schema_property(
                &spec,
                "BootstrapUserDeviceRequestBody",
                "bootstrapRequestId"
            )["format"],
            "uuid"
        );
        assert_eq!(
            schema_property(&spec, "OperatorSyncIdentityResponse", "createdAt")["format"],
            "date-time"
        );
        assert_eq!(
            schema_property(&spec, "OperatorDeviceResponse", "registeredAt")["format"],
            "date-time"
        );
        assert!(
            schema_property(&spec, "BootstrapUserDeviceResponse", "bootstrapStatus")
                .to_string()
                .contains("BootstrapStatusSchema")
        );
        assert!(schema_property(&spec, "OperatorDeviceResponse", "status")
            .to_string()
            .contains("DeviceStatusSchema"));
        assert_eq!(
            spec["components"]["schemas"]["BootstrapStatusSchema"]["enum"],
            serde_json::json!(["pending_delivery", "acknowledged", "abandoned"])
        );
        assert_eq!(
            spec["components"]["schemas"]["DeviceStatusSchema"]["enum"],
            serde_json::json!(["active", "revoked"])
        );
        assert!(
            spec["components"]["schemas"]["BootstrapUserDeviceResponse"]["example"].is_object()
        );
        assert!(spec["components"]["schemas"]["OperatorDeviceResponse"]["example"].is_object());
        assert!(
            spec["components"]["schemas"]["EnsureOperatorSyncIdentityResponse"]["example"]
                .is_object()
        );
    }

    #[test]
    fn admin_diagnostics_and_recovery_paths_are_in_openapi() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();

        for path in [
            "/admin/status",
            "/admin/users",
            "/admin/user/{user_id}/runtime-policy",
            "/admin/user/{user_id}",
            "/admin/user/{user_id}/stats",
            "/admin/user/{user_id}/evict",
            "/admin/user/{user_id}/checkpoint",
            "/admin/user/{user_id}/offline",
            "/admin/user/{user_id}/online",
        ] {
            assert!(
                spec["paths"][path].is_object(),
                "expected OpenAPI path to include {path}"
            );
        }

        assert_eq!(
            spec["paths"]["/admin/users"]["get"]["responses"]["200"]["content"]["application/json"]
                ["schema"]["items"]["$ref"],
            "#/components/schemas/AdminUserSummary"
        );
        assert_eq!(
            spec["paths"]["/admin/user/{user_id}"]["delete"]["responses"]["200"]["content"]
                ["application/json"]["schema"]["$ref"],
            "#/components/schemas/DeleteUserResponse"
        );
        assert_eq!(
            spec["paths"]["/admin/status"]["get"]["security"],
            serde_json::json!([{ "operatorBearer": [] }])
        );
        assert_eq!(
            spec["paths"]["/admin/user/{user_id}/stats"]["get"]["responses"]["200"]["content"]
                ["application/json"]["schema"]["$ref"],
            "#/components/schemas/UserStats"
        );
        assert_eq!(
            spec["paths"]["/admin/user/{user_id}/runtime-policy"]["put"]["requestBody"]["content"]
                ["application/json"]["schema"]["$ref"],
            "#/components/schemas/ApplyRuntimePolicyRequest"
        );
        assert_eq!(
            spec["components"]["schemas"]["RuntimePolicyEnforcementState"]["enum"],
            serde_json::json!(["unmanaged", "current", "missing_applied", "stale_applied"])
        );
    }

    #[test]
    fn me_endpoint_is_in_openapi_with_expected_schema() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();

        assert!(spec["paths"]["/api/me"]["get"].is_object());
        assert_eq!(
            spec["paths"]["/api/me"]["get"]["responses"]["200"]["content"]["application/json"]
                ["schema"]["$ref"],
            "#/components/schemas/MeResponse"
        );
        assert_eq!(
            spec["components"]["schemas"]["MeResponse"]["properties"]["createdAt"]["format"],
            "date-time"
        );
    }
}
