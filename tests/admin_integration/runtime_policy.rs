//! Admin runtime policy integration tests.

use super::support::*;
use cmdock_server::runtime_policy::{RuntimeAccessMode, RuntimeDeleteAction, RuntimePolicy};
use serde_json::Value;

#[tokio::test]
async fn test_operator_runtime_policy_apply_and_readback() {
    let env = setup_bootstrap().await;
    let (h, v) = auth_header(&env.admin_token);

    let initial = env
        .server
        .get(&format!("/admin/user/{}/runtime-policy", env.user_id))
        .add_header(h.clone(), v.clone())
        .await;
    initial.assert_status_ok();
    let initial_body: Value = initial.json();
    assert_eq!(initial_body["enforcementState"], "unmanaged");
    assert!(initial_body["desiredVersion"].is_null());
    assert!(initial_body["appliedVersion"].is_null());

    let resp = env
        .server
        .put(&format!("/admin/user/{}/runtime-policy", env.user_id))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "policyVersion": "runtime-v1",
            "policy": {
                "runtimeAccess": "block",
                "deleteAction": "forbid"
            }
        }))
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["desiredVersion"], "runtime-v1");
    assert_eq!(body["appliedVersion"], "runtime-v1");
    assert_eq!(body["desiredPolicy"]["runtimeAccess"], "block");
    assert_eq!(body["desiredPolicy"]["deleteAction"], "forbid");
    assert_eq!(body["enforcementState"], "current");
    assert_rfc3339_timestamp(&body["appliedAt"]);
    assert_rfc3339_timestamp(&body["updatedAt"]);

    let readback = env
        .server
        .get(&format!("/admin/user/{}/runtime-policy", env.user_id))
        .add_header(h, v)
        .await;
    readback.assert_status_ok();
    let readback_body: Value = readback.json();
    assert_eq!(readback_body["desiredVersion"], "runtime-v1");
    assert_eq!(readback_body["appliedVersion"], "runtime-v1");
    assert_eq!(readback_body["enforcementState"], "current");
}

#[tokio::test]
async fn test_operator_runtime_policy_rejects_empty_policy_version() {
    let env = setup_bootstrap().await;
    let (h, v) = auth_header(&env.admin_token);

    env.server
        .put(&format!("/admin/user/{}/runtime-policy", env.user_id))
        .add_header(h, v)
        .json(&serde_json::json!({
            "policyVersion": "   ",
            "policy": {
                "runtimeAccess": "block",
                "deleteAction": "forbid"
            }
        }))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_runtime_policy_blocks_bearer_access_until_reactivated() {
    let env = setup_bootstrap().await;
    let policy = RuntimePolicy {
        runtime_access: RuntimeAccessMode::Block,
        delete_action: RuntimeDeleteAction::Allow,
    };
    env.store
        .upsert_runtime_policy(
            &env.user_id,
            "block-v1",
            &policy,
            Some("block-v1"),
            Some(&policy),
            Some("2026-04-03 12:00:00"),
        )
        .await
        .unwrap();

    let (user_h, user_v) = auth_header(&env.token);
    let blocked = env
        .server
        .get("/api/tasks")
        .add_header(user_h.clone(), user_v.clone())
        .await;
    blocked.assert_status(axum::http::StatusCode::FORBIDDEN);
    assert!(blocked.text().contains("Runtime access blocked by policy"));

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    env.server
        .put(&format!("/admin/user/{}/runtime-policy", env.user_id))
        .add_header(admin_h, admin_v)
        .json(&serde_json::json!({
            "policyVersion": "allow-v2",
            "policy": {
                "runtimeAccess": "allow",
                "deleteAction": "allow"
            }
        }))
        .await
        .assert_status_ok();

    env.server
        .get("/api/tasks")
        .add_header(user_h, user_v)
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn test_stale_runtime_policy_fails_safe_for_runtime_access() {
    let env = setup_bootstrap().await;
    let desired = RuntimePolicy {
        runtime_access: RuntimeAccessMode::Allow,
        delete_action: RuntimeDeleteAction::Allow,
    };
    let applied = RuntimePolicy {
        runtime_access: RuntimeAccessMode::Block,
        delete_action: RuntimeDeleteAction::Allow,
    };
    env.store
        .upsert_runtime_policy(
            &env.user_id,
            "desired-v2",
            &desired,
            Some("applied-v1"),
            Some(&applied),
            Some("2026-04-03 12:00:00"),
        )
        .await
        .unwrap();

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let policy_resp = env
        .server
        .get(&format!("/admin/user/{}/runtime-policy", env.user_id))
        .add_header(admin_h, admin_v)
        .await;
    policy_resp.assert_status_ok();
    let policy_body: Value = policy_resp.json();
    assert_eq!(policy_body["enforcementState"], "stale_applied");

    let (user_h, user_v) = auth_header(&env.token);
    let resp = env
        .server
        .get("/api/tasks")
        .add_header(user_h, user_v)
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert!(resp
        .text()
        .contains("Runtime policy is stale or not applied"));
}
