use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use subtle::ConstantTimeEq;

use crate::app_state::AppState;
use crate::audit;

/// Extractor for operator-only HTTP endpoints under `/admin/*`.
///
/// This is intentionally separate from normal user bearer auth so operator
/// control-plane requests do not share the same auth boundary as end-user REST.
#[derive(Debug, Clone, Copy)]
pub struct OperatorAuth;

impl FromRequestParts<AppState> for OperatorAuth {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let client_ip =
            audit::client_ip(&parts.headers, state.config.server.trust_forwarded_headers);
        let expected = state.operator_token().ok_or_else(|| {
            tracing::warn!(
                target: "audit",
                action = "admin.auth.failure",
                source = "api",
                client_ip = %client_ip,
                reason = "admin_http_auth_not_configured",
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Admin HTTP auth is not configured",
            )
        })?;

        let auth_header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                tracing::warn!(
                    target: "audit",
                    action = "admin.auth.failure",
                    source = "api",
                    client_ip = %client_ip,
                    reason = "missing_authorization_header",
                );
                (StatusCode::UNAUTHORIZED, "Missing Authorization header")
            })?;

        let provided = auth_header.strip_prefix("Bearer ").ok_or_else(|| {
            tracing::warn!(
                target: "audit",
                action = "admin.auth.failure",
                source = "api",
                client_ip = %client_ip,
                reason = "invalid_authorization_format",
            );
            (StatusCode::UNAUTHORIZED, "Invalid Authorization format")
        })?;

        let matches = bool::from(expected.as_bytes().ct_eq(provided.as_bytes()));
        if !matches {
            tracing::warn!(
                target: "audit",
                action = "admin.auth.failure",
                source = "api",
                client_ip = %client_ip,
                reason = "invalid_operator_token",
            );
            return Err((StatusCode::UNAUTHORIZED, "Invalid operator token"));
        }

        Ok(OperatorAuth)
    }
}
