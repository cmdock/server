pub mod filter;
pub mod handlers;
pub mod models;
pub mod mutations;
pub mod parser;
pub mod service;
pub mod urgency;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use taskchampion::Task;

use crate::app_state::AppState;
use axum::Router;

/// Parse the `scheduled` property from a TaskChampion task.
///
/// TC stores `scheduled` as a generic string property containing epoch seconds.
/// This helper centralises the parsing so callers don't duplicate the
/// `get_value → parse::<i64> → from_timestamp` chain.
pub fn parse_task_scheduled(task: &Task) -> Option<DateTime<Utc>> {
    task.get_value("scheduled")
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|secs| DateTime::from_timestamp(secs, 0))
}

/// Keys filtered from the UDA pass-through. Includes:
/// - All explicit `TaskItem` JSON field names (prevent duplicate keys via flatten)
/// - TC properties we consume internally but don't expose on TaskItem
///   (e.g. `scheduled` — used by urgency/filter but stored as raw epoch seconds)
/// - TC recurrence/internal keys that may arrive via sync from other clients
const TASKITEM_RESERVED_KEYS: &[&str] = &[
    // Explicit TaskItem fields
    "uuid",
    "description",
    "project",
    "tags",
    "priority",
    "due",
    "urgency",
    "depends",
    "blocked",
    "waiting",
    "status",
    // Internal TC properties consumed by urgency/filter
    "scheduled",
    // Recurrence/internal keys (may arrive via sync)
    "recur",
    "until",
    "mask",
    "imask",
    "parent",
];

/// Extract user-defined attributes (UDAs) from a TaskChampion task.
///
/// Returns all properties not covered by the explicit `TaskItem` schema.
/// TC's `get_user_defined_attributes()` filters out its own `Prop` enum
/// members (`description`, `due`, `status`, etc.), tags (`tag_*`),
/// annotations (`annotation_*`), and dependencies (`dep_*`). We additionally
/// filter keys that TC considers user-defined but our schema consumes
/// explicitly (e.g. `project`, `scheduled`).
pub fn extract_udas(task: &Task) -> HashMap<String, String> {
    task.get_user_defined_attributes()
        .filter(|(k, _)| !TASKITEM_RESERVED_KEYS.contains(k))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

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
