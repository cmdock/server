use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use taskchampion::Status;
use uuid::Uuid;

use std::collections::HashSet;
use std::time::Instant;

use super::models::{AddTaskRequest, ModifyTaskRequest, TaskActionResponse, TaskItem};
use super::mutations::{self, TaskMutationKind};
use super::service;
use crate::app_state::AppState;
use crate::auth::AuthUser;
use crate::metrics as m;
use crate::replica;
use crate::user_runtime::{handle_replica_error, open_user_replica};

#[derive(Deserialize, utoipa::IntoParams)]
pub struct TaskListQuery {
    /// View ID to filter tasks by (looks up filter expression from views table)
    pub view: Option<String>,
}

/// List tasks, optionally filtered by a view definition.
///
/// Without a `view` parameter, returns all **pending** tasks.
/// With a `view` parameter, applies the view's filter expression
/// which may include non-pending tasks depending on the filter.
#[utoipa::path(
    get,
    path = "/api/tasks",
    operation_id = "listTasks",
    params(TaskListQuery),
    responses(
        (status = 200, description = "List of tasks", body = Vec<TaskItem>),
        (status = 401, description = "Unauthorised"),
        (status = 404, description = "View not found (when view parameter is specified)"),
        (status = 500, description = "Internal server error")
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id, view = ?query.view))]
pub async fn list_tasks(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(query): Query<TaskListQuery>,
) -> Result<Json<Vec<TaskItem>>, StatusCode> {
    let rep_arc = open_user_replica(&state, &auth.user_id, "api").await?;

    // If a view is specified, look up its filter from the config store.
    // Treat empty string as "no view" (client may send ?view= with no value).
    let filter = if let Some(view_id) = query.view.as_deref().filter(|v| !v.is_empty()) {
        if let Err(e) =
            crate::views::defaults::reconcile_default_views(state.store.as_ref(), &auth.user_id)
                .await
        {
            tracing::warn!("Failed to reconcile default views before task list: {e}");
        }
        let views = state.store.list_views(&auth.user_id).await.map_err(|e| {
            tracing::error!("Failed to list views: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        let view = views
            .into_iter()
            .find(|v| v.id == view_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        Some(view.filter)
    } else {
        None
    };

    // Fetch data under lock, then drop lock before CPU-intensive filter/map
    let tasks: Vec<TaskItem> = match filter {
        Some(ref filter_str) => {
            let read_start = Instant::now();
            let all = {
                let mut rep = rep_arc.lock().await;
                let result = rep.all_tasks().await.map_err(|e| {
                    m::record_replica_op("all_tasks", read_start.elapsed().as_secs_f64(), "error");
                    handle_replica_error(&state, &auth.user_id, &e, "all_tasks", "api")
                })?;
                m::record_replica_op("all_tasks", read_start.elapsed().as_secs_f64(), "ok");
                result
            }; // lock dropped here — filter/map runs without holding it

            let filter_start = Instant::now();
            let tasks_scanned = all.len();
            let pending_uuids: HashSet<Uuid> = all
                .values()
                .filter(|t| t.get_status() == Status::Pending)
                .map(|t| t.get_uuid())
                .collect();
            // Parse filter once + shared time context (zero per-task allocation)
            let parsed_filter = super::filter::parse_filter(filter_str);
            let eval_ctx = super::filter::EvalCtx::new();
            let result: Vec<TaskItem> = all
                .values()
                .filter(|t| super::filter::matches_with_context(t, &parsed_filter, &eval_ctx))
                .map(|t| replica::task_to_item(t, Some(&pending_uuids)))
                .collect();
            m::record_filter_eval(
                filter_start.elapsed().as_secs_f64(),
                tasks_scanned,
                result.len(),
            );
            result
        }
        None => {
            let read_start = Instant::now();
            let pending = {
                let mut rep = rep_arc.lock().await;
                let result = rep.pending_tasks().await.map_err(|e| {
                    m::record_replica_op(
                        "pending_tasks",
                        read_start.elapsed().as_secs_f64(),
                        "error",
                    );
                    handle_replica_error(&state, &auth.user_id, &e, "pending_tasks", "api")
                })?;
                m::record_replica_op("pending_tasks", read_start.elapsed().as_secs_f64(), "ok");
                result
            }; // lock dropped before map
            let pending_uuids: HashSet<Uuid> = pending
                .iter()
                .filter(|t| t.get_status() == Status::Pending)
                .map(|t| t.get_uuid())
                .collect();
            pending
                .iter()
                // TaskChampion's pending_tasks() should already exclude deleted tasks,
                // but enforce the API contract here in case it returns stale/deleted rows.
                .filter(|task| task.get_status() == Status::Pending)
                .map(|t| replica::task_to_item(t, Some(&pending_uuids)))
                .collect()
        }
    };

    Ok(Json(tasks))
}

/// Add a new task using Taskwarrior raw syntax.
#[utoipa::path(
    post,
    path = "/api/tasks",
    operation_id = "addTask",
    request_body = AddTaskRequest,
    responses(
        (status = 200, description = "Task created", body = TaskActionResponse),
        (status = 400, description = "Invalid task payload"),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error")
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id))]
pub async fn add_task(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Json(body): Json<AddTaskRequest>,
) -> Result<Json<TaskActionResponse>, StatusCode> {
    if let Err(status) = crate::validation::validate_or_bad_request(&body, "Invalid task payload") {
        mutations::log_rejected(
            &headers,
            &state,
            &auth.user_id,
            TaskMutationKind::Create,
            None,
            "invalid_payload",
        );
        return Err(status);
    }
    let outcome = match service::add_task(&state, &auth.user_id, &body).await {
        Ok(outcome) => outcome,
        Err(status) => {
            mutations::log_failed_status(
                &headers,
                &state,
                &auth.user_id,
                TaskMutationKind::Create,
                None,
                status,
            );
            return Err(status);
        }
    };

    mutations::finalize_success(
        &state,
        &headers,
        &auth.user_id,
        outcome.kind,
        outcome.uuid,
        outcome.task_item,
        outcome.changed_fields,
        outcome.audit,
    )
    .await;

    Ok(Json(TaskActionResponse {
        success: true,
        output: format!("Created task {}.", outcome.uuid),
    }))
}

/// Mark a task as completed.
///
/// Uses POST (not PUT/PATCH) for backwards compatibility with the iOS app.
/// Returns 409 Conflict if the task was concurrently deleted by another request.
#[utoipa::path(
    post,
    path = "/api/tasks/{uuid}/done",
    operation_id = "completeTask",
    params(("uuid" = String, Path, description = "Task UUID", format = "uuid")),
    responses(
        (status = 200, description = "Task completed", body = TaskActionResponse),
        (status = 400, description = "Invalid UUID"),
        (status = 401, description = "Unauthorised"),
        (status = 404, description = "Task not found"),
        (status = 409, description = "Conflict — task was concurrently modified or deleted"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id, uuid = %uuid_str))]
pub async fn complete_task(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(uuid_str): Path<String>,
) -> Result<Json<TaskActionResponse>, StatusCode> {
    let uuid = match Uuid::parse_str(&uuid_str) {
        Ok(uuid) => uuid,
        Err(_) => {
            mutations::log_rejected(
                &headers,
                &state,
                &auth.user_id,
                TaskMutationKind::Complete,
                None,
                "invalid_uuid",
            );
            return Err(StatusCode::BAD_REQUEST);
        }
    };
    let outcome = match service::complete_task(&state, &auth.user_id, uuid).await {
        Ok(outcome) => outcome,
        Err(status) => {
            mutations::log_failed_status(
                &headers,
                &state,
                &auth.user_id,
                TaskMutationKind::Complete,
                Some(uuid),
                status,
            );
            return Err(status);
        }
    };

    mutations::finalize_success(
        &state,
        &headers,
        &auth.user_id,
        outcome.kind,
        outcome.uuid,
        outcome.task_item,
        outcome.changed_fields,
        outcome.audit,
    )
    .await;

    Ok(Json(TaskActionResponse {
        success: true,
        output: format!("Completed task {}.", outcome.uuid),
    }))
}

/// Soft-delete a task (sets status to deleted).
///
/// Uses POST (not DELETE) for backwards compatibility with the iOS app.
/// Returns 409 Conflict if the task was concurrently modified.
#[utoipa::path(
    post,
    path = "/api/tasks/{uuid}/undo",
    operation_id = "undoTask",
    params(("uuid" = String, Path, description = "Task UUID", format = "uuid")),
    responses(
        (status = 200, description = "Task marked pending again", body = TaskActionResponse),
        (status = 400, description = "Invalid UUID"),
        (status = 401, description = "Unauthorised"),
        (status = 404, description = "Task not found"),
        (status = 409, description = "Conflict — task is not currently completed"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id, uuid = %uuid_str))]
