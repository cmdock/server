use std::collections::{BTreeSet, HashMap};

use crate::app_state::AppState;
use crate::replica;
use crate::store::models::WebhookSyncSummary;
use crate::tasks::models::TaskItem;

#[derive(Debug, Clone)]
pub struct SyncTaskSnapshot(HashMap<String, TaskItem>);

pub async fn capture_sync_snapshot(
    state: &AppState,
    user_id: &str,
) -> anyhow::Result<SyncTaskSnapshot> {
    let replica = state.replica_manager.get_replica(user_id).await?;
    let mut rep = replica.lock().await;
    let tasks = rep
        .all_tasks()
        .await?
        .into_values()
        .map(|task| replica::task_to_item(&task))
        .map(|task| (task.uuid.clone(), task))
        .collect();
    Ok(SyncTaskSnapshot(tasks))
}

fn summarize_sync_change(
    before: SyncTaskSnapshot,
    after: SyncTaskSnapshot,
) -> Option<WebhookSyncSummary> {
    let mut created = 0usize;
    let mut completed = 0usize;
    let mut deleted = 0usize;
    let mut modified = 0usize;

    for uuid in before
        .0
        .keys()
        .chain(after.0.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
    {
        match (before.0.get(&uuid), after.0.get(&uuid)) {
            (None, Some(_)) => created += 1,
            (Some(_), None) => deleted += 1,
            (Some(before_task), Some(after_task)) if before_task == after_task => {}
            (Some(before_task), Some(after_task)) => {
                if before_task.status != "completed" && after_task.status == "completed" {
                    completed += 1;
                } else if before_task.status != "deleted" && after_task.status == "deleted" {
                    deleted += 1;
                } else {
                    modified += 1;
                }
            }
            (None, None) => {}
        }
    }

    let tasks_changed = created + completed + deleted + modified;
    (tasks_changed > 0).then_some(WebhookSyncSummary {
        tasks_changed,
        created,
        completed,
        deleted,
        modified,
    })
}

pub async fn emit_sync_completed_if_changed(
    state: &AppState,
    user_id: &str,
    request_id: Option<String>,
    before: Option<SyncTaskSnapshot>,
) {
    let Some(before) = before else {
        return;
    };

    let after = match capture_sync_snapshot(state, user_id).await {
        Ok(after) => after,
        Err(err) => {
            tracing::warn!(
                user_id = %user_id,
                error = %err,
                "Failed to capture post-sync task snapshot for sync.completed webhook"
            );
            return;
        }
    };

    if let Some(summary) = summarize_sync_change(before, after) {
        crate::webhooks::delivery::emit_sync_event(state, user_id, summary, request_id).await;
    }
}
