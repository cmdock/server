pub mod handlers;
pub mod service;

use axum::Router;

use crate::app_state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/devices", axum::routing::get(handlers::list_devices))
        .route(
            "/api/devices",
            axum::routing::post(handlers::register_device),
        )
        .route(
            "/api/devices/{client_id}",
            axum::routing::delete(handlers::revoke_device),
        )
        .route(
            "/api/devices/{client_id}",
            axum::routing::patch(handlers::rename_device),
        )
}
