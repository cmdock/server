//! Planning for inbound merged-sync versions.
//!
//! This module owns the personal-scope policy that turns decoded wire
//! operations into canonical source-write work plus any server-owned corrective
//! projection that must follow acceptance.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::app_state::AppState;
use crate::merged_sync_gateway::codec::{WireOp, WireVersion};
pub(super) use crate::task_keys::udas::{
    CMDOCK_ACCOUNT_UDA, CMDOCK_KEY_UDA, CMDOCK_TASK_SCOPE_UDA,
};

/// One task's wire operations, preserving operation indexes within the version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireTaskGroup {
    pub task_uuid: Uuid,
    pub operations: Vec<IndexedWireOp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedWireOp {
    pub operation_index: usize,
    pub op: WireOp,
}

/// One visible Task Scope for the merged gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleTaskScope {
    pub task_scope_id: String,
    pub key_prefix: String,
}

/// Locally-authorized Task Scopes visible to one Runtime User.
///
/// S5 is still personal-only, so this set contains exactly one active Personal
/// Task Scope. Future Team slices extend this set from local membership state;
/// the sync hot path must not call the control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleTaskScopeSet {
    pub scopes: Vec<VisibleTaskScope>,
}

impl VisibleTaskScopeSet {
    pub fn personal_only(scope: VisibleTaskScope) -> Self {
        Self {
            scopes: vec![scope],
        }
    }

    pub fn sole_scope(&self) -> Option<&VisibleTaskScope> {
        self.scopes.first().filter(|_| self.scopes.len() == 1)
    }
}

/// Source-truth operations to apply after a version is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceApplyPlan {
    pub groups: Vec<WireTaskGroup>,
}

/// Server-owned corrections/projection that must be appended before finalize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectiveProjectionPlan {
    pub corrections: Vec<CorrectiveProjectionReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectiveProjectionReason {
    pub task_uuid: Uuid,
    pub operation_index: usize,
    pub property: &'static str,
    pub outcome: &'static str,
}

