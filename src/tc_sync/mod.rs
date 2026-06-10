pub(crate) mod auth;
pub mod bridge;
pub mod crypto;
pub mod cryptor_cache;
mod edge;
// Re-export the pure content-type matcher for the `tc_sync_content_type` fuzz
// target (the module itself stays private).
pub use edge::content_type_matches;
pub(crate) mod events;
mod gateway_thread;
pub mod handlers;
pub(crate) mod payloads;
pub mod runtime;
pub mod storage;

use crate::app_state::AppState;
use axum::extract::DefaultBodyLimit;
use axum::Router;
use tower_http::limit::RequestBodyLimitLayer;

/// Routes for the TaskChampion sync protocol.
///
/// Auth: X-Client-Id header → devices table → user_id → merged gateway storage.
/// The TC CLI doesn't support bearer tokens, so sync endpoints authenticate via
/// per-device `client_id` lookup. Devices are provisioned via the device
/// registry and served by `MergedSyncGateway` at
/// `data/users/{user_id}/merged/sync.sqlite`.
///
/// Body size limit: 10 MiB (enforced at both the Tower layer and Axum extractor level).
/// The Tower `RequestBodyLimitLayer` rejects oversized bodies before buffering.
/// The Axum `DefaultBodyLimit` overrides the framework's default 2 MiB limit for
/// `Bytes` extraction.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/client/add-version/{parent_version_id}",
            axum::routing::post(handlers::add_version),
        )
        .route(
            "/v1/client/get-child-version/{parent_version_id}",
            axum::routing::get(handlers::get_child_version),
        )
        .route(
            "/v1/client/add-snapshot/{version_id}",
            axum::routing::post(handlers::add_snapshot),
        )
        .route(
            "/v1/client/snapshot",
            axum::routing::get(handlers::get_snapshot),
        )
        .layer(RequestBodyLimitLayer::new(10 * 1024 * 1024)) // 10 MiB (Tower layer)
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024)) // 10 MiB (Axum extractor limit)
}
