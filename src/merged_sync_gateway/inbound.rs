//! Inbound personal-only apply path for the merged sync gateway.
//!
//! Phase 5 accepts one plaintext TaskChampion history segment from a TW client,
//! journals it, appends the accepted version to the durable merged chain, applies
//! allowed personal-scope writes to canonical source truth, then projects any
//! server-owned corrections back into the merged chain before finalizing.

use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::app_state::AppState;
pub(super) use crate::merged_sync_gateway::audit::{
    audit_corrective_projection, audit_source_apply_success, audit_version_lifecycle,
};
use crate::merged_sync_gateway::audit::{audit_projection_failure, audit_source_apply_failure};
use crate::merged_sync_gateway::codec::decode_history_segment;
use crate::merged_sync_gateway::journal::{GatewayJournalState, GatewayRecoveryStatus};
use crate::merged_sync_gateway::journal_ops::{
    fail_received, mark_quarantined, transition, JournalTransition,
};
pub use crate::merged_sync_gateway::planner::{
    plan_personal_source_apply, CorrectiveProjectionPlan, CorrectiveProjectionReason,
    IndexedWireOp, SourceApplyPlan, VisibleTaskScope, VisibleTaskScopeSet, WireTaskGroup,
};
use crate::merged_sync_gateway::planner::{
    plan_personal_source_apply_inner, resolve_visible_task_scopes,
};
use crate::merged_sync_gateway::projection::project_personal_now;
pub(super) use crate::merged_sync_gateway::source::{
    apply_source_plan, validate_source_plan_for_current_source,
};
use crate::merged_sync_gateway::sqlite_error::is_sqlite_constraint_violation;
use crate::store::models::NewMergedSyncJournalAttempt;

static INBOUND_ADD_LOCKS: OnceLock<dashmap::DashMap<String, Arc<tokio::sync::Mutex<()>>>> =
    OnceLock::new();

fn inbound_add_lock(user_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    INBOUND_ADD_LOCKS
        .get_or_init(dashmap::DashMap::new)
        .entry(user_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

pub fn evict_inbound_add_lock(user_id: &str) {
    if let Some(locks) = INBOUND_ADD_LOCKS.get() {
        locks.remove(user_id);
    }
}

/// Plaintext inbound version submitted to the gateway by the TC protocol edge.
#[derive(Debug, Clone)]
pub struct GatewayVersion {
    pub user_id: String,
    pub client_id: String,
    pub parent_version_id: Uuid,
    pub history_segment: Vec<u8>,
    /// Optional HTTP request correlation propagated from the TC protocol edge.
    pub request_id: Option<String>,
}

/// Durable attempt identity for one inbound gateway version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayJournalAttempt {
    pub journal_id: String,
    pub attempt_id: String,
}

/// Outcome of an inbound add-version call at the gateway boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayAddVersionOutcome {
    Accepted {
        journal_id: String,
        version_id: Uuid,
    },
    ExpectedParentVersion {
        journal_id: String,
        expected_parent_version_id: Uuid,
    },
    Rejected {
        journal_id: String,
        code: &'static str,
    },
}

