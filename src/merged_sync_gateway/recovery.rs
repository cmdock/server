//! Forward-only restart recovery for merged-sync gateway journal rows.
//!
//! The gateway never rewrites accepted client history. Recovery either finishes
//! the already-recorded source/projection/finalize work using the typed inbound
//! segment stored in the journal, or moves the row to an operator-visible
//! terminal diagnostic state when doing so would require guessing.

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::merged_sync_gateway::audit::{
    audit_recovery_finished, audit_recovery_started, audit_recovery_version_rejected,
};
use crate::merged_sync_gateway::codec::decode_history_segment;
use crate::merged_sync_gateway::inbound::{self, GatewayJournalAttempt, GatewayVersion};
use crate::merged_sync_gateway::journal::{GatewayJournalState, GatewayRecoveryStatus};
use crate::merged_sync_gateway::journal_ops::{try_transition, JournalTransition};
use crate::merged_sync_gateway::planner;
use crate::merged_sync_gateway::recovery_acceptance::{
    recover_acceptance_boundary, verify_accepted_merged_version, AcceptanceRecovery,
};
use crate::store::models::{MergedSyncJournalDiagnostic, MergedSyncJournalRecord};

const RECOVERY_SCAN_LIMIT_PER_USER: usize = 10_000;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GatewayRecoverySummary {
    pub inspected: usize,
    pub recovered: usize,
    pub failed: usize,
    pub quarantined: usize,
    pub skipped_terminal: usize,
    pub stale: usize,
}

struct JournalTerminal<'a> {
    from_state: GatewayJournalState,
    to_state: GatewayJournalState,
    recovery_status: GatewayRecoveryStatus,
    code: &'a str,
    message: &'a str,
}

/// Recover all non-terminal merged-sync journal rows for every user.
///
/// Intended for startup and operator-triggered recovery. The scan limit is high
/// enough for the beta/runtime journal model; if an operator ever accumulates
/// more than this many non-terminal rows for one user, this pass processes the
/// oldest rows first and leaves newer rows for later passes/operator diagnostics.
pub async fn recover_all_users(state: &AppState) -> Result<GatewayRecoverySummary> {
    let mut summary = GatewayRecoverySummary::default();
    for user in state.store.list_users().await? {
        let user_summary = recover_user(state, &user.id).await?;
        summary.inspected += user_summary.inspected;
        summary.recovered += user_summary.recovered;
        summary.failed += user_summary.failed;
        summary.quarantined += user_summary.quarantined;
        summary.skipped_terminal += user_summary.skipped_terminal;
        summary.stale += user_summary.stale;
    }
    Ok(summary)
}

