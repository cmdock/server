pub mod handlers;

use crate::app_state::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/app-config",
            axum::routing::get(handlers::get_app_config),
        )
        .route(
            "/api/shopping-config",
            axum::routing::put(handlers::upsert_shopping_config),
        )
        .route(
            "/api/shopping-config",
            axum::routing::delete(handlers::delete_shopping_config),
        )
        .route("/api/contexts", axum::routing::get(handlers::list_contexts))
        .route(
            "/api/contexts/{id}",
            axum::routing::put(handlers::upsert_context),
        )
        .route(
            "/api/contexts/{id}",
            axum::routing::delete(handlers::delete_context),
        )
        .route("/api/stores", axum::routing::get(handlers::list_stores))
        .route(
            "/api/stores/{id}",
            axum::routing::put(handlers::upsert_store),
        )
        .route(
            "/api/stores/{id}",
            axum::routing::delete(handlers::delete_store),
        )
        .route(
            "/api/presets/{id}",
            axum::routing::put(handlers::upsert_preset),
        )
        .route(
            "/api/presets/{id}",
            axum::routing::delete(handlers::delete_preset),
        )
}
