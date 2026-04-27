use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode};

use crate::audit;
use crate::runtime_policy::{
    runtime_access_for_user, runtime_access_message, runtime_access_reason, RuntimeAccessDecision,
};
use crate::store::ConfigStore;

pub struct RuntimeAccessRejection {
    pub status: StatusCode,
    pub message: &'static str,
}

pub async fn enforce_runtime_access(
    store: Arc<dyn ConfigStore>,
    headers: &HeaderMap,
    user_id: &str,
    client_id: Option<&str>,
    trust_forwarded_headers: bool,
) -> Result<(), RuntimeAccessRejection> {
    let decision = runtime_access_for_user(store.as_ref(), user_id)
        .await
        .map_err(|_| RuntimeAccessRejection {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "Database error",
        })?;

    match decision {
        RuntimeAccessDecision::Allow => Ok(()),
        RuntimeAccessDecision::Blocked | RuntimeAccessDecision::NotCurrent => {
            let reason = runtime_access_reason(decision).expect("rejected decisions have reasons");
            let message =
                runtime_access_message(decision).expect("rejected decisions have messages");
            tracing::warn!(
                target: "audit",
                action = "auth.failure",
                source = "api",
                client_ip = %audit::client_ip(headers, trust_forwarded_headers),
                user_id = %user_id,
                client_id = client_id,
                reason = reason,
            );
            let status = match decision {
                RuntimeAccessDecision::Blocked => StatusCode::FORBIDDEN,
                RuntimeAccessDecision::NotCurrent => StatusCode::SERVICE_UNAVAILABLE,
                RuntimeAccessDecision::Allow => unreachable!(),
            };
            Err(RuntimeAccessRejection { status, message })
        }
    }
}
