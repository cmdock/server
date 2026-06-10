use crate::app_state::AppState;
use axum::Router;

pub mod handlers;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/me", axum::routing::get(handlers::get_me))
}
