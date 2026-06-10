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

use anyhow::Context;
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
use cmdock_server::request_id;
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
            Error responses never return JSON. Body conventions:\n\
            - **401** — plain-text short message (`Invalid token`, `Missing Authorization header`).\n\
            - **400** — plain-text bare error code (`INVALID_FIELD`, `INVALID_DATE`, `INVALID_RAW`, \
              `INVALID_CONTEXT_ID`, `INVALID_QUERY_PARAM`, `INVALID_UUID`, `TOO_MANY_UUIDS`, \
              `EMPTY_UUIDS`).\n\
            - **404** — **empty body** (no `Content-Type`, `Content-Length: 0`). Required for \
              iOS-app compatibility and for the existence-leak rule on `/api/tasks/{uuid}`.\n\
            - **500** — plain-text short message.\n\n\
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
        tasks::handlers::get_task_by_id,
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
        admin::devices::rotate_device,
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
        admin::connect_config::create_connect_config,
    ),
    components(
        schemas(
            health::handlers::HealthResponse,
            tasks::models::TaskItem,
            tasks::models::TaskAnnotation,
            tasks::models::TaskActionResponse,
            tasks::models::TaskBatchLookupResponse,
            tasks::models::TaskListResponse,
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
            admin::devices::OperatorRotateDeviceResponse,
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
            admin::users::MergedSyncDiagnostics,
            admin::users::MergedSyncJournalResponse,
            admin::users::MergedSyncJournalStateCountResponse,
            admin::runtime_ops::AdminActionResponse,
            admin::users::DeleteUserResponse,
            admin::connect_config::CreateConnectConfigRequest,
            admin::connect_config::CreateConnectConfigResponse,
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
    let sqlite_store = Arc::new(SqliteConfigStore::new(&db_path.to_string_lossy()).await?);
    let store: Arc<dyn ConfigStore> = sqlite_store.clone();
    let maintenance: Arc<dyn cmdock_server::store::OperatorMaintenanceBackend> = sqlite_store;

    store.run_migrations().await?;

    // Idempotent prefix backfill for users created before #130 (or
    // re-deploys onto an existing DB). Runs once per boot; second pass
    // sees no NULL-prefix rows and exits in O(1).
    match cmdock_server::admin::prefix::backfill_missing_user_prefixes(store.as_ref()).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            assigned = n,
            "Backfilled task-key prefixes for existing users"
        ),
        Err(e) => {
            tracing::warn!(error = %e, "Prefix backfill failed; continuing startup");
        }
    }

    match cmdock_server::admin::prefix::backfill_personal_task_scopes(store.as_ref()).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            created = n,
            "Backfilled Personal Task Scopes for existing Runtime Users"
        ),
        Err(e) => {
            tracing::warn!(error = %e, "Personal Task Scope backfill failed; continuing startup");
        }
    }

    match store.backfill_task_key_allocation_task_scope_ids().await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            stamped = n,
            "Backfilled Task Scope IDs onto task-key allocation rows"
        ),
        Err(e) => tracing::error!(
            error = %e,
            "Task-key allocation Task Scope backfill failed; continuing startup"
        ),
    }
    match store
        .count_task_key_allocations_missing_task_scope_id()
        .await
    {
        Ok(0) => ::metrics::gauge!("task_key_allocations_missing_task_scope_id").set(0.0),
        Ok(n) => {
            ::metrics::gauge!("task_key_allocations_missing_task_scope_id").set(n as f64);
            tracing::warn!(
                missing = n,
                "Task-key allocation rows still missing Task Scope IDs after startup backfill"
            );
        }
        Err(e) => tracing::warn!(
            error = %e,
            "Could not verify task-key allocation Task Scope readiness invariant"
        ),
    }

    if cli.migrate {
        tracing::info!("Migrations complete.");
        return Ok(());
    }

    let state = AppState::new(store, maintenance, &config);
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

    // Recovery may open merged sync stores before the idle-storage reaper starts.
    // This is intentional: startup must finish or fail the forward-recovery
    // gate before serving traffic; any stores opened here are registered with
    // the manager and become reapable as soon as the reaper starts below.
    let gateway_recovery = match cmdock_server::merged_sync_gateway::recovery::recover_all_users(
        &state,
    )
    .await
    {
        Ok(summary) => summary,
        Err(err) => {
            tracing::error!(error = %err, "Merged sync gateway journal recovery failed during startup; refusing to serve traffic");
            return Err(err).context("merged sync gateway journal recovery failed during startup");
        }
    };
    tracing::info!(
        inspected = gateway_recovery.inspected,
        recovered = gateway_recovery.recovered,
        failed = gateway_recovery.failed,
        quarantined = gateway_recovery.quarantined,
        skipped_terminal = gateway_recovery.skipped_terminal,
        stale = gateway_recovery.stale,
        "Merged sync gateway journal recovery complete"
    );

    // Start background reapers for idle sync storage connections (5 min TTL, 60s sweep)
    state.sync_storage_manager.start_reaper();
    state.merged_sync_storage_manager.start_reaper();

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
        .layer(middleware::from_fn(request_id::request_id_middleware))
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([
                    header::AUTHORIZATION,
                    header::CONTENT_TYPE,
                    // Idempotency-Key on POST /api/tasks and modify per
                    // task-write-contract.md § Idempotency. Browser clients
                    // need this allowlisted in the CORS preflight.
                    axum::http::HeaderName::from_static("idempotency-key"),
                    // X-Request-ID may be supplied by a browser client for
                    // correlation; expose it so JS can read the echoed value.
                    axum::http::HeaderName::from_static("x-request-id"),
                ])
                .expose_headers([axum::http::HeaderName::from_static("x-request-id")]),
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

    /// Regression: `task-read-contract.md` § Response makes `GET /api/tasks`
    /// polymorphic on success — `Vec<TaskItem>` for no-params/view shapes,
    /// `TaskBatchLookupResponse` for `?uuids=<csv>`. Document the union as
    /// `oneOf` via `TaskListResponse` so generated clients see both shapes.
    /// (Codex review #109 iteration 1, important issue 1.)
    #[test]
    fn task_list_response_is_oneof_in_openapi() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();

        let task_list_response = &spec["components"]["schemas"]["TaskListResponse"];
        assert!(
            task_list_response.is_object(),
            "TaskListResponse must be registered in components.schemas; got {task_list_response:?}"
        );
        let one_of = &task_list_response["oneOf"];
        assert!(
            one_of.is_array(),
            "TaskListResponse must use oneOf to express the polymorphic 200 body; got {task_list_response:?}"
        );

        // GET /api/tasks 200 response must reference TaskListResponse.
        let get_tasks_200 = &spec["paths"]["/api/tasks"]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        assert_eq!(
            get_tasks_200["$ref"], "#/components/schemas/TaskListResponse",
            "GET /api/tasks 200 must reference TaskListResponse; got {get_tasks_200:?}"
        );

        // Singleton GET /api/tasks/{uuid} 200 must reference TaskItem.
        let get_task_by_id_200 = &spec["paths"]["/api/tasks/{uuid}"]["get"]["responses"]["200"]
            ["content"]["application/json"]["schema"];
        assert_eq!(
            get_task_by_id_200["$ref"], "#/components/schemas/TaskItem",
            "GET /api/tasks/{{uuid}} 200 must reference TaskItem; got {get_task_by_id_200:?}"
        );
    }

    /// Regression: `task-write-contract.md` § Wire exposure makes
    /// `TaskItem.key` a nullable top-level string (`string | null`). The
    /// OpenAPI schema must reflect this so generated clients accept
    /// `null` payloads — Phase 5d deliverable for #130. Field is also
    /// always present (no `skip_serializing_if`); the integration tests
    /// in `tests/tasks_integration.rs` pin the wire-shape side.
    #[test]
    fn task_item_key_is_nullable_in_openapi() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
        let key_schema = &spec["components"]["schemas"]["TaskItem"]["properties"]["key"];
        assert!(
            key_schema.is_object(),
            "TaskItem.key schema must be present; got {key_schema:?}"
        );
        // utoipa emits OpenAPI 3.1 nullable as `"type": ["string", "null"]`
        // (CLAUDE.md § Architecture — "API docs: OpenAPI 3.1.0 via utoipa").
        let type_field = &key_schema["type"];
        let types: Vec<&str> = type_field
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert!(
            types.contains(&"string") && types.contains(&"null"),
            "TaskItem.key must be nullable in OpenAPI 3.1 (`type: [string, null]`) per task-write-contract.md § Wire exposure; got {key_schema:?}",
        );

        // The contract claim is "always present, may be null". OpenAPI
        // expresses always-present via the schema's `required` array —
        // without this, clients are free to read the spec as "may be
        // omitted OR may be null", which is weaker than the contract.
        let required = &spec["components"]["schemas"]["TaskItem"]["required"];
        let req_strs: Vec<&str> = required
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert!(
            req_strs.contains(&"key"),
            "TaskItem.key must be in `required` (always-present-may-be-null per task-write-contract.md § Wire exposure); got required={req_strs:?}",
        );
    }

    /// Regression: `TaskActionResponse.key` is intentionally **omitted**
    /// from the wire when `None` (mutation responses are a different
    /// contract surface from `TaskItem` — only `add_task` populates the
    /// key today; `done`/`undo`/`delete`/`modify` leave it `None`). The
    /// load-bearing wire invariant is "key MAY be absent from the JSON":
    /// utoipa expresses this as `key` NOT appearing in the schema's
    /// `required[]` array. The property's nullability shape is utoipa's
    /// default emission for `Option<T>` and is NOT load-bearing for the
    /// wire — clients use `skip_serializing_if`-driven omission, not
    /// `null`. Pin the absent-from-required invariant so a future
    /// refactor that flips this surface to "always present, may be null"
    /// (matching `TaskItem.key`'s post-Phase-5d shape) lands as an
    /// explicit contract decision, not an accidental wire-break for
    /// legacy mutation endpoints. See `src/tasks/models.rs` docstring
    /// for the rationale (Phase 6 of #130).
    #[test]
    fn task_item_schema_exposes_task_scope_not_deprecated_account() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
        let props = &spec["components"]["schemas"]["TaskItem"]["properties"];
        assert!(props.is_object(), "TaskItem.properties missing: {props:?}");
        assert!(
            props.get("cmdock_task_scope").is_some(),
            "cmdock_task_scope must be declared on TaskItem as a first-class projection field"
        );
        assert!(
            props.get("cmdock_account").is_none(),
            "cmdock_account must NOT be present on TaskItem — deprecated alias removed at beta"
        );
    }

    #[test]
    fn task_action_response_key_is_optional_in_openapi() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
        let key_schema = &spec["components"]["schemas"]["TaskActionResponse"]["properties"]["key"];
        assert!(
            key_schema.is_object(),
            "TaskActionResponse.key schema must be present; got {key_schema:?}"
        );
        // `required` MUST NOT contain `key` — the field is omitted from
        // JSON when `None` per `skip_serializing_if`. This is the wire-
        // meaningful invariant; the property's `type` array (utoipa's
        // default for `Option<T>`) is not part of the contract claim.
        let required = &spec["components"]["schemas"]["TaskActionResponse"]["required"];
        let req_strs: Vec<&str> = required
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert!(
            !req_strs.contains(&"key"),
            "TaskActionResponse.key must NOT be in `required` (legacy mutation responses omit when None); got required={req_strs:?}",
        );
    }

    /// Regression: every task-path endpoint (`GET /api/tasks/{uuid}` and
    /// the four `POST /api/tasks/{uuid}/{modify,done,undo,delete}`)
    /// documents UUID-or-key acceptance in its `uuid` path-parameter
    /// description. Pins the `cmdock/architecture` `task-write-contract.md`
    /// § Resolution semantics surface so a future utoipa annotation edit
    /// that drops the wording is caught at CI.
    #[test]
    fn task_path_endpoints_document_uuid_or_key_in_openapi() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
        // (path, method) — all five endpoints accept a key in the form
        // `<PREFIX>-N` per Phase 3 of #130.
        let surfaces: &[(&str, &str)] = &[
            ("/api/tasks/{uuid}", "get"),
            ("/api/tasks/{uuid}/modify", "post"),
            ("/api/tasks/{uuid}/done", "post"),
            ("/api/tasks/{uuid}/undo", "post"),
            ("/api/tasks/{uuid}/delete", "post"),
        ];
        for (path, method) in surfaces {
            let params = &spec["paths"][path][method]["parameters"];
            let arr = params
                .as_array()
                .unwrap_or_else(|| panic!("{method} {path}: parameters[] missing; got {params:?}"));
            let uuid_param = arr
                .iter()
                .find(|p| p["name"].as_str() == Some("uuid"))
                .unwrap_or_else(|| {
                    panic!("{method} {path}: no `uuid` path param found; got {params:?}")
                });
            let desc = uuid_param["description"].as_str().unwrap_or_default();
            assert!(
                desc.contains("task key") || desc.contains("<PREFIX>-N"),
                "{method} {path}: `uuid` description must mention task-key acceptance per task-write-contract.md § Resolution semantics; got {desc:?}",
            );
        }
    }
}
