use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::admin::handlers::{require_existing_user, validate_user_id};
use crate::admin::openapi::sqlite_utc_to_rfc3339;
use crate::app_state::AppState;
use crate::audit;
use crate::auth::OperatorAuth;
use crate::runtime_policy::{
    enforcement_state, RuntimePolicy, RuntimePolicyEnforcementState, RuntimePolicyService,
};
use crate::store::models::RuntimePolicyRecord;

#[derive(Debug, Deserialize, ToSchema, Validate)]
#[schema(example = json!({
    "policyVersion": "2026-04-03T12:00:00Z",
    "policy": {
        "runtimeAccess": "block",
        "deleteAction": "forbid"
    }
}))]
#[serde(rename_all = "camelCase")]
pub struct ApplyRuntimePolicyRequest {
    #[garde(
        length(min = 1, max = 128),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub policy_version: String,
    #[garde(skip)]
    pub policy: RuntimePolicy,
}

#[derive(Debug, Serialize, ToSchema)]
#[schema(example = json!({
    "userId": "86a9cca3-5689-41e4-8361-8075c9c49b38",
    "desiredVersion": "2026-04-03T12:00:00Z",
    "desiredPolicy": {
        "runtimeAccess": "block",
        "deleteAction": "forbid"
    },
    "appliedVersion": "2026-04-03T12:00:00Z",
    "appliedPolicy": {
        "runtimeAccess": "block",
        "deleteAction": "forbid"
    },
    "enforcementState": "current",
    "appliedAt": "2026-04-03T12:00:01+00:00",
    "updatedAt": "2026-04-03T12:00:01+00:00"
}))]
#[serde(rename_all = "camelCase")]
pub struct OperatorRuntimePolicyResponse {
    #[schema(format = "uuid", example = "86a9cca3-5689-41e4-8361-8075c9c49b38")]
    pub user_id: String,
    pub desired_version: Option<String>,
    pub desired_policy: Option<RuntimePolicy>,
    pub applied_version: Option<String>,
    pub applied_policy: Option<RuntimePolicy>,
    #[schema(value_type = RuntimePolicyEnforcementState, example = "current")]
    pub enforcement_state: RuntimePolicyEnforcementState,
    #[schema(format = "date-time", example = "2026-04-03T12:00:01+00:00")]
    pub applied_at: Option<String>,
    #[schema(format = "date-time", example = "2026-04-03T12:00:01+00:00")]
    pub updated_at: Option<String>,
}

#[utoipa::path(
    get,
    path = "/admin/user/{user_id}/runtime-policy",
    operation_id = "getOperatorRuntimePolicy",
    params(("user_id" = String, Path, description = "User ID")),
    responses(
        (status = 200, description = "Desired/applied runtime policy for the target user", body = OperatorRuntimePolicyResponse),
        (status = 400, description = "Invalid user ID"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User not found"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn get_runtime_policy(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path(user_id): Path<String>,
) -> Result<Json<OperatorRuntimePolicyResponse>, StatusCode> {
    validate_user_id(&user_id)?;
    require_existing_user(&state, &user_id).await?;

    let service = RuntimePolicyService::new(state.store.clone());
    let policy = service
        .get_for_user(&user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(map_runtime_policy_response(&user_id, policy.as_ref())))
}

#[utoipa::path(
    put,
    path = "/admin/user/{user_id}/runtime-policy",
    operation_id = "applyOperatorRuntimePolicy",
    params(("user_id" = String, Path, description = "User ID")),
    request_body = ApplyRuntimePolicyRequest,
    responses(
        (status = 200, description = "Desired/applied runtime policy after apply", body = OperatorRuntimePolicyResponse),
        (status = 400, description = "Invalid user ID or runtime policy request"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User not found"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn apply_runtime_policy(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(body): Json<ApplyRuntimePolicyRequest>,
) -> Result<Json<OperatorRuntimePolicyResponse>, (StatusCode, String)> {
    validate_user_id(&user_id).map_err(|status| (status, "Invalid user ID".to_string()))?;
    crate::validation::validate_or_bad_request_text(&body, "Invalid runtime policy request")?;
    require_existing_user(&state, &user_id)
        .await
        .map_err(|status| match status {
            StatusCode::NOT_FOUND => (StatusCode::NOT_FOUND, "User not found".to_string()),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error".to_string(),
            ),
        })?;

    let service = RuntimePolicyService::new(state.store.clone());
    let policy = service
        .apply_for_user(&user_id, &body.policy_version, &body.policy)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error".to_string(),
            )
        })?;

    tracing::info!(
        target: "audit",
        action = "admin.runtime_policy.apply",
        source = "api",
        client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
        user_id = %user_id,
        desired_version = %policy.desired_version,
        runtime_access = ?policy.desired_policy.runtime_access,
        delete_action = ?policy.desired_policy.delete_action,
    );

    Ok(Json(map_runtime_policy_response(&user_id, Some(&policy))))
}

fn map_runtime_policy_response(
    user_id: &str,
    policy: Option<&RuntimePolicyRecord>,
) -> OperatorRuntimePolicyResponse {
    OperatorRuntimePolicyResponse {
        user_id: user_id.to_string(),
        desired_version: policy.map(|record| record.desired_version.clone()),
        desired_policy: policy.map(|record| record.desired_policy.clone()),
        applied_version: policy.and_then(|record| record.applied_version.clone()),
        applied_policy: policy.and_then(|record| record.applied_policy.clone()),
        enforcement_state: enforcement_state(policy),
        applied_at: policy
            .and_then(|record| record.applied_at.as_deref())
            .map(sqlite_utc_to_rfc3339),
        updated_at: policy.map(|record| sqlite_utc_to_rfc3339(&record.updated_at)),
    }
}
