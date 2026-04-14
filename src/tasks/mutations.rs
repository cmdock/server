use axum::http::{HeaderMap, StatusCode};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::audit;
use crate::tasks::models::TaskItem;
use crate::webhooks;

#[derive(Clone, Copy)]
pub enum TaskMutationKind {
    Create,
    Complete,
    Undo,
    Delete,
    Modify,
}

impl TaskMutationKind {
    fn action(self) -> &'static str {
        match self {
            Self::Create => "task.create",
            Self::Complete => "task.complete",
            Self::Undo => "task.undo",
            Self::Delete => "task.delete",
            Self::Modify => "task.modify",
        }
    }

    fn webhook_event(self) -> &'static str {
        match self {
            Self::Create => "task.created",
            Self::Complete => "task.completed",
            Self::Undo | Self::Modify => "task.modified",
            Self::Delete => "task.deleted",
        }
    }

    fn should_clear_scheduler_history(self, changed_fields: Option<&[String]>) -> bool {
        match self {
            Self::Complete | Self::Undo | Self::Delete => true,
            Self::Modify => changed_fields
                .unwrap_or(&[])
                .iter()
                .any(|field| field == "due" || field == "status"),
            Self::Create => false,
        }
    }
}

pub enum TaskMutationAudit {
    None,
    Create {
        project: Option<String>,
        priority: Option<String>,
    },
    Modify {
        changed_description: bool,
        changed_project: bool,
        changed_priority: bool,
        changed_due: bool,
        changed_tags: bool,
        changed_depends: bool,
    },
}

pub fn changed_fields(before: &TaskItem, after: &TaskItem) -> Vec<String> {
    let mut changed = Vec::new();
    if before.description != after.description {
        changed.push("description".to_string());
    }
    if before.project != after.project {
        changed.push("project".to_string());
    }
    if before.priority != after.priority {
        changed.push("priority".to_string());
    }
    if before.due != after.due {
        changed.push("due".to_string());
    }
    if before.tags != after.tags {
        changed.push("tags".to_string());
    }
    if before.status != after.status {
        changed.push("status".to_string());
    }
    if before.blocked != after.blocked {
        changed.push("blocked".to_string());
    }
    if before.waiting != after.waiting {
        changed.push("waiting".to_string());
    }
    changed
}

pub fn log_rejected(
    headers: &HeaderMap,
    state: &AppState,
    user_id: &str,
    kind: TaskMutationKind,
    task_id: Option<Uuid>,
    reason: &str,
) {
    log_queue_mutation_boundary(
        "queue.mutation_rejected",
        headers,
        state,
        user_id,
        kind.action(),
        task_id,
        Some(reason),
    );
}

pub fn log_failed_status(
    headers: &HeaderMap,
    state: &AppState,
    user_id: &str,
    kind: TaskMutationKind,
    task_id: Option<Uuid>,
    status: StatusCode,
) {
    let event = if status == StatusCode::CONFLICT {
        "queue.mutation_conflicted"
    } else {
        "queue.mutation_rejected"
    };
    log_queue_mutation_boundary(
        event,
        headers,
        state,
        user_id,
        kind.action(),
        task_id,
        Some(status.as_str()),
    );
}

// Params mix request infrastructure (state, headers) with mutation semantics;
// a struct split would be forced cohesion, so allow the arity.
#[allow(clippy::too_many_arguments)]
pub async fn finalize_success(
    state: &AppState,
    headers: &HeaderMap,
    user_id: &str,
    kind: TaskMutationKind,
    uuid: Uuid,
    task_item: TaskItem,
    changed_fields: Option<Vec<String>>,
    audit_fields: TaskMutationAudit,
) {
    match audit_fields {
        TaskMutationAudit::None => {
            tracing::info!(
                target: "audit",
                action = kind.action(),
                source = "api",
                user_id = %user_id,
                client_ip = %audit::client_ip(headers, state.config.server.trust_forwarded_headers),
                task_id = %uuid,
            );
        }
        TaskMutationAudit::Create { project, priority } => {
            tracing::info!(
                target: "audit",
                action = kind.action(),
                source = "api",
                user_id = %user_id,
                client_ip = %audit::client_ip(headers, state.config.server.trust_forwarded_headers),
                task_id = %uuid,
                project = %project.as_deref().unwrap_or(""),
                priority = %priority.as_deref().unwrap_or(""),
            );
        }
        TaskMutationAudit::Modify {
            changed_description,
            changed_project,
            changed_priority,
            changed_due,
            changed_tags,
            changed_depends,
        } => {
            tracing::info!(
                target: "audit",
                action = kind.action(),
                source = "api",
                user_id = %user_id,
                client_ip = %audit::client_ip(headers, state.config.server.trust_forwarded_headers),
                task_id = %uuid,
                changed_description,
                changed_project,
                changed_priority,
                changed_due,
                changed_tags,
                changed_depends,
            );
        }
    }

    log_queue_mutation_boundary(
        "queue.mutation_accepted",
        headers,
        state,
        user_id,
        kind.action(),
        Some(uuid),
        None,
    );

    state
        .runtime_sync
        .note_canonical_change(user_id, "rest_write");
    if kind.should_clear_scheduler_history(changed_fields.as_deref()) {
        clear_webhook_scheduler_history(state, user_id, &uuid).await;
    }
    webhooks::delivery::emit_task_event(
        state,
        user_id,
        kind.webhook_event(),
        task_item,
        changed_fields,
        audit::request_id(headers),
    )
    .await;
}

fn log_queue_mutation_boundary(
    event_name: &'static str,
    headers: &HeaderMap,
    state: &AppState,
    user_id: &str,
    mutation_kind: &'static str,
    task_id: Option<Uuid>,
    reason: Option<&str>,
) {
    let (Some(session_id), Some(mutation_id)) =
        (audit::session_id(headers), audit::mutation_id(headers))
    else {
        return;
    };

    let client_ip = audit::client_ip(headers, state.config.server.trust_forwarded_headers);
    match event_name {
        "queue.mutation_rejected" => tracing::error!(
            target: "boundary",
            event = event_name,
            component = "cmdock/server",
            correlation_id = %session_id,
            request_id = ?audit::request_id(headers),
            session_id = %session_id,
            mutation_id = %mutation_id,
            mutation_kind = mutation_kind,
            user_id = %user_id,
            task_id = ?task_id,
            client_ip = %client_ip,
            reason = ?reason,
        ),
        _ => tracing::info!(
            target: "boundary",
            event = event_name,
            component = "cmdock/server",
            correlation_id = %session_id,
            request_id = ?audit::request_id(headers),
            session_id = %session_id,
            mutation_id = %mutation_id,
            mutation_kind = mutation_kind,
            user_id = %user_id,
            task_id = ?task_id,
            client_ip = %client_ip,
            reason = ?reason,
        ),
    }
}

async fn clear_webhook_scheduler_history(state: &AppState, user_id: &str, task_uuid: &Uuid) {
    if let Err(err) = state
        .store
        .clear_webhook_event_history(user_id, &task_uuid.to_string())
        .await
    {
        tracing::warn!(
            user_id = %user_id,
            task_uuid = %task_uuid,
            error = %err,
            "Failed to clear webhook scheduler history"
        );
    }
}
