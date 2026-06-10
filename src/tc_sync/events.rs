use crate::app_state::AppState;
use crate::webhooks::summary;

pub type SyncSnapshot = summary::SyncTaskSnapshot;

pub async fn capture_before_sync(state: &AppState, user_id: &str) -> Option<SyncSnapshot> {
    match summary::capture_sync_snapshot(state, user_id).await {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            tracing::warn!(
                user_id = %user_id,
                error = %err,
                "Failed to capture pre-sync task snapshot for sync.completed webhook"
            );
            None
        }
    }
}

pub async fn emit_sync_completed_if_changed(
    state: &AppState,
    user_id: &str,
    request_id: Option<String>,
    before_sync: Option<SyncSnapshot>,
) {
    summary::emit_sync_completed_if_changed(state, user_id, request_id, before_sync).await;
}