impl CorrectiveProjectionPlan {
    pub fn requires_projection(&self) -> bool {
        !self.corrections.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlanReject {
    pub(super) code: &'static str,
    pub(super) message: String,
}

impl PlanReject {
    fn as_error_string(&self) -> String {
        format!("{}: {}", self.code, self.message)
    }
}

/// Recovery-facing replay plan prepared through the same planner used by the
/// live inbound path. Keeping this as a named facade avoids recovery parsing
/// Task Scope/key policy details on its own.
pub(super) struct PersonalReplayPlan {
    pub(super) scope: VisibleTaskScope,
    pub(super) source_plan: SourceApplyPlan,
    pub(super) corrective_plan: CorrectiveProjectionPlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PersonalReplayReject {
    pub(super) code: &'static str,
    pub(super) message: String,
}

pub(super) async fn prepare_personal_replay_plan(
    state: &AppState,
    user_id: &str,
    decoded: &WireVersion,
) -> std::result::Result<PersonalReplayPlan, PersonalReplayReject> {
    let scope = resolve_personal_visible_task_scope(state, user_id)
        .await
        .map_err(|reject| PersonalReplayReject {
            code: reject.code,
            message: reject.message,
        })?;
    let (source_plan, corrective_plan) = plan_personal_source_apply_inner(decoded, &scope)
        .map_err(|reject| PersonalReplayReject {
            code: reject.code,
            message: reject.message,
        })?;
    Ok(PersonalReplayPlan {
        scope,
        source_plan,
        corrective_plan,
    })
}

/// Decode output -> grouped source and corrective plans.
pub fn plan_personal_source_apply(
    decoded: &WireVersion,
    scope: &VisibleTaskScope,
) -> std::result::Result<(SourceApplyPlan, CorrectiveProjectionPlan), String> {
    plan_personal_source_apply_inner(decoded, scope).map_err(|reject| reject.as_error_string())
}

pub(super) async fn resolve_personal_visible_task_scope(
    state: &AppState,
    user_id: &str,
) -> std::result::Result<VisibleTaskScope, PlanReject> {
    let scope = state
        .store
        .ensure_personal_task_scope_for_user(user_id)
        .await
        .map_err(|err| PlanReject {
            code: "MISSING_TASK_SCOPE_ID",
            message: err.to_string(),
        })?
        .scope;
    Ok(VisibleTaskScope {
        task_scope_id: scope.id,
        key_prefix: scope.key_prefix,
    })
}

pub(super) async fn resolve_visible_task_scopes(
    state: &AppState,
    user_id: &str,
) -> std::result::Result<VisibleTaskScopeSet, PlanReject> {
    resolve_personal_visible_task_scope(state, user_id)
        .await
        .map(VisibleTaskScopeSet::personal_only)
}

pub(super) fn plan_personal_source_apply_inner(
    decoded: &WireVersion,
    scope: &VisibleTaskScope,
) -> std::result::Result<(SourceApplyPlan, CorrectiveProjectionPlan), PlanReject> {
    let mut grouped: BTreeMap<Uuid, Vec<IndexedWireOp>> = BTreeMap::new();
    let mut corrections = Vec::new();

    for (operation_index, op) in decoded.operations.iter().cloned().enumerate() {
        if let WireOp::Update {
            property, value, ..
        } = &op
        {
            if property == CMDOCK_TASK_SCOPE_UDA || property == CMDOCK_ACCOUNT_UDA {
                if let Some(requested_prefix) = value {
                    if !requested_prefix.eq_ignore_ascii_case(&scope.key_prefix) {
                        return Err(PlanReject {
                            code: "TASK_SCOPE_FORBIDDEN",
                            message: format!(
                                "operation {operation_index} targets forbidden Task Scope prefix {requested_prefix}"
                            ),
                        });
                    }
                }
                // Even accepted/missing scope UDA writes are canonicalized back
                // to the exact server prefix spelling.
                corrections.push(CorrectiveProjectionReason {
                    task_uuid: op.uuid(),
                    operation_index,
                    property: if property == CMDOCK_TASK_SCOPE_UDA {
                        CMDOCK_TASK_SCOPE_UDA
                    } else {
                        CMDOCK_ACCOUNT_UDA
                    },
                    outcome: "corrected",
                });
            }
            if property == CMDOCK_KEY_UDA {
                corrections.push(CorrectiveProjectionReason {
                    task_uuid: op.uuid(),
                    operation_index,
                    property: CMDOCK_KEY_UDA,
                    outcome: "corrected",
                });
            }
        }
        grouped.entry(op.uuid()).or_default().push(IndexedWireOp {
            operation_index,
            op,
        });
    }

    Ok((
        SourceApplyPlan {
            groups: grouped
                .into_iter()
                .map(|(task_uuid, operations)| WireTaskGroup {
                    task_uuid,
                    operations,
                })
                .collect(),
        },
        CorrectiveProjectionPlan { corrections },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_groups_by_uuid_and_preserves_operation_indexes() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let decoded = WireVersion {
            operations: vec![
                WireOp::Create { uuid: b },
                WireOp::Create { uuid: a },
                WireOp::Update {
                    uuid: b,
                    property: "description".to_string(),
                    value: Some("b".to_string()),
                    timestamp: chrono::Utc::now(),
                },
            ],
        };
        let scope = VisibleTaskScope {
            task_scope_id: "scope-personal".to_string(),
            key_prefix: "PERS".to_string(),
        };
        let (plan, correction) = plan_personal_source_apply_inner(&decoded, &scope).unwrap();
        assert!(!correction.requires_projection());
        assert_eq!(plan.groups.len(), 2);
        let b_group = plan.groups.iter().find(|g| g.task_uuid == b).unwrap();
        assert_eq!(
            b_group
                .operations
                .iter()
                .map(|op| op.operation_index)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
    }

    #[test]
    fn planner_rejects_forbidden_task_scope_before_acceptance() {
        let uuid = Uuid::new_v4();
        let decoded = WireVersion {
            operations: vec![WireOp::Update {
                uuid,
                property: CMDOCK_ACCOUNT_UDA.to_string(),
                value: Some("TEAM".to_string()),
                timestamp: chrono::Utc::now(),
            }],
        };
        let scope = VisibleTaskScope {
            task_scope_id: "scope-personal".to_string(),
            key_prefix: "PERS".to_string(),
        };
        let err = plan_personal_source_apply_inner(&decoded, &scope).unwrap_err();
        assert_eq!(err.code, "TASK_SCOPE_FORBIDDEN");
    }

    #[test]
    fn planner_treats_cmdock_key_as_corrective_not_authoritative() {
        let uuid = Uuid::new_v4();
        let decoded = WireVersion {
            operations: vec![WireOp::Update {
                uuid,
                property: CMDOCK_KEY_UDA.to_string(),
                value: Some("PERS-999".to_string()),
                timestamp: chrono::Utc::now(),
            }],
        };
        let scope = VisibleTaskScope {
            task_scope_id: "scope-personal".to_string(),
            key_prefix: "PERS".to_string(),
        };
        let (_plan, correction) = plan_personal_source_apply_inner(&decoded, &scope).unwrap();
        assert!(correction.requires_projection());
    }
}
