pub mod backup;
pub mod bootstrap;
pub mod cli;
pub mod connect_config;
pub mod console;
pub mod devices;
pub mod handlers;
pub mod openapi;
pub mod recovery;
pub mod runtime_ops;
pub mod runtime_policy;
pub mod services;
pub mod sync_identity;
pub mod user_diagnostics;
pub mod user_lifecycle;
pub mod users;
pub mod webhooks;

use crate::app_state::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/console", axum::routing::get(console::console_shell))
        .route(
            "/admin/console/style.css",
            axum::routing::get(console::console_styles),
        )
        .route(
            "/admin/console/app.js",
            axum::routing::get(console::console_script),
        )
        .route("/admin/status", axum::routing::get(handlers::server_status))
        .route("/admin/users", axum::routing::get(users::list_users))
        .route(
            "/admin/webhooks",
            axum::routing::get(webhooks::list_webhooks).post(webhooks::create_webhook),
        )
        .route(
            "/admin/webhooks/{id}",
            axum::routing::get(webhooks::get_webhook)
                .patch(webhooks::update_webhook)
                .delete(webhooks::delete_webhook),
        )
        .route(
            "/admin/webhooks/{id}/test",
            axum::routing::post(webhooks::test_webhook),
        )
        .route("/admin/backup", axum::routing::post(backup::create_backup))
        .route(
            "/admin/backup/list",
            axum::routing::get(backup::list_backups),
        )
        .route(
            "/admin/backup/restore",
            axum::routing::post(backup::restore_backup),
        )
        .route(
            "/admin/bootstrap/user-device",
            axum::routing::post(bootstrap::bootstrap_user_device),
        )
        .route(
            "/admin/bootstrap/{bootstrap_request_id}/ack",
            axum::routing::post(bootstrap::acknowledge_bootstrap_request),
        )
        .route(
            "/admin/user/{user_id}/connect-config",
            axum::routing::post(connect_config::create_connect_config),
        )
        .route(
            "/admin/user/{user_id}/sync-identity",
            axum::routing::get(sync_identity::get_sync_identity),
        )
        .route(
            "/admin/user/{user_id}/sync-identity/ensure",
            axum::routing::post(sync_identity::ensure_sync_identity),
        )
        .route(
            "/admin/user/{user_id}/runtime-policy",
            axum::routing::get(runtime_policy::get_runtime_policy)
                .put(runtime_policy::apply_runtime_policy),
        )
        .route(
            "/admin/user/{user_id}/devices",
            axum::routing::get(devices::list_devices).post(devices::create_device),
        )
        .route(
            "/admin/user/{user_id}/devices/{client_id}",
            axum::routing::get(devices::get_device)
                .patch(devices::rename_device)
                .delete(devices::delete_device),
        )
        .route(
            "/admin/user/{user_id}/devices/{client_id}/revoke",
            axum::routing::post(devices::revoke_device),
        )
        .route(
            "/admin/user/{user_id}/devices/{client_id}/unrevoke",
            axum::routing::post(devices::unrevoke_device),
        )
        .route(
            "/admin/user/{user_id}/stats",
            axum::routing::get(user_diagnostics::user_stats),
        )
        .route(
            "/admin/user/{user_id}",
            axum::routing::delete(user_lifecycle::delete_user),
        )
        .route(
            "/admin/user/{user_id}/evict",
            axum::routing::post(runtime_ops::evict_replica),
        )
        .route(
            "/admin/user/{user_id}/checkpoint",
            axum::routing::post(runtime_ops::checkpoint_replica),
        )
        .route(
            "/admin/user/{user_id}/offline",
            axum::routing::post(runtime_ops::quarantine_user),
        )
        .route(
            "/admin/user/{user_id}/online",
            axum::routing::post(runtime_ops::unquarantine_user),
        )
}
