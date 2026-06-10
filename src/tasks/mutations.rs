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
    if before.depends != after.depends {
        changed.push("depends".to_string());
    }
    // Emit individual UDA field names that changed (sorted for stable webhook payloads)
    if before.extra != after.extra {
        let mut uda_changes: Vec<&String> = before
            .extra
            .keys()
            .chain(after.extra.keys())
            .filter(|k| before.extra.get(*k) != after.extra.get(*k))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        uda_changes.sort();
        changed.extend(uda_changes.into_iter().cloned());
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
    // Webhook target lookup + delivery moves OFF the synchronous response path
    // (#149): `deliver` retries inline (1s/10s/60s), so a slow/dead endpoint
    // could otherwise stall the mutation response for the full retry budget.
    // Spawn it instead. The tracker is entered HERE (before spawn) so a
    // quiescence check after the response returns already sees this pending
    // dispatch; the guard moves into the future and decrements on completion
    // or panic. `clear_webhook_scheduler_history` above intentionally stays
    // synchronous (scheduler dedup state, ordering-sensitive, single write).
    //
    // Semantics note (as-built; the contract is silent on this): the task
    // PAYLOAD is snapshotted at mutation time (`task_item` is captured here),
    // but webhook SUBSCRIPTION matching now happens at DISPATCH time inside the
    // spawned task. A client that changes its webhook subscriptions in the
    // window between the 200 response and the spawned lookup may see the event
    // matched against the updated subscription set. This is the conventional
    // semantic for async delivery and is inherent to moving the lookup off the
    // response path. webhook-contract.md § Event Filtering makes no timing
    // claim; surfaced to the arch team for an optional clarification.
    // #156: dispatch is bounded. `try_enter` admits under capacity (permit held
    // by the guard for the dispatch's lifetime) or returns None at capacity, in
    // which case we SHED — drop the event + record it. A shed is a permanent
    // drop (no retry); capacity is biased high so only pathological pile-up
    // (dead hooks holding permits ~71s) sheds, not a legitimate burst.
    match state.webhook_dispatch.try_enter() {
        Some(dispatch_guard) => {
            let dispatch_state = state.clone();
            let dispatch_user_id = user_id.to_string();
            let dispatch_event = kind.webhook_event();
            let dispatch_request_id = audit::request_id(headers);
            tokio::spawn(async move {
                let _dispatch_guard = dispatch_guard;
                webhooks::delivery::emit_task_event(
                    &dispatch_state,
                    &dispatch_user_id,
                    dispatch_event,
                    task_item,
                    changed_fields,
                    dispatch_request_id,
                )
                .await;
            });
        }
        None => {
            ::metrics::counter!("webhook_dispatch_shed_total").increment(1);
            tracing::warn!(
                user_id = %user_id,
                event = kind.webhook_event(),
                capacity = state.webhook_dispatch.capacity(),
                in_flight = state.webhook_dispatch.in_flight(),
                "webhook dispatch shed: at capacity (CMDOCK_WEBHOOK_DISPATCH_MAX_INFLIGHT); event dropped without delivery",
            );
        }
    }
}

// ADR-0002 HC-1 exception: 7 args, two over threshold. This is a focused
// emitter for `target = "boundary"` events on the queue mutation flow
// (mutation_accepted / rejected / failed). Each arg has a distinct role
// at the boundary-event level — collapsing into a struct adds a layer
// for callers (mostly inside this same module) without simplifying.
#[allow(clippy::too_many_arguments)]
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
