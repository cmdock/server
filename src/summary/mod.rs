pub mod handlers;
pub mod llm;

use crate::app_state::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/summary", axum::routing::get(handlers::get_summary))
}
