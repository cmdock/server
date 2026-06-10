//! Admin/diagnostic endpoints for ops.
//!
//! These endpoints provide detailed server status, per-user diagnostics,
//! and operational actions (evict replica, WAL checkpoint).
//!
//! Protected by the dedicated operator HTTP auth boundary, not ordinary user
//! bearer auth.

use crate::app_state::AppState;
use crate::auth::OperatorAuth;
use crate::runtime_recovery::StartupRecoverySnapshot;
use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;
use utoipa::ToSchema;

/// Validate that a user_id doesn't contain path traversal characters.
pub(crate) fn validate_user_id(user_id: &str) -> Result<(), StatusCode> {
    if user_id.contains('/') || user_id.contains('\\') || user_id.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(())
}

pub(crate) async fn require_existing_user(
    state: &AppState,
    user_id: &str,
) -> Result<(), StatusCode> {
    if state
        .store
        .get_user_by_id(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .is_none()
    {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(())
}

/// Detailed server status for ops dashboards.
#[derive(Serialize, ToSchema)]
#[schema(example = json!({
    "status": "ok",
    "uptimeSeconds": 5234.2,
    "cachedReplicas": 4,
    "quarantinedUsers": 1,
    "startupRecovery": {
        "totalUsers": 12,
        "healthyUsers": 10,
        "rebuildableUsers": 1,
        "needsOperatorAttentionUsers": 1,
        "alreadyOfflineUsers": 0,
        "newlyOfflinedUsers": ["86a9cca3-5689-41e4-8361-8075c9c49b38"],
        "orphanUserDirs": []
    },
    "authCacheSize": "LRU/1024",
    "configDb": "ok",
    "llmCircuitBreaker": "closed"
}))]
pub struct ServerStatus {
    pub status: String,
    pub uptime_seconds: f64,
    pub cached_replicas: usize,
    pub quarantined_users: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_recovery: Option<StartupRecoverySnapshot>,
    pub auth_cache_size: String,
    pub config_db: String,
    pub llm_circuit_breaker: String,
}

/// GET /admin/status — detailed server health for ops.
#[utoipa::path(
    get,
    path = "/admin/status",
    operation_id = "getAdminStatus",
    responses(
        (status = 200, description = "Operator server diagnostics", body = ServerStatus),
        (status = 401, description = "Invalid operator token"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn server_status(
    State(state): State<AppState>,
    _auth: OperatorAuth,
) -> Json<ServerStatus> {
    let cached = state.replica_manager.replica_count();
    let llm_status = state.llm_circuit_breaker.status();

    Json(ServerStatus {
        status: "ok".to_string(),
        uptime_seconds: state.started_at.elapsed().as_secs_f64(),
        cached_replicas: cached,
        quarantined_users: state.recovery_runtime.quarantined_user_count(),
        startup_recovery: state.recovery_runtime.startup_recovery_snapshot(),
        auth_cache_size: "LRU/1024".to_string(),
        config_db: "ok".to_string(),
        llm_circuit_breaker: llm_status,
    })
}