/// Accept and apply a personal-only plaintext merged-chain version.
///
/// Phase 6 will call this from `tc_sync::handlers` after auth/encryption
/// translation. Until then integration tests exercise it directly.
pub async fn add_personal_version(
    state: &AppState,
    version: GatewayVersion,
) -> Result<GatewayAddVersionOutcome> {
    let add_lock = inbound_add_lock(&version.user_id);
    let _add_guard = add_lock.lock().await;

    let journal = GatewayJournalAttempt {
        journal_id: Uuid::new_v4().to_string(),
        attempt_id: Uuid::new_v4().to_string(),
    };
    state
        .store
        .create_merged_sync_journal_attempt(&NewMergedSyncJournalAttempt {
            journal_id: journal.journal_id.clone(),
            user_id: version.user_id.clone(),
            client_id: version.client_id.clone(),
            attempt_id: journal.attempt_id.clone(),
            parent_version_id: version.parent_version_id.to_string(),
            inbound_history_segment: version.history_segment.clone(),
        })
        .await
        .context("create merged sync journal attempt")?;
    audit_version_lifecycle(
        &version,
        &journal,
        "merged_sync.version_received",
        None,
        "received",
    );

    let decoded = match decode_history_segment(&version.history_segment) {
        Ok(decoded) => decoded,
        Err(err) => {
            crate::metrics::record_merged_gateway_codec_failure("decode");
            fail_received(state, &journal, "codec_failed", &err.to_string()).await?;
            audit_version_lifecycle(
                &version,
                &journal,
                "merged_sync.version_rejected",
                None,
                "codec_failed",
            );
            return Ok(GatewayAddVersionOutcome::Rejected {
                journal_id: journal.journal_id,
                code: "codec_failed",
            });
        }
    };

    let visible_scopes = resolve_visible_task_scopes(state, &version.user_id)
        .await
        .map_err(|reject| anyhow::anyhow!("{}: {}", reject.code, reject.message))?;
    let scope = visible_scopes
        .sole_scope()
        .ok_or_else(|| anyhow::anyhow!("expected exactly one visible Task Scope"))?
        .clone();

    let (source_plan, corrective_plan) = match plan_personal_source_apply_inner(&decoded, &scope) {
        Ok(plan) => plan,
        Err(reject) => {
            fail_received(state, &journal, reject.code, &reject.message).await?;
            tracing::info!(
                target: "audit",
                action = "merged_sync.task_scope_forbidden",
                user_id = %version.user_id,
                client_id = %version.client_id,
                gateway_attempt_id = %journal.attempt_id,
                parent_version_id = %version.parent_version_id,
                task_scope_id = %scope.task_scope_id,
                task_scope_prefix = %scope.key_prefix,
                request_id = %version.request_id.as_deref().unwrap_or(""),
                code = reject.code,
                message = %reject.message,
                outcome = "rejected",
            );
            audit_version_lifecycle(
                &version,
                &journal,
                "merged_sync.version_rejected",
                None,
                reject.code,
            );
            return Ok(GatewayAddVersionOutcome::Rejected {
                journal_id: journal.journal_id,
                code: reject.code,
            });
        }
    };

    // INVARIANT: this is a pre-accept semantic check, not a lock held until
    // source apply. A concurrent REST/source write can still race after this
    // point; accepted post-append failures are handled by forward-only
    // quarantine/recovery rather than by rewriting merged history.
    if let Err(err) =
        validate_source_plan_for_current_source(state, &version.user_id, &source_plan).await
    {
        fail_received(
            state,
            &journal,
            "invalid_source_operation",
            &err.to_string(),
        )
        .await?;
        audit_version_lifecycle(
            &version,
            &journal,
            "merged_sync.version_rejected",
            None,
            "invalid_source_operation",
        );
        return Ok(GatewayAddVersionOutcome::Rejected {
            journal_id: journal.journal_id,
            code: "invalid_source_operation",
        });
    }

    let append_result = append_inbound_merged_version(state, &version).await?;
    let merged_version_id = match append_result {
        Ok(version_id) => version_id,
        Err(expected) => {
            fail_received(
                state,
                &journal,
                "expected_parent_version",
                &format!("expected parent version {expected}"),
            )
            .await?;
            audit_version_lifecycle(
                &version,
                &journal,
                "merged_sync.version_rejected",
                None,
                "expected_parent_version",
            );
            return Ok(GatewayAddVersionOutcome::ExpectedParentVersion {
                journal_id: journal.journal_id,
                expected_parent_version_id: expected,
            });
        }
    };

    transition(
        state,
        &journal,
        JournalTransition {
            from_state: GatewayJournalState::Received,
            to_state: GatewayJournalState::MergedVersionAccepted,
            merged_version_id: Some(merged_version_id),
            recovery_status: GatewayRecoveryStatus::Recoverable,
            diagnostic: None,
        },
    )
    .await?;
    audit_version_lifecycle(
        &version,
        &journal,
        "merged_sync.version_accepted",
        Some(merged_version_id),
        "accepted",
    );

    if let Err(err) = apply_source_plan(state, &version.user_id, &scope, &source_plan).await {
        let message = format!(
            "apply source plan for journal {}: {err:#}",
            journal.journal_id
        );
        mark_quarantined(
            state,
            &journal,
            GatewayJournalState::MergedVersionAccepted,
            "source_apply_failed",
            &message,
        )
        .await?;
        audit_source_apply_failure(&version, &journal, merged_version_id, &scope, &message);
        anyhow::bail!(message);
    }
    audit_source_apply_success(&version, &journal, merged_version_id, &scope, &source_plan);
    transition(
        state,
        &journal,
        JournalTransition {
            from_state: GatewayJournalState::MergedVersionAccepted,
            to_state: GatewayJournalState::SourcePlanApplied,
            merged_version_id: None,
            recovery_status: GatewayRecoveryStatus::Recoverable,
            diagnostic: None,
        },
    )
    .await?;

    if corrective_plan.requires_projection() {
        let projection = match project_personal_now(state, &version.user_id).await {
            Ok(summary) => summary,
            Err(err) => {
                let message = format!(
                    "append corrective projection for journal {}: {err:#}",
                    journal.journal_id
                );
                mark_quarantined(
                    state,
                    &journal,
                    GatewayJournalState::SourcePlanApplied,
                    "projection_append_failed",
                    &message,
                )
                .await?;
                audit_projection_failure(&version, &journal, merged_version_id, &scope, &message);
                anyhow::bail!(message);
            }
        };
        // Only claim a corrective projection when the projection actually
        // emitted ops. `requires_projection()` is true whenever an inbound
        // reserved-UDA op is seen, but if the client's value already matched
        // canonical the mirror is a no-op — emitting `*_corrected` then would
        // overclaim a correction that never happened.
        if projection.changed {
            audit_corrective_projection(
                &version,
                &journal,
                merged_version_id,
                &scope,
                &corrective_plan,
            );
        }
    }
    transition(
        state,
        &journal,
        JournalTransition {
            from_state: GatewayJournalState::SourcePlanApplied,
            to_state: GatewayJournalState::ProjectionAppended,
            merged_version_id: None,
            recovery_status: GatewayRecoveryStatus::Recoverable,
            diagnostic: None,
        },
    )
    .await?;

    transition(
        state,
        &journal,
        JournalTransition {
            from_state: GatewayJournalState::ProjectionAppended,
            to_state: GatewayJournalState::Finalized,
            merged_version_id: None,
            recovery_status: GatewayRecoveryStatus::Recovered,
            diagnostic: None,
        },
    )
    .await?;
    audit_version_lifecycle(
        &version,
        &journal,
        "merged_sync.version_finalized",
        Some(merged_version_id),
        "finalized",
    );

    Ok(GatewayAddVersionOutcome::Accepted {
        journal_id: journal.journal_id,
        version_id: merged_version_id,
    })
}

pub(super) async fn append_inbound_merged_version(
    state: &AppState,
    version: &GatewayVersion,
) -> Result<std::result::Result<Uuid, Uuid>> {
    let storage =
        crate::merged_sync_gateway::storage::open_merged_sync_storage(state, &version.user_id)
            .map_err(|status| anyhow::anyhow!("open merged sync storage: HTTP {status}"))?;
    let guard = storage.lock().unwrap_or_else(|e| e.into_inner());
    match guard.add_version(version.parent_version_id, &version.history_segment) {
        Ok(result) => Ok(result),
        Err(err) if is_sqlite_constraint_violation(&err) => {
            let latest = guard.get_latest_version_id()?;
            Ok(Err(latest))
        }
        Err(err) => Err(err),
    }
}
