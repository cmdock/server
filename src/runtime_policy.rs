use std::sync::Arc;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::store::models::RuntimePolicyRecord;
use crate::store::ConfigStore;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAccessMode {
    Allow,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeDeleteAction {
    Allow,
    Forbid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[schema(example = json!({
    "runtimeAccess": "allow",
    "deleteAction": "forbid"
}))]
#[serde(rename_all = "camelCase")]
pub struct RuntimePolicy {
    pub runtime_access: RuntimeAccessMode,
    pub delete_action: RuntimeDeleteAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePolicyEnforcementState {
    Unmanaged,
    Current,
    MissingApplied,
    StaleApplied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeAccessDecision {
    Allow,
    Blocked,
    NotCurrent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeDeleteDecision {
    Allow,
    Forbidden,
    NotCurrent,
}

#[derive(Clone)]
pub struct RuntimePolicyService {
    store: Arc<dyn ConfigStore>,
}

impl RuntimePolicyService {
    pub fn new(store: Arc<dyn ConfigStore>) -> Self {
        Self { store }
    }

    pub async fn get_for_user(&self, user_id: &str) -> anyhow::Result<Option<RuntimePolicyRecord>> {
        self.store.get_runtime_policy(user_id).await
    }

    pub async fn apply_for_user(
        &self,
        user_id: &str,
        policy_version: &str,
        policy: &RuntimePolicy,
    ) -> anyhow::Result<RuntimePolicyRecord> {
        let applied_at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        self.store
            .upsert_runtime_policy(
                user_id,
                policy_version,
                policy,
                Some(policy_version),
                Some(policy),
                Some(&applied_at),
            )
            .await
    }

    pub async fn runtime_access_for_user(
        &self,
        user_id: &str,
    ) -> anyhow::Result<RuntimeAccessDecision> {
        runtime_access_for_user(self.store.as_ref(), user_id).await
    }

    pub async fn runtime_delete_for_user(
        &self,
        user_id: &str,
    ) -> anyhow::Result<RuntimeDeleteDecision> {
        runtime_delete_for_user(self.store.as_ref(), user_id).await
    }
}

pub async fn runtime_access_for_user(
    store: &dyn ConfigStore,
    user_id: &str,
) -> anyhow::Result<RuntimeAccessDecision> {
    let policy = store.get_runtime_policy(user_id).await?;
    Ok(runtime_access_decision(policy.as_ref()))
}

pub async fn runtime_delete_for_user(
    store: &dyn ConfigStore,
    user_id: &str,
) -> anyhow::Result<RuntimeDeleteDecision> {
    let policy = store.get_runtime_policy(user_id).await?;
    Ok(runtime_delete_decision(policy.as_ref()))
}

pub fn enforcement_state(policy: Option<&RuntimePolicyRecord>) -> RuntimePolicyEnforcementState {
    let Some(policy) = policy else {
        return RuntimePolicyEnforcementState::Unmanaged;
    };

    match (&policy.applied_version, &policy.applied_policy) {
        (Some(applied_version), Some(applied_policy))
            if applied_version == &policy.desired_version
                && applied_policy == &policy.desired_policy =>
        {
            RuntimePolicyEnforcementState::Current
        }
        (None, _) | (_, None) => RuntimePolicyEnforcementState::MissingApplied,
        _ => RuntimePolicyEnforcementState::StaleApplied,
    }
}

pub fn runtime_access_decision(policy: Option<&RuntimePolicyRecord>) -> RuntimeAccessDecision {
    match enforcement_state(policy) {
        RuntimePolicyEnforcementState::Unmanaged => RuntimeAccessDecision::Allow,
        RuntimePolicyEnforcementState::Current => {
            let applied = policy
                .and_then(|record| record.applied_policy.as_ref())
                .expect("current runtime policy must include applied policy");
            match applied.runtime_access {
                RuntimeAccessMode::Allow => RuntimeAccessDecision::Allow,
                RuntimeAccessMode::Block => RuntimeAccessDecision::Blocked,
            }
        }
        RuntimePolicyEnforcementState::MissingApplied
        | RuntimePolicyEnforcementState::StaleApplied => RuntimeAccessDecision::NotCurrent,
    }
}

pub fn runtime_delete_decision(policy: Option<&RuntimePolicyRecord>) -> RuntimeDeleteDecision {
    match enforcement_state(policy) {
        RuntimePolicyEnforcementState::Unmanaged => RuntimeDeleteDecision::Allow,
        RuntimePolicyEnforcementState::Current => {
            let applied = policy
                .and_then(|record| record.applied_policy.as_ref())
                .expect("current runtime policy must include applied policy");
            match applied.delete_action {
                RuntimeDeleteAction::Allow => RuntimeDeleteDecision::Allow,
                RuntimeDeleteAction::Forbid => RuntimeDeleteDecision::Forbidden,
            }
        }
        RuntimePolicyEnforcementState::MissingApplied
        | RuntimePolicyEnforcementState::StaleApplied => RuntimeDeleteDecision::NotCurrent,
    }
}

pub fn runtime_access_reason(decision: RuntimeAccessDecision) -> Option<&'static str> {
    match decision {
        RuntimeAccessDecision::Allow => None,
        RuntimeAccessDecision::Blocked => Some("runtime_policy_blocked"),
        RuntimeAccessDecision::NotCurrent => Some("runtime_policy_not_current"),
    }
}

pub fn runtime_access_message(decision: RuntimeAccessDecision) -> Option<&'static str> {
    match decision {
        RuntimeAccessDecision::Allow => None,
        RuntimeAccessDecision::Blocked => Some("Runtime access blocked by policy"),
        RuntimeAccessDecision::NotCurrent => Some("Runtime policy is stale or not applied"),
    }
}

pub fn runtime_delete_message(decision: RuntimeDeleteDecision) -> Option<&'static str> {
    match decision {
        RuntimeDeleteDecision::Allow => None,
        RuntimeDeleteDecision::Forbidden => {
            Some("User deletion rejected: applied runtime policy does not allow deletion")
        }
        RuntimeDeleteDecision::NotCurrent => {
            Some("User deletion rejected: runtime policy is stale or not applied")
        }
    }
}
