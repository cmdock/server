pub mod filter;
pub mod handlers;
pub mod models;
pub mod mutations;
pub mod parser;
pub mod service;
pub mod urgency;

use crate::app_state::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/tasks", axum::routing::get(handlers::list_tasks))
        .route("/api/tasks", axum::routing::post(handlers::add_task))
        .route(
            "/api/tasks/{uuid}/done",
            axum::routing::post(handlers::complete_task),
        )
        .route(
            "/api/tasks/{uuid}/undo",
            axum::routing::post(handlers::undo_task),
        )
        .route(
            "/api/tasks/{uuid}/delete",
            axum::routing::post(handlers::delete_task),
        )
        .route(
            "/api/tasks/{uuid}/modify",
            axum::routing::post(handlers::modify_task),
        )
}