pub async fn undo_task(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(uuid_str): Path<String>,
) -> Result<Json<TaskActionResponse>, StatusCode> {
    let uuid = match Uuid::parse_str(&uuid_str) {
        Ok(uuid) => uuid,
        Err(_) => {
            mutations::log_rejected(
                &headers,
                &state,
                &auth.user_id,
                TaskMutationKind::Undo,
                None,
                "invalid_uuid",
            );
            return Err(StatusCode::BAD_REQUEST);
        }
    };
    let outcome = match service::undo_task(&state, &auth.user_id, uuid).await {
        Ok(outcome) => outcome,
        Err(status) => {
            mutations::log_failed_status(
                &headers,
                &state,
                &auth.user_id,
                TaskMutationKind::Undo,
                Some(uuid),
                status,
            );
            return Err(status);
        }
    };

    mutations::finalize_success(
        &state,
        &headers,
        &auth.user_id,
        outcome.kind,
        outcome.uuid,
        outcome.task_item,
        outcome.changed_fields,
        outcome.audit,
    )
    .await;

    Ok(Json(TaskActionResponse {
        success: true,
        output: format!("Reopened task {}.", outcome.uuid),
    }))
}

/// Soft-delete a task (sets status to deleted).
///
/// Uses POST (not DELETE) for backwards compatibility with the iOS app.
/// Returns 409 Conflict if the task was concurrently modified.
#[utoipa::path(
    post,
    path = "/api/tasks/{uuid}/delete",
    operation_id = "deleteTask",
    params(("uuid" = String, Path, description = "Task UUID", format = "uuid")),
    responses(
        (status = 200, description = "Task deleted", body = TaskActionResponse),
        (status = 400, description = "Invalid UUID"),
        (status = 401, description = "Unauthorised"),
        (status = 404, description = "Task not found"),
        (status = 409, description = "Conflict — task was concurrently modified"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id, uuid = %uuid_str))]
