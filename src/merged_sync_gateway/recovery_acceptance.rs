//! Recovery helpers for the inbound merged-chain acceptance boundary.

use anyhow::Result;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::merged_sync_gateway::inbound::{self, GatewayVersion};

pub(super) enum AcceptanceRecovery {
    Accepted(Uuid),
    UnacceptedConflict { expected: Uuid },
}

pub(super) fn verify_accepted_merged_version(
    state: &AppState,
    user_id: &str,
    parent_version_id: Uuid,
    merged_version_id: Option<Uuid>,
    expected_segment: &[u8],
) -> Result<bool> {
    let Some(merged_version_id) = merged_version_id else {
        return Ok(false);
    };
    Ok(
        matching_child_version(state, user_id, parent_version_id, expected_segment)?
            == Some(merged_version_id),
    )
}

fn matching_child_version(
    state: &AppState,
    user_id: &str,
    parent_version_id: Uuid,
    expected_segment: &[u8],
) -> Result<Option<Uuid>> {
    let storage = crate::merged_sync_gateway::storage::open_merged_sync_storage(state, user_id)
        .map_err(|status| {
            anyhow::anyhow!("open merged sync storage during recovery: HTTP {status}")
        })?;
    let guard = storage.lock().unwrap_or_else(|e| e.into_inner());
    let (child, _parent_known, _has_versions) =
        guard.get_child_version_with_context(parent_version_id)?;
    Ok(match child {
        Some((child_id, _parent_id, segment)) if segment == expected_segment => Some(child_id),
        _ => None,
    })
}

pub(super) async fn recover_acceptance_boundary(
    state: &AppState,
    version: &mut GatewayVersion,
    expected_segment: &[u8],
) -> Result<AcceptanceRecovery> {
    let storage =
        crate::merged_sync_gateway::storage::open_merged_sync_storage(state, &version.user_id)
            .map_err(|status| {
                anyhow::anyhow!("open merged sync storage during recovery: HTTP {status}")
            })?;
    {
        let guard = storage.lock().unwrap_or_else(|e| e.into_inner());
        let (child, _parent_known, _has_versions) =
            guard.get_child_version_with_context(version.parent_version_id)?;
        if let Some((child_id, _parent_id, segment)) = child {
            if segment == expected_segment {
                return Ok(AcceptanceRecovery::Accepted(child_id));
            }
            return Ok(AcceptanceRecovery::UnacceptedConflict { expected: child_id });
        }
        let latest = guard.get_latest_version_id()?;
        if latest != version.parent_version_id {
            return Ok(AcceptanceRecovery::UnacceptedConflict { expected: latest });
        }
    }

    match inbound::append_inbound_merged_version(state, version).await? {
        Ok(version_id) => Ok(AcceptanceRecovery::Accepted(version_id)),
        Err(expected) => {
            if let Some(version_id) = matching_child_version(
                state,
                &version.user_id,
                version.parent_version_id,
                expected_segment,
            )? {
                Ok(AcceptanceRecovery::Accepted(version_id))
            } else {
                Ok(AcceptanceRecovery::UnacceptedConflict { expected })
            }
        }
    }
}
