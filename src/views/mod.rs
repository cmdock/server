pub mod defaults;
pub mod handlers;

use crate::app_state::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/views", axum::routing::get(handlers::list_views))
        .route("/api/views/{id}", axum::routing::put(handlers::upsert_view))
        .route(
            "/api/views/{id}",
            axum::routing::delete(handlers::delete_view),
        )
}