pub async fn delete_task(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(uuid_str): Path<String>,
) -> Result<Json<TaskActionResponse>, StatusCode> {
    let uuid = match Uuid::parse_str(&uuid_str) {
        Ok(uuid) => uuid,
        Err(_) => {
            mutations::log_rejected(
                &headers,
                &state,
                &auth.user_id,
                TaskMutationKind::Delete,
                None,
                "invalid_uuid",
            );
            return Err(StatusCode::BAD_REQUEST);
        }
    };
    let outcome = match service::delete_task(&state, &auth.user_id, uuid).await {
        Ok(outcome) => outcome,
        Err(status) => {
            mutations::log_failed_status(
                &headers,
                &state,
                &auth.user_id,
                TaskMutationKind::Delete,
                Some(uuid),
                status,
            );
            return Err(status);
        }
    };

    mutations::finalize_success(
        &state,
        &headers,
        &auth.user_id,
        outcome.kind,
        outcome.uuid,
        outcome.task_item,
        outcome.changed_fields,
        outcome.audit,
    )
    .await;

    Ok(Json(TaskActionResponse {
        success: true,
        output: format!("Deleted task {}.", outcome.uuid),
    }))
}

/// Modify task fields. Only provided fields are updated.
///
/// Uses POST (not PATCH) for backwards compatibility with the iOS app.
/// Returns 409 Conflict if the task was concurrently deleted.
#[utoipa::path(
    post,
    path = "/api/tasks/{uuid}/modify",
    operation_id = "modifyTask",
    params(("uuid" = String, Path, description = "Task UUID", format = "uuid")),
    request_body = ModifyTaskRequest,
    responses(
        (status = 200, description = "Task modified", body = TaskActionResponse),
        (status = 400, description = "Invalid UUID or task payload"),
        (status = 401, description = "Unauthorised"),
        (status = 404, description = "Task not found"),
        (status = 409, description = "Conflict — task was concurrently deleted"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id, uuid = %uuid_str))]
pub async fn modify_task(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(uuid_str): Path<String>,
    Json(body): Json<ModifyTaskRequest>,
) -> Result<Json<TaskActionResponse>, StatusCode> {
    let uuid = match Uuid::parse_str(&uuid_str) {
        Ok(uuid) => uuid,
        Err(_) => {
            mutations::log_rejected(
                &headers,
                &state,
                &auth.user_id,
                TaskMutationKind::Modify,
                None,
                "invalid_uuid",
            );
            return Err(StatusCode::BAD_REQUEST);
        }
    };
    if let Err(status) = crate::validation::validate_or_bad_request(&body, "Invalid task payload") {
        mutations::log_rejected(
            &headers,
            &state,
            &auth.user_id,
            TaskMutationKind::Modify,
            Some(uuid),
            "invalid_payload",
        );
        return Err(status);
    }
    let parsed_depends = match service::parse_modify_dependencies(uuid, body.depends.as_ref()) {
        Ok(value) => value,
        Err(reason) => {
            mutations::log_rejected(
                &headers,
                &state,
                &auth.user_id,
                TaskMutationKind::Modify,
                Some(uuid),
                reason,
            );
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    let outcome =
        match service::modify_task(&state, &auth.user_id, uuid, &body, parsed_depends).await {
            Ok(outcome) => outcome,
            Err(status) => {
                mutations::log_failed_status(
                    &headers,
                    &state,
                    &auth.user_id,
                    TaskMutationKind::Modify,
                    Some(uuid),
                    status,
                );
                return Err(status);
            }
        };

    mutations::finalize_success(
        &state,
        &headers,
        &auth.user_id,
        outcome.kind,
        outcome.uuid,
        outcome.task_item,
        outcome.changed_fields,
        outcome.audit,
    )
    .await;

    Ok(Json(TaskActionResponse {
        success: true,
        output: format!("Modified task {}.", outcome.uuid),
    }))
}
