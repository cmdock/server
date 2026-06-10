//! Audit helpers for merged sync gateway inbound/recovery flows.

use uuid::Uuid;

use anyhow::Result;

use crate::app_state::AppState;
use crate::merged_sync_gateway::inbound::{GatewayJournalAttempt, GatewayVersion};
use crate::merged_sync_gateway::planner::{
    CorrectiveProjectionPlan, SourceApplyPlan, VisibleTaskScope, CMDOCK_ACCOUNT_UDA,
    CMDOCK_TASK_SCOPE_UDA,
};
use crate::store::models::MergedSyncJournalRecord;

pub(super) fn audit_version_lifecycle(
    version: &GatewayVersion,
    journal: &GatewayJournalAttempt,
    action: &'static str,
    merged_version_id: Option<Uuid>,
    outcome: &'static str,
) {
    tracing::info!(
        target: "audit",
        action,
        user_id = %version.user_id,
        client_id = %version.client_id,
        gateway_attempt_id = %journal.attempt_id,
        journal_id = %journal.journal_id,
        parent_version_id = %version.parent_version_id,
        merged_version_id = %merged_version_id.map(|id| id.to_string()).unwrap_or_default(),
        request_id = %version.request_id.as_deref().unwrap_or(""),
        outcome,
    );
}

pub(super) fn audit_source_apply_success(
    version: &GatewayVersion,
    journal: &GatewayJournalAttempt,
    merged_version_id: Uuid,
    scope: &VisibleTaskScope,
    plan: &SourceApplyPlan,
) {
    for group in &plan.groups {
        for op in &group.operations {
            tracing::info!(
                target: "audit",
                action = "merged_sync.source_apply_succeeded",
                user_id = %version.user_id,
                client_id = %version.client_id,
                gateway_attempt_id = %journal.attempt_id,
                journal_id = %journal.journal_id,
                parent_version_id = %version.parent_version_id,
                merged_version_id = %merged_version_id,
                task_scope_id = %scope.task_scope_id,
                task_scope_prefix = %scope.key_prefix,
                task_uuid = %group.task_uuid,
                operation_index = op.operation_index,
                request_id = %version.request_id.as_deref().unwrap_or(""),
                outcome = "success",
            );
        }
    }
}

pub(super) fn audit_source_apply_failure(
    version: &GatewayVersion,
    journal: &GatewayJournalAttempt,
    merged_version_id: Uuid,
    scope: &VisibleTaskScope,
    message: &str,
) {
    tracing::info!(
        target: "audit",
        action = "merged_sync.source_apply_failed",
        user_id = %version.user_id,
        client_id = %version.client_id,
        gateway_attempt_id = %journal.attempt_id,
        journal_id = %journal.journal_id,
        parent_version_id = %version.parent_version_id,
        merged_version_id = %merged_version_id,
        task_scope_id = %scope.task_scope_id,
        task_scope_prefix = %scope.key_prefix,
        request_id = %version.request_id.as_deref().unwrap_or(""),
        outcome = "failure",
        message = %message,
    );
}

pub(super) fn audit_corrective_projection(
    version: &GatewayVersion,
    journal: &GatewayJournalAttempt,
    merged_version_id: Uuid,
    scope: &VisibleTaskScope,
    plan: &CorrectiveProjectionPlan,
) {
    for correction in &plan.corrections {
        let action = if correction.property == CMDOCK_TASK_SCOPE_UDA {
            "merged_sync.cmdock_task_scope_corrected"
        } else if correction.property == CMDOCK_ACCOUNT_UDA {
            "merged_sync.cmdock_account_corrected"
        } else {
            "merged_sync.cmdock_key_corrected"
        };
        tracing::info!(
            target: "audit",
            action,
            user_id = %version.user_id,
            client_id = %version.client_id,
            gateway_attempt_id = %journal.attempt_id,
            journal_id = %journal.journal_id,
            parent_version_id = %version.parent_version_id,
            merged_version_id = %merged_version_id,
            task_scope_id = %scope.task_scope_id,
            task_scope_prefix = %scope.key_prefix,
            task_uuid = %correction.task_uuid,
            operation_index = correction.operation_index,
            property = correction.property,
            request_id = %version.request_id.as_deref().unwrap_or(""),
            outcome = correction.outcome,
        );
    }
}

pub(super) fn audit_projection_failure(
    version: &GatewayVersion,
    journal: &GatewayJournalAttempt,
    merged_version_id: Uuid,
    scope: &VisibleTaskScope,
    message: &str,
) {
    tracing::info!(
        target: "audit",
        action = "merged_sync.corrective_projection_failed",
        user_id = %version.user_id,
        client_id = %version.client_id,
        gateway_attempt_id = %journal.attempt_id,
        journal_id = %journal.journal_id,
        parent_version_id = %version.parent_version_id,
        merged_version_id = %merged_version_id,
        task_scope_id = %scope.task_scope_id,
        task_scope_prefix = %scope.key_prefix,
        request_id = %version.request_id.as_deref().unwrap_or(""),
        outcome = "failure",
        message = %message,
    );
}

pub(super) fn audit_recovery_started(row: &MergedSyncJournalRecord) {
    tracing::info!(
        target: "audit",
        action = "merged_sync.recovery_started",
        user_id = %row.user_id,
        client_id = %row.client_id,
        gateway_attempt_id = %row.attempt_id,
        journal_id = %row.journal_id,
        parent_version_id = %row.parent_version_id,
        merged_version_id = %row.merged_version_id.as_deref().unwrap_or(""),
        from_state = row.state.as_str(),
        outcome = "started",
    );
}

pub(super) async fn audit_recovery_version_rejected(
    state: &AppState,
    journal: &GatewayJournalAttempt,
    code: &str,
) -> Result<()> {
    let Some(row) = state
        .store
        .get_merged_sync_journal(&journal.journal_id)
        .await?
    else {
        return Ok(());
    };
    tracing::info!(
        target: "audit",
        action = "merged_sync.version_rejected",
        user_id = %row.user_id,
        client_id = %row.client_id,
        gateway_attempt_id = %row.attempt_id,
        journal_id = %row.journal_id,
        parent_version_id = %row.parent_version_id,
        merged_version_id = %row.merged_version_id.as_deref().unwrap_or(""),
        request_id = "",
        outcome = code,
    );
    Ok(())
}

pub(super) fn audit_recovery_finished(
    row: &MergedSyncJournalRecord,
    outcome: &'static str,
    code: Option<&str>,
) {
    tracing::info!(
        target: "audit",
        action = "merged_sync.recovery_finished",
        user_id = %row.user_id,
        client_id = %row.client_id,
        gateway_attempt_id = %row.attempt_id,
        journal_id = %row.journal_id,
        parent_version_id = %row.parent_version_id,
        merged_version_id = %row.merged_version_id.as_deref().unwrap_or(""),
        from_state = row.state.as_str(),
        code = code.unwrap_or(""),
        outcome,
    );
}