/// Recover non-terminal merged-sync journal rows for one user.
pub async fn recover_user(state: &AppState, user_id: &str) -> Result<GatewayRecoverySummary> {
    let rows = state
        .store
        .list_nonterminal_merged_sync_journal_for_user(user_id, RECOVERY_SCAN_LIMIT_PER_USER)
        .await?;
    let mut summary = GatewayRecoverySummary::default();
    // Oldest first so parent/version-chain dependencies are recovered in order.
    for row in rows {
        summary.inspected += 1;
        match recover_row(state, row).await? {
            RowRecoveryOutcome::Recovered => summary.recovered += 1,
            RowRecoveryOutcome::Failed => summary.failed += 1,
            RowRecoveryOutcome::Quarantined => summary.quarantined += 1,
            RowRecoveryOutcome::SkippedTerminal => summary.skipped_terminal += 1,
            RowRecoveryOutcome::Stale => summary.stale += 1,
        }
    }
    Ok(summary)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowRecoveryOutcome {
    Recovered,
    Failed,
    Quarantined,
    SkippedTerminal,
    Stale,
}

async fn recover_row(state: &AppState, row: MergedSyncJournalRecord) -> Result<RowRecoveryOutcome> {
    if row.state.is_terminal() {
        return Ok(RowRecoveryOutcome::SkippedTerminal);
    }

    audit_recovery_started(&row);

    let outcome = recover_row_inner(state, &row).await;
    match &outcome {
        Ok(RowRecoveryOutcome::Recovered) => {
            let latest = latest_journal_row(state, &row)
                .await?
                .unwrap_or_else(|| row.clone());
            audit_recovery_finished(&latest, "recovered", None)
        }
        Ok(RowRecoveryOutcome::Failed) => {
            let latest = latest_journal_row(state, &row)
                .await?
                .unwrap_or_else(|| row.clone());
            audit_recovery_finished(&latest, "failed", latest.diagnostic_code.as_deref())
        }
        Ok(RowRecoveryOutcome::Quarantined) => {
            let latest = latest_journal_row(state, &row)
                .await?
                .unwrap_or_else(|| row.clone());
            audit_recovery_finished(&latest, "quarantined", latest.diagnostic_code.as_deref())
        }
        Ok(RowRecoveryOutcome::Stale) => audit_recovery_finished(&row, "stale", None),
        Ok(RowRecoveryOutcome::SkippedTerminal) => {}
        Err(err) => audit_recovery_finished(&row, "error", Some(&err.to_string())),
    }
    outcome
}

async fn recover_row_inner(
    state: &AppState,
    row: &MergedSyncJournalRecord,
) -> Result<RowRecoveryOutcome> {
    let journal = GatewayJournalAttempt {
        journal_id: row.journal_id.clone(),
        attempt_id: row.attempt_id.clone(),
    };
    let parent_version_id = Uuid::parse_str(&row.parent_version_id)
        .with_context(|| format!("journal {} has invalid parent_version_id", row.journal_id))?;
    let mut version = GatewayVersion {
        user_id: row.user_id.clone(),
        client_id: row.client_id.clone(),
        parent_version_id,
        history_segment: row.inbound_history_segment.clone(),
        request_id: None,
    };

    let decoded = match decode_history_segment(&row.inbound_history_segment) {
        Ok(decoded) => decoded,
        Err(err) => {
            return terminal(
                state,
                &journal,
                JournalTerminal {
                    from_state: row.state,
                    to_state: GatewayJournalState::Failed,
                    recovery_status: GatewayRecoveryStatus::Failed,
                    code: "codec_failed",
                    message: &err.to_string(),
                },
            )
            .await;
        }
    };
    let replay = match planner::prepare_personal_replay_plan(state, &row.user_id, &decoded).await {
        Ok(replay) => replay,
        Err(reject) => {
            return terminal(
                state,
                &journal,
                JournalTerminal {
                    from_state: row.state,
                    to_state: GatewayJournalState::Failed,
                    recovery_status: GatewayRecoveryStatus::Failed,
                    code: reject.code,
                    message: &reject.message,
                },
            )
            .await;
        }
    };

    let mut state_cursor = row.state;
    let mut merged_version_id = row
        .merged_version_id
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .with_context(|| format!("journal {} has invalid merged_version_id", row.journal_id))?;

    if state_cursor == GatewayJournalState::Received {
        if let Err(err) = inbound::validate_source_plan_for_current_source(
            state,
            &row.user_id,
            &replay.source_plan,
        )
        .await
        {
            return terminal(
                state,
                &journal,
                JournalTerminal {
                    from_state: GatewayJournalState::Received,
                    to_state: GatewayJournalState::Failed,
                    recovery_status: GatewayRecoveryStatus::Failed,
                    code: "invalid_source_operation",
                    message: &err.to_string(),
                },
            )
            .await;
        }
        match recover_acceptance_boundary(state, &mut version, &row.inbound_history_segment).await?
        {
            AcceptanceRecovery::Accepted(version_id) => {
                merged_version_id = Some(version_id);
                if !try_transition(
                    state,
                    &journal,
                    JournalTransition {
                        from_state: GatewayJournalState::Received,
                        to_state: GatewayJournalState::MergedVersionAccepted,
                        merged_version_id: Some(version_id),
                        recovery_status: GatewayRecoveryStatus::Recoverable,
                        diagnostic: None,
                    },
                )
                .await?
                {
                    return Ok(RowRecoveryOutcome::Stale);
                }
                inbound::audit_version_lifecycle(
                    &version,
                    &journal,
                    "merged_sync.version_accepted",
                    Some(version_id),
                    "accepted",
                );
                state_cursor = GatewayJournalState::MergedVersionAccepted;
            }
            AcceptanceRecovery::UnacceptedConflict { expected } => {
                return terminal(
                    state,
                    &journal,
                    JournalTerminal {
                        from_state: GatewayJournalState::Received,
                        to_state: GatewayJournalState::Failed,
                        recovery_status: GatewayRecoveryStatus::Failed,
                        code: "expected_parent_version",
                        message: &format!("expected parent version {expected}"),
                    },
                )
                .await;
            }
        }
    } else if !verify_accepted_merged_version(
        state,
        &row.user_id,
        parent_version_id,
        merged_version_id,
        &row.inbound_history_segment,
    )? {
        return terminal(
            state,
            &journal,
            JournalTerminal {
                from_state: state_cursor,
                to_state: GatewayJournalState::Quarantined,
                recovery_status: GatewayRecoveryStatus::Quarantined,
                code: "accepted_version_missing_or_mismatch",
                message: "accepted-or-later journal row does not match retained merged chain",
            },
        )
        .await;
    }

    if state_cursor == GatewayJournalState::MergedVersionAccepted {
        // Recovery runs at startup/operator time outside the live inbound HTTP
        // path, so it intentionally does not acquire `INBOUND_ADD_LOCKS`.
        // `apply_source_plan` still serializes canonical writes via the
        // per-user task mutation lock plus the replica lock.
        if let Err(err) =
            inbound::apply_source_plan(state, &row.user_id, &replay.scope, &replay.source_plan)
                .await
        {
            return terminal(
                state,
                &journal,
                JournalTerminal {
                    from_state: GatewayJournalState::MergedVersionAccepted,
                    to_state: GatewayJournalState::Quarantined,
                    recovery_status: GatewayRecoveryStatus::Quarantined,
                    code: "source_apply_failed",
                    message: &format!(
                        "recover source apply for journal {}: {err:#}",
                        row.journal_id
                    ),
                },
            )
            .await;
        }
        // `merged_version_id` is expected to be Some here: accepted-or-later
        // rows with a missing/mismatched merged version are quarantined by the
        // verification guard above. Keep the Option shape because the journal
        // DB column itself is nullable and direct DB tampering should not panic.
        if let Some(version_id) = merged_version_id {
            inbound::audit_source_apply_success(
                &version,
                &journal,
                version_id,
                &replay.scope,
                &replay.source_plan,
            );
        }
        if !try_transition(
            state,
            &journal,
            JournalTransition {
                from_state: GatewayJournalState::MergedVersionAccepted,
                to_state: GatewayJournalState::SourcePlanApplied,
                merged_version_id,
                recovery_status: GatewayRecoveryStatus::Recoverable,
                diagnostic: None,
            },
        )
        .await?
        {
            return Ok(RowRecoveryOutcome::Stale);
        }
        state_cursor = GatewayJournalState::SourcePlanApplied;
    }

    if state_cursor == GatewayJournalState::SourcePlanApplied {
        if replay.corrective_plan.requires_projection() {
            if let Err(err) =
                crate::merged_sync_gateway::projection::project_personal_now(state, &row.user_id)
                    .await
            {
                return terminal(
                    state,
                    &journal,
                    JournalTerminal {
                        from_state: GatewayJournalState::SourcePlanApplied,
                        to_state: GatewayJournalState::Quarantined,
                        recovery_status: GatewayRecoveryStatus::Quarantined,
                        code: "projection_append_failed",
                        message: &format!(
                            "recover projection append for journal {}: {err:#}",
                            row.journal_id
                        ),
                    },
                )
                .await;
            }
            if let Some(version_id) = merged_version_id {
                inbound::audit_corrective_projection(
                    &version,
                    &journal,
                    version_id,
                    &replay.scope,
                    &replay.corrective_plan,
                );
            }
        }
        if !try_transition(
            state,
            &journal,
            JournalTransition {
                from_state: GatewayJournalState::SourcePlanApplied,
                to_state: GatewayJournalState::ProjectionAppended,
                merged_version_id,
                recovery_status: GatewayRecoveryStatus::Recoverable,
                diagnostic: None,
            },
        )
        .await?
        {
            return Ok(RowRecoveryOutcome::Stale);
        }
        state_cursor = GatewayJournalState::ProjectionAppended;
    }

    if state_cursor == GatewayJournalState::ProjectionAppended {
        if !try_transition(
            state,
            &journal,
            JournalTransition {
                from_state: GatewayJournalState::ProjectionAppended,
                to_state: GatewayJournalState::Finalized,
                merged_version_id,
                recovery_status: GatewayRecoveryStatus::Recovered,
                diagnostic: None,
            },
        )
        .await?
        {
            return Ok(RowRecoveryOutcome::Stale);
        }
        return Ok(RowRecoveryOutcome::Recovered);
    }

    Ok(RowRecoveryOutcome::Stale)
}

async fn terminal(
    state: &AppState,
    journal: &GatewayJournalAttempt,
    terminal: JournalTerminal<'_>,
) -> Result<RowRecoveryOutcome> {
    let diagnostic = MergedSyncJournalDiagnostic {
        code: Some(terminal.code.to_string()),
        message: Some(terminal.message.to_string()),
    };
    let transitioned = try_transition(
        state,
        journal,
        JournalTransition {
            from_state: terminal.from_state,
            to_state: terminal.to_state,
            merged_version_id: None,
            recovery_status: terminal.recovery_status,
            diagnostic: Some(&diagnostic),
        },
    )
    .await?;
    if !transitioned {
        return Ok(RowRecoveryOutcome::Stale);
    }
    if emits_recovery_version_rejected(terminal.code) {
        audit_recovery_version_rejected(state, journal, terminal.code).await?;
    }
    Ok(match terminal.to_state {
        GatewayJournalState::Failed => RowRecoveryOutcome::Failed,
        GatewayJournalState::Quarantined => RowRecoveryOutcome::Quarantined,
        _ => RowRecoveryOutcome::Stale,
    })
}

async fn latest_journal_row(
    state: &AppState,
    row: &MergedSyncJournalRecord,
) -> Result<Option<MergedSyncJournalRecord>> {
    state.store.get_merged_sync_journal(&row.journal_id).await
}

fn emits_recovery_version_rejected(code: &str) -> bool {
    matches!(
        code,
        "codec_failed"
            | "invalid_source_operation"
            | "expected_parent_version"
            | "accepted_version_missing_or_mismatch"
            | "TASK_SCOPE_FORBIDDEN"
    )
}
