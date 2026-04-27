pub mod api;
pub mod delivery;
pub mod handlers;
pub mod scheduler;
pub mod security;
pub mod summary;

use crate::app_state::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/webhooks",
            axum::routing::get(handlers::list_webhooks).post(handlers::create_webhook),
        )
        .route(
            "/api/webhooks/{id}",
            axum::routing::get(handlers::get_webhook)
                .put(handlers::update_webhook)
                .delete(handlers::delete_webhook),
        )
        .route(
            "/api/webhooks/{id}/test",
            axum::routing::post(handlers::test_webhook),
        )
}
