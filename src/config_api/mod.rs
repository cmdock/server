pub mod handlers;

use crate::app_state::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    // Legacy compatibility routes for clients that still use generic config
    // instead of the newer typed resource surfaces.
    Router::new()
        .route(
            "/api/config/{config_type}",
            axum::routing::get(handlers::get_config),
        )
        .route(
            "/api/config/{config_type}",
            axum::routing::post(handlers::upsert_config),
        )
        .route(
            "/api/config/{config_type}/{item_id}",
            axum::routing::delete(handlers::delete_config_item),
        )
}
