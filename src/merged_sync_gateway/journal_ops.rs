//! Journal transition helpers for merged sync gateway inbound orchestration.

use anyhow::Result;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::merged_sync_gateway::inbound::GatewayJournalAttempt;
use crate::merged_sync_gateway::journal::{GatewayJournalState, GatewayRecoveryStatus};
use crate::store::models::{MergedSyncJournalDiagnostic, MergedSyncJournalTransition};

pub(super) struct JournalTransition<'a> {
    pub(super) from_state: GatewayJournalState,
    pub(super) to_state: GatewayJournalState,
    pub(super) merged_version_id: Option<Uuid>,
    pub(super) recovery_status: GatewayRecoveryStatus,
    pub(super) diagnostic: Option<&'a MergedSyncJournalDiagnostic>,
}

pub(super) async fn fail_received(
    state: &AppState,
    journal: &GatewayJournalAttempt,
    code: &str,
    message: &str,
) -> Result<()> {
    let diagnostic = MergedSyncJournalDiagnostic {
        code: Some(code.to_string()),
        message: Some(message.to_string()),
    };
    transition(
        state,
        journal,
        JournalTransition {
            from_state: GatewayJournalState::Received,
            to_state: GatewayJournalState::Failed,
            merged_version_id: None,
            recovery_status: GatewayRecoveryStatus::Failed,
            diagnostic: Some(&diagnostic),
        },
    )
    .await
}

pub(super) async fn mark_quarantined(
    state: &AppState,
    journal: &GatewayJournalAttempt,
    from_state: GatewayJournalState,
    code: &str,
    message: &str,
) -> Result<()> {
    let diagnostic = MergedSyncJournalDiagnostic {
        code: Some(code.to_string()),
        message: Some(message.to_string()),
    };
    transition(
        state,
        journal,
        JournalTransition {
            from_state,
            to_state: GatewayJournalState::Quarantined,
            merged_version_id: None,
            recovery_status: GatewayRecoveryStatus::Quarantined,
            diagnostic: Some(&diagnostic),
        },
    )
    .await?;
    if let Some(row) = state
        .store
        .get_merged_sync_journal(&journal.journal_id)
        .await?
    {
        tracing::info!(
            target: "audit",
            action = "merged_sync.journal_quarantined",
            user_id = %row.user_id,
            client_id = %row.client_id,
            gateway_attempt_id = %row.attempt_id,
            journal_id = %row.journal_id,
            parent_version_id = %row.parent_version_id,
            merged_version_id = %row.merged_version_id.as_deref().unwrap_or(""),
            from_state = from_state.as_str(),
            code,
            message,
            outcome = "quarantined",
        );
    }
    Ok(())
}

pub(super) async fn transition(
    state: &AppState,
    journal: &GatewayJournalAttempt,
    step: JournalTransition<'_>,
) -> Result<()> {
    let from_state = step.from_state;
    let to_state = step.to_state;
    if !try_transition(state, journal, step).await? {
        anyhow::bail!(
            "stale merged sync journal transition {} -> {} for journal {}",
            from_state.as_str(),
            to_state.as_str(),
            journal.journal_id
        );
    }
    Ok(())
}

pub(super) async fn try_transition(
    state: &AppState,
    journal: &GatewayJournalAttempt,
    step: JournalTransition<'_>,
) -> Result<bool> {
    debug_assert!(
        step.from_state.can_transition_to(step.to_state),
        "illegal merged sync journal transition {} -> {}",
        step.from_state.as_str(),
        step.to_state.as_str()
    );
    let merged_version_id_string = step.merged_version_id.map(|id| id.to_string());
    let row = state
        .store
        .transition_merged_sync_journal(MergedSyncJournalTransition {
            journal_id: &journal.journal_id,
            attempt_id: &journal.attempt_id,
            from_state: step.from_state,
            to_state: step.to_state,
            merged_version_id: merged_version_id_string.as_deref(),
            recovery_status: step.recovery_status,
            diagnostic: step.diagnostic,
        })
        .await?;
    if row.is_some() {
        crate::metrics::record_merged_gateway_journal_transition(
            step.from_state.as_str(),
            step.to_state.as_str(),
        );
        if step.to_state.is_terminal() {
            crate::metrics::record_merged_gateway_recovery_outcome(step.recovery_status.as_str());
        }
        Ok(true)
    } else {
        Ok(false)
    }
}
