//! HTTP-edge helpers for TaskChampion sync handlers.

use anyhow::Result;
use axum::http::{header, HeaderMap, StatusCode};

use crate::app_state::AppState;
use crate::audit;

/// Content-type for history segments.
pub(super) const HISTORY_SEGMENT_CONTENT_TYPE: &str =
    "application/vnd.taskchampion.history-segment";

/// Content-type for snapshots.
pub(super) const SNAPSHOT_CONTENT_TYPE: &str = "application/vnd.taskchampion.snapshot";

/// Validate request Content-Type matches expected value (ignores parameters like charset).
pub(super) fn require_content_type(headers: &HeaderMap, expected: &str) -> Result<(), StatusCode> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type_matches(ct, expected) {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    Ok(())
}

/// Return true when a Content-Type header matches an expected media type,
/// ignoring parameters such as `charset`. `pub` + re-exported from `tc_sync`
/// so the `tc_sync_content_type` fuzz target can exercise this pure parser
/// (it is otherwise an internal helper of `require_content_type`).
pub fn content_type_matches(content_type: &str, expected: &str) -> bool {
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    media_type.eq_ignore_ascii_case(expected)
}

fn sync_client_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-client-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn log_sync_boundary_info(
    event_name: &'static str,
    operation: &'static str,
    headers: &HeaderMap,
    state: &AppState,
    user_id: Option<&str>,
    detail: Option<&str>,
) {
    tracing::info!(
        target: "boundary",
        event = event_name,
        component = "cmdock/server",
        correlation_id = ?audit::request_id(headers),
        request_id = ?audit::request_id(headers),
        sync_operation = operation,
        client_id = ?sync_client_id(headers),
        user_id = ?user_id,
        client_ip = %audit::client_ip(headers, state.config.server.trust_forwarded_headers),
        detail = ?detail,
    );
}

pub(super) fn log_sync_boundary_error(
    operation: &'static str,
    headers: &HeaderMap,
    state: &AppState,
    user_id: Option<&str>,
    reason: &str,
) {
    tracing::error!(
        target: "boundary",
        event = "sync.failed",
        component = "cmdock/server",
        correlation_id = ?audit::request_id(headers),
        request_id = ?audit::request_id(headers),
        sync_operation = operation,
        client_id = ?sync_client_id(headers),
        user_id = ?user_id,
        client_ip = %audit::client_ip(headers, state.config.server.trust_forwarded_headers),
        reason = %reason,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_type_matches_ignores_parameters() {
        assert!(content_type_matches(
            "application/vnd.taskchampion.snapshot; charset=utf-8",
            SNAPSHOT_CONTENT_TYPE
        ));
    }

    #[test]
    fn test_content_type_matches_rejects_wrong_media_type() {
        assert!(!content_type_matches(
            "application/json",
            HISTORY_SEGMENT_CONTENT_TYPE
        ));
    }
}
