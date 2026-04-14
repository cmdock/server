use std::time::Duration;

use chrono::{DateTime, Utc};
use taskchampion::Status;
use tokio::time::MissedTickBehavior;

use crate::app_state::AppState;
use crate::metrics;
use crate::replica;
use crate::store::models::WebhookRecord;

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const DUE_WINDOW: chrono::TimeDelta = chrono::TimeDelta::hours(24);

pub fn start(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + POLL_INTERVAL, POLL_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(err) = poll_once(&state, Utc::now()).await {
                tracing::error!(error = %err, "Webhook scheduler poll failed");
            }
        }
    });
}

pub async fn poll_once(state: &AppState, now: DateTime<Utc>) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let result = poll_once_inner(state, now).await;
    metrics::record_webhook_scheduler_run(
        if result.is_ok() { "ok" } else { "error" },
        started.elapsed().as_secs_f64(),
    );
    result
}

async fn poll_once_inner(state: &AppState, now: DateTime<Utc>) -> anyhow::Result<()> {
    let users = state.store.list_users().await?;
    for user in users {
        let webhooks = state.store.list_webhooks(&user.id).await?;
        if !has_time_driven_webhooks(&webhooks) {
            continue;
        }

        let replica = state.replica_manager.get_replica(&user.id).await?;
        let tasks: Vec<_> = {
            let mut rep = replica.lock().await;
            rep.all_tasks().await?.into_values().collect()
        };

        for task in tasks {
            if task.get_status() != Status::Pending {
                continue;
            }
            let Some(due_at) = task.get_due() else {
                continue;
            };

            if is_due(due_at, now) {
                emit_time_event(state, &user.id, &task, "task.due", due_at).await?;
            }
            if is_overdue(due_at, now) {
                emit_time_event(state, &user.id, &task, "task.overdue", due_at).await?;
            }
        }
    }
    Ok(())
}

async fn emit_time_event(
    state: &AppState,
    user_id: &str,
    task: &taskchampion::Task,
    event: &str,
    due_at: DateTime<Utc>,
) -> anyhow::Result<()> {
    let task_item = replica::task_to_item(task);
    let inserted = state
        .store
        .record_webhook_event_history(user_id, &task_item.uuid, event, &due_at.to_rfc3339())
        .await?;
    if !inserted {
        return Ok(());
    }

    crate::webhooks::delivery::emit_task_event(state, user_id, event, task_item, None, None).await;
    Ok(())
}

fn has_time_driven_webhooks(webhooks: &[WebhookRecord]) -> bool {
    webhooks
        .iter()
        .filter(|webhook| webhook.enabled)
        .any(|webhook| {
            webhook
                .events
                .iter()
                .any(|event| matches!(event.as_str(), "*" | "task.*" | "task.due" | "task.overdue"))
        })
}

fn is_due(due_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    due_at >= now && due_at <= now + DUE_WINDOW
}

fn is_overdue(due_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    due_at < now
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use chrono::{Duration as ChronoDuration, TimeZone};

    use super::{is_due, is_overdue};

    #[test]
    fn due_window_is_inclusive_and_future_only() {
        let now = Utc.with_ymd_and_hms(2026, 4, 7, 0, 0, 0).unwrap();
        assert!(is_due(now, now));
        assert!(is_due(now + ChronoDuration::hours(24), now));
        assert!(!is_due(now - ChronoDuration::seconds(1), now));
        assert!(!is_due(now + ChronoDuration::hours(25), now));
    }

    #[test]
    fn overdue_requires_past_due() {
        let now = Utc.with_ymd_and_hms(2026, 4, 7, 0, 0, 0).unwrap();
        assert!(is_overdue(now - ChronoDuration::seconds(1), now));
        assert!(!is_overdue(now, now));
        assert!(!is_overdue(now + ChronoDuration::seconds(1), now));
    }
}
