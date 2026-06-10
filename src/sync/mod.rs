pub mod handlers;

use crate::app_state::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/sync", axum::routing::post(handlers::sync))
}
