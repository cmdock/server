pub mod handlers;

use crate::app_state::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new().route("/healthz", axum::routing::get(handlers::healthz))
}
