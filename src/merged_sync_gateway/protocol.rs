//! TaskChampion protocol-facing gateway facade.
//!
//! `tc_sync::handlers` calls this module instead of opening sync storage
//! directly. Storage details remain owned by the gateway/storage layer.

use anyhow::{Context, Result};
use axum::http::StatusCode;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::merged_sync_gateway::storage::open_merged_sync_storage;

const SNAPSHOT_URGENCY_THRESHOLD: u64 = 100;
const SNAPSHOT_URGENCY_HIGH_THRESHOLD: u64 = 500;

pub fn snapshot_urgency_header(versions_since: u64) -> Option<&'static str> {
    match versions_since {
        n if n >= SNAPSHOT_URGENCY_HIGH_THRESHOLD => Some("urgency=high"),
        n if n >= SNAPSHOT_URGENCY_THRESHOLD => Some("urgency=low"),
        _ => None,
    }
}
use crate::tc_sync::storage::MIN_RETAINED_VERSIONS_AFTER_GC;

fn map_open_status(status: StatusCode) -> anyhow::Error {
    if status == StatusCode::SERVICE_UNAVAILABLE {
        anyhow::anyhow!("open merged sync storage: file is not a database")
    } else {
        anyhow::anyhow!("open merged sync storage: HTTP {status}")
    }
}

/// Child-version lookup outcome for the TC protocol adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayChildVersionOutcome {
    Version {
        version_id: Uuid,
        parent_version_id: Uuid,
        history_segment: Vec<u8>,
    },
    NotFound,
    Gone,
}

/// Return a child version from the durable merged chain.
pub async fn get_child_version(
    state: &AppState,
    user_id: &str,
    parent_version_id: Uuid,
) -> Result<GatewayChildVersionOutcome> {
    let storage = open_merged_sync_storage(state, user_id).map_err(map_open_status)?;
    tokio::task::spawn_blocking(move || {
        let guard = storage.lock().unwrap_or_else(|e| e.into_inner());
        guard.get_child_version_with_context(parent_version_id)
    })
    .await
    .context("get_child_version task panicked")?
    .map(|(child, parent_known, has_versions)| match child {
        Some((version_id, parent_version_id, history_segment)) => {
            GatewayChildVersionOutcome::Version {
                version_id,
                parent_version_id,
                history_segment,
            }
        }
        None if parent_known || !has_versions => GatewayChildVersionOutcome::NotFound,
        None => {
            crate::metrics::record_merged_gateway_retention_outcome("stale_client", "gone");
            GatewayChildVersionOutcome::Gone
        }
    })
}

/// Store a client-uploaded snapshot in the durable merged chain.
pub async fn add_snapshot(
    state: &AppState,
    user_id: &str,
    version_id: Uuid,
    snapshot: Vec<u8>,
) -> Result<bool> {
    let storage = open_merged_sync_storage(state, user_id).map_err(map_open_status)?;
    let user_id = user_id.to_string();
    tokio::task::spawn_blocking(move || {
        let guard = storage.lock().unwrap_or_else(|e| e.into_inner());
        let accepted = guard.add_snapshot(version_id, &snapshot)?;
        if accepted {
            let deleted =
                guard.garbage_collect_older_than_snapshot(MIN_RETAINED_VERSIONS_AFTER_GC)?;
            if deleted > 0 {
                crate::metrics::record_merged_gateway_retention_outcome("gc", "pruned");
                tracing::info!(
                    user_id,
                    version_id = %version_id,
                    deleted,
                    retained_versions = MIN_RETAINED_VERSIONS_AFTER_GC,
                    "merged sync retention GC pruned old versions after snapshot"
                );
            }
        }
        Ok::<bool, anyhow::Error>(accepted)
    })
    .await
    .context("add_snapshot task panicked")?
}

/// Return the latest snapshot from the durable merged chain.
pub async fn get_snapshot(state: &AppState, user_id: &str) -> Result<Option<(Uuid, Vec<u8>)>> {
    let storage = open_merged_sync_storage(state, user_id).map_err(map_open_status)?;
    tokio::task::spawn_blocking(move || {
        let guard = storage.lock().unwrap_or_else(|e| e.into_inner());
        guard.get_snapshot()
    })
    .await
    .context("get_snapshot task panicked")?
}

/// Count versions since the latest snapshot for snapshot-urgency signalling.
pub async fn versions_since_snapshot(state: &AppState, user_id: &str) -> Result<u64> {
    let storage = open_merged_sync_storage(state, user_id).map_err(map_open_status)?;
    tokio::task::spawn_blocking(move || {
        let guard = storage.lock().unwrap_or_else(|e| e.into_inner());
        guard.versions_since_snapshot()
    })
    .await
    .context("versions_since_snapshot task panicked")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_urgency_below_threshold() {
        // Fewer than 100 versions → no urgency
        assert_eq!(snapshot_urgency_header(0), None);
        assert_eq!(snapshot_urgency_header(50), None);
        assert_eq!(snapshot_urgency_header(99), None);
    }

    #[test]
    fn test_snapshot_urgency_low() {
        // 100..499 versions → low urgency
        assert_eq!(snapshot_urgency_header(100), Some("urgency=low"));
        assert_eq!(snapshot_urgency_header(250), Some("urgency=low"));
        assert_eq!(snapshot_urgency_header(499), Some("urgency=low"));
    }

    #[test]
    fn test_snapshot_urgency_high() {
        // 500+ versions → high urgency
        assert_eq!(snapshot_urgency_header(500), Some("urgency=high"));
        assert_eq!(snapshot_urgency_header(1000), Some("urgency=high"));
        assert_eq!(snapshot_urgency_header(u64::MAX), Some("urgency=high"));
    }
}
