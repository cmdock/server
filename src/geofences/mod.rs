pub mod handlers;

use crate::app_state::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/geofences",
            axum::routing::get(handlers::list_geofences),
        )
        .route(
            "/api/geofences/{id}",
            axum::routing::put(handlers::upsert_geofence),
        )
        .route(
            "/api/geofences/{id}",
            axum::routing::delete(handlers::delete_geofence),
        )
}
