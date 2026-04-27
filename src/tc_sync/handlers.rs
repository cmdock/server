//! HTTP handlers for the TaskChampion sync protocol.
//!
//! These 4 endpoints implement the server side of the TaskChampion sync protocol,
//! allowing `task sync` to work against the cmdock server.
//!
//! Auth: X-Client-Id header → devices table → user. Devices authenticate with
//! distinct credentials, but the server stores one shared per-user TaskChampion
//! sync chain and re-encrypts protocol payloads at the HTTP boundary.
//!
//! Reference: https://gothenburgbitfactory.org/taskchampion/sync-protocol.html

use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::audit;
use crate::metrics as m;
use crate::replica;
use crate::webhooks::summary;

use super::auth::authenticate_sync_client;
use super::payloads::{
    ensure_device_bridge_ready, translate_inbound_device_payload,
    translate_outbound_canonical_payload,
};
use super::runtime::{
    handle_sync_error, open_sync_storage, reconcile_after_tc_write, sync_device_if_stale,
    InFlightGuard,
};

/// Content-type for history segments.
const HISTORY_SEGMENT_CONTENT_TYPE: &str = "application/vnd.taskchampion.history-segment";

/// Content-type for snapshots.
const SNAPSHOT_CONTENT_TYPE: &str = "application/vnd.taskchampion.snapshot";

/// Versions since last snapshot before requesting one.
const SNAPSHOT_URGENCY_THRESHOLD: u64 = 100;
const SNAPSHOT_URGENCY_HIGH_THRESHOLD: u64 = 500;

/// Validate request Content-Type matches expected value (ignores parameters like charset).
fn require_content_type(headers: &HeaderMap, expected: &str) -> Result<(), StatusCode> {
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
/// ignoring parameters such as `charset`.
pub fn content_type_matches(content_type: &str, expected: &str) -> bool {
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    media_type.eq_ignore_ascii_case(expected)
}

/// Determine snapshot urgency based on versions since last snapshot.
fn snapshot_urgency(versions_since: u64) -> Option<&'static str> {
    match versions_since {
        n if n >= SNAPSHOT_URGENCY_HIGH_THRESHOLD => Some("urgency=high"),
        n if n >= SNAPSHOT_URGENCY_THRESHOLD => Some("urgency=low"),
        _ => None,
    }
}

fn sync_client_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-client-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn log_sync_boundary_info(
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

fn log_sync_boundary_error(
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

/// POST /v1/client/add-version/{parent_version_id}
///
/// Accept a new version (history segment) from a client.
/// Returns 200 with X-Version-Id on success, 409 on conflict.
pub async fn add_version(
    State(state): State<AppState>,
    Path(parent_version_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    log_sync_boundary_info(
        "sync.request_received",
        "add_version",
        &headers,
        &state,
        None,
        None,
    );
    let auth = match authenticate_sync_client(&state, &headers).await {
        Ok(a) => a,
        Err(resp) => {
            log_sync_boundary_error("add_version", &headers, &state, None, "auth_failed");
            return resp;
        }
    };
    let user_id = auth.user_id.clone();
    if let Err(status) = require_content_type(&headers, HISTORY_SEGMENT_CONTENT_TYPE) {
        log_sync_boundary_error(
            "add_version",
            &headers,
            &state,
            Some(&user_id),
            "invalid_content_type",
        );
        return status.into_response();
    }
    if let Err(status) = ensure_device_bridge_ready(&state, &auth.device) {
        log_sync_boundary_error(
            "add_version",
            &headers,
            &state,
            Some(&user_id),
            "device_bridge_not_ready",
        );
        return status.into_response();
    }
    let body = match translate_inbound_device_payload(
        &state,
        &auth.device,
        parent_version_id,
        body.as_ref(),
    )
    .await
    {
        Ok(body) => Bytes::from(body),
        Err(status) => {
            log_sync_boundary_error(
                "add_version",
                &headers,
                &state,
                Some(&user_id),
                "payload_translation_failed",
            );
            return status.into_response();
        }
    };
    // Body size enforced by RequestBodyLimitLayer on the route group

    let storage = match open_sync_storage(&state, &auth.device) {
        Ok(s) => s,
        Err(status) => {
            log_sync_boundary_error(
                "add_version",
                &headers,
                &state,
                Some(&user_id),
                "sync_storage_unavailable",
            );
            return status.into_response();
        }
    };

    let body_len = body.len();
    m::record_sync_body_size("add_version", body_len);

    let _in_flight = InFlightGuard::new();
    let start = Instant::now();
    let result = replica::retry_with_jitter("sync_add_version", 4, move || {
        let storage = Arc::clone(&storage);
        let body = body.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                let guard = storage.lock().unwrap_or_else(|e| e.into_inner());
                let add_result = guard.add_version(parent_version_id, &body)?;
                // Propagate urgency errors (could indicate corruption) instead of swallowing
                let urgency_count = guard.versions_since_snapshot()?;
                Ok::<_, anyhow::Error>((add_result, urgency_count))
            })
            .await
            .map_err(|e| anyhow::anyhow!("add_version task panicked: {e}"))?
        }
    })
    .await;
    let elapsed = start.elapsed().as_secs_f64();

    match result {
        Ok((Ok(version_id), urgency_count)) => {
            let before_sync = match summary::capture_sync_snapshot(&state, &user_id).await {
                Ok(snapshot) => Some(snapshot),
                Err(err) => {
                    tracing::warn!(
                        user_id = %user_id,
                        error = %err,
                        "Failed to capture pre-sync task snapshot for sync.completed webhook"
                    );
                    None
                }
            };
            m::record_sync_op("add_version", elapsed, "ok");
            log_sync_boundary_info(
                "sync.complete",
                "add_version",
                &headers,
                &state,
                Some(&user_id),
                Some("ok"),
            );
            tracing::info!(
                target: "audit",
                action = "sync.add_version",
                source = "api",
                user_id = %user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                version_id = %version_id,
                body_bytes = body_len,
            );
            if let Err(status) =
                reconcile_after_tc_write(&state, &auth.device, "tc_write_fallback").await
            {
                log_sync_boundary_error(
                    "add_version",
                    &headers,
                    &state,
                    Some(&user_id),
                    "reconcile_failed",
                );
                return status.into_response();
            }
            summary::emit_sync_completed_if_changed(
                &state,
                &user_id,
                audit::request_id(&headers),
                before_sync,
            )
            .await;
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header("X-Version-Id", version_id.to_string())
                .header(header::CONTENT_TYPE, HISTORY_SEGMENT_CONTENT_TYPE);
            if let Some(urgency) = snapshot_urgency(urgency_count) {
                let level = if urgency.contains("high") {
                    "high"
                } else {
                    "low"
                };
                m::record_sync_snapshot_urgency(level);
                builder = builder.header("X-Snapshot-Request", urgency);
            }
            builder.body(axum::body::Body::empty()).unwrap()
        }
        Ok((Err(expected_parent), _)) => {
            m::record_sync_op("add_version", elapsed, "conflict");
            m::record_sync_conflict();
            log_sync_boundary_error("add_version", &headers, &state, Some(&user_id), "conflict");
            Response::builder()
                .status(StatusCode::CONFLICT)
                .header("X-Parent-Version-Id", expected_parent.to_string())
                .body(axum::body::Body::empty())
                .unwrap()
        }
        Err(e) => {
            // UNIQUE constraint violation from concurrent insert → treat as conflict
            // SQLite UNIQUE constraint violation = SQLITE_CONSTRAINT (code 19)
            let is_constraint_violation = e.chain().any(|cause| {
                if let Some(sqlite_err) = cause.downcast_ref::<rusqlite::Error>() {
                    matches!(
                        sqlite_err,
                        rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error {
                                code: rusqlite::ffi::ErrorCode::ConstraintViolation,
                                ..
                            },
                            _
                        )
                    )
                } else {
                    false
                }
            });
            if is_constraint_violation {
                tracing::warn!("add_version concurrent conflict for user {}: {e}", user_id);
                // Re-read latest to return proper conflict response.
                // Use open_sync_storage (not direct get_or_open) to respect quarantine.
                let re_read_storage = match open_sync_storage(&state, &auth.device) {
                    Ok(s) => s,
                    Err(status) => {
                        m::record_sync_op("add_version", elapsed, "error");
                        log_sync_boundary_error(
                            "add_version",
                            &headers,
                            &state,
                            Some(&user_id),
                            "sync_storage_unavailable",
                        );
                        return status.into_response();
                    }
                };
                let latest = tokio::task::spawn_blocking(move || {
                    let guard = re_read_storage.lock().unwrap_or_else(|e| e.into_inner());
                    guard.get_latest_version_id()
                })
                .await;

                // Record metrics after re-read to reflect final outcome
                match latest {
                    Ok(Ok(vid)) if vid != Uuid::nil() => {
                        m::record_sync_op("add_version", elapsed, "conflict");
                        m::record_sync_conflict();
                        log_sync_boundary_error(
                            "add_version",
                            &headers,
                            &state,
                            Some(&user_id),
                            "conflict",
                        );
                        Response::builder()
                            .status(StatusCode::CONFLICT)
                            .header("X-Parent-Version-Id", vid.to_string())
                            .body(axum::body::Body::empty())
                            .unwrap()
                    }
                    Ok(Err(ref re_err)) if replica::is_corruption_in_chain(re_err) => {
                        // Re-read hit corruption — quarantine
                        log_sync_boundary_error(
                            "add_version",
                            &headers,
                            &state,
                            Some(&user_id),
                            "corruption_detected",
                        );
                        handle_sync_error(&state, &user_id, re_err, "add_version_reread", "sync")
                            .into_response()
                    }
                    _ => {
                        m::record_sync_op("add_version", elapsed, "error");
                        log_sync_boundary_error(
                            "add_version",
                            &headers,
                            &state,
                            Some(&user_id),
                            "reread_failed",
                        );
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    }
                }
            } else {
                m::record_sync_op("add_version", elapsed, "error");
                log_sync_boundary_error(
                    "add_version",
                    &headers,
                    &state,
                    Some(&user_id),
                    "storage_error",
                );
                handle_sync_error(&state, &user_id, &e, "add_version", "sync").into_response()
            }
        }
    }
}

/// GET /v1/client/get-child-version/{parent_version_id}
///
/// Return the version that is a child of the given parent.
/// Returns 200 with the history segment, 404 if up to date, or 410 if
/// the parent is unknown and the server has existing data (sync error).
pub async fn get_child_version(
    State(state): State<AppState>,
    Path(parent_version_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    log_sync_boundary_info(
        "sync.request_received",
        "get_child_version",
        &headers,
        &state,
        None,
        None,
    );
    let auth = match authenticate_sync_client(&state, &headers).await {
        Ok(a) => a,
        Err(resp) => {
            log_sync_boundary_error("get_child_version", &headers, &state, None, "auth_failed");
            return resp;
        }
    };
    let user_id = auth.user_id.clone();

    sync_device_if_stale(&state, &auth.device).await;

    let storage = match open_sync_storage(&state, &auth.device) {
        Ok(s) => s,
        Err(status) => {
            log_sync_boundary_error(
                "get_child_version",
                &headers,
                &state,
                Some(&user_id),
                "sync_storage_unavailable",
            );
            return status.into_response();
        }
    };

    let _in_flight = InFlightGuard::new();
    let start = Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        let guard = storage.lock().unwrap_or_else(|e| e.into_inner());
        guard.get_child_version_with_context(parent_version_id)
    })
    .await;
    let elapsed = start.elapsed().as_secs_f64();

    match result {
        Ok(Ok((Some((version_id, parent_id, history_segment)), _, _))) => {
            m::record_sync_op("get_child_version", elapsed, "ok");
            m::record_sync_body_size("get_child_version", history_segment.len());
            log_sync_boundary_info(
                "sync.complete",
                "get_child_version",
                &headers,
                &state,
                Some(&user_id),
                Some("ok"),
            );
            Response::builder()
                .status(StatusCode::OK)
                .header("X-Version-Id", version_id.to_string())
                .header("X-Parent-Version-Id", parent_id.to_string())
                .header(header::CONTENT_TYPE, HISTORY_SEGMENT_CONTENT_TYPE)
                .header(header::CACHE_CONTROL, "no-store")
                .body(axum::body::Body::from(
                    match translate_outbound_canonical_payload(
                        &state,
                        &auth.device,
                        parent_id,
                        &history_segment,
                    )
                    .await
                    {
                        Ok(body) => body,
                        Err(status) => {
                            log_sync_boundary_error(
                                "get_child_version",
                                &headers,
                                &state,
                                Some(&user_id),
                                "payload_translation_failed",
                            );
                            return status.into_response();
                        }
                    },
                ))
                .unwrap()
        }
        Ok(Ok((None, true, _))) | Ok(Ok((None, false, false))) => {
            m::record_sync_op("get_child_version", elapsed, "ok");
            log_sync_boundary_info(
                "sync.complete",
                "get_child_version",
                &headers,
                &state,
                Some(&user_id),
                Some("not_found"),
            );
            StatusCode::NOT_FOUND.into_response()
        }
        Ok(Ok((None, false, true))) => {
            m::record_sync_op("get_child_version", elapsed, "ok");
            log_sync_boundary_info(
                "sync.complete",
                "get_child_version",
                &headers,
                &state,
                Some(&user_id),
                Some("gone"),
            );
            StatusCode::GONE.into_response()
        }
        Ok(Err(e)) => {
            m::record_sync_op("get_child_version", elapsed, "error");
            log_sync_boundary_error(
                "get_child_version",
                &headers,
                &state,
                Some(&user_id),
                "storage_error",
            );
            handle_sync_error(&state, &user_id, &e, "get_child_version", "sync").into_response()
        }
        Err(e) => {
            m::record_sync_op("get_child_version", elapsed, "error");
            tracing::error!("get_child_version task panicked: {e}");
            log_sync_boundary_error(
                "get_child_version",
                &headers,
                &state,
                Some(&user_id),
                "task_panic",
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// POST /v1/client/add-snapshot/{version_id}
///
/// Accept a snapshot for a specific version. Returns 400 if the version_id
/// doesn't exist in the version chain.
pub async fn add_snapshot(
    State(state): State<AppState>,
    Path(version_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    log_sync_boundary_info(
        "sync.request_received",
        "add_snapshot",
        &headers,
        &state,
        None,
        None,
    );
    let auth = match authenticate_sync_client(&state, &headers).await {
        Ok(a) => a,
        Err(resp) => {
            log_sync_boundary_error("add_snapshot", &headers, &state, None, "auth_failed");
            return resp;
        }
    };
    let user_id = auth.user_id.clone();
    if let Err(status) = require_content_type(&headers, SNAPSHOT_CONTENT_TYPE) {
        log_sync_boundary_error(
            "add_snapshot",
            &headers,
            &state,
            Some(&user_id),
            "invalid_content_type",
        );
        return status.into_response();
    }
    if let Err(status) = ensure_device_bridge_ready(&state, &auth.device) {
        log_sync_boundary_error(
            "add_snapshot",
            &headers,
            &state,
            Some(&user_id),
            "device_bridge_not_ready",
        );
        return status.into_response();
    }
    let body =
        match translate_inbound_device_payload(&state, &auth.device, version_id, body.as_ref())
            .await
        {
            Ok(body) => Bytes::from(body),
            Err(status) => {
                log_sync_boundary_error(
                    "add_snapshot",
                    &headers,
                    &state,
                    Some(&user_id),
                    "payload_translation_failed",
                );
                return status.into_response();
            }
        };

    let body_len = body.len();
    m::record_sync_body_size("add_snapshot", body_len);

    let storage = match open_sync_storage(&state, &auth.device) {
        Ok(s) => s,
        Err(status) => {
            log_sync_boundary_error(
                "add_snapshot",
                &headers,
                &state,
                Some(&user_id),
                "sync_storage_unavailable",
            );
            return status.into_response();
        }
    };

    let _in_flight = InFlightGuard::new();
    let start = Instant::now();
    let result = replica::retry_with_jitter("sync_add_snapshot", 4, move || {
        let storage = Arc::clone(&storage);
        let body = body.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                let guard = storage.lock().unwrap_or_else(|e| e.into_inner());
                guard.add_snapshot(version_id, &body)
            })
            .await
            .map_err(|e| anyhow::anyhow!("add_snapshot task panicked: {e}"))?
        }
    })
    .await;
    let elapsed = start.elapsed().as_secs_f64();

    match result {
        Ok(true) => {
            let before_sync = match summary::capture_sync_snapshot(&state, &user_id).await {
                Ok(snapshot) => Some(snapshot),
                Err(err) => {
                    tracing::warn!(
                        user_id = %user_id,
                        error = %err,
                        "Failed to capture pre-sync task snapshot for sync.completed webhook"
                    );
                    None
                }
            };
            m::record_sync_op("add_snapshot", elapsed, "ok");
            log_sync_boundary_info(
                "sync.complete",
                "add_snapshot",
                &headers,
                &state,
                Some(&user_id),
                Some("ok"),
            );
            tracing::info!(
                target: "audit",
                action = "sync.snapshot",
                source = "api",
                user_id = %user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                version_id = %version_id,
                body_bytes = body_len,
            );
            if let Err(status) =
                reconcile_after_tc_write(&state, &auth.device, "tc_snapshot_fallback").await
            {
                log_sync_boundary_error(
                    "add_snapshot",
                    &headers,
                    &state,
                    Some(&user_id),
                    "reconcile_failed",
                );
                return status.into_response();
            }
            summary::emit_sync_completed_if_changed(
                &state,
                &user_id,
                audit::request_id(&headers),
                before_sync,
            )
            .await;
            StatusCode::OK.into_response()
        }
        Ok(false) => {
            m::record_sync_op("add_snapshot", elapsed, "error");
            log_sync_boundary_error(
                "add_snapshot",
                &headers,
                &state,
                Some(&user_id),
                "invalid_version",
            );
            StatusCode::BAD_REQUEST.into_response()
        }
        Err(e) => {
            m::record_sync_op("add_snapshot", elapsed, "error");
            log_sync_boundary_error(
                "add_snapshot",
                &headers,
                &state,
                Some(&user_id),
                "storage_error",
            );
            handle_sync_error(&state, &user_id, &e, "add_snapshot", "sync").into_response()
        }
    }
}

/// GET /v1/client/snapshot
///
/// Return the latest snapshot. Returns 404 if no snapshot exists.
pub async fn get_snapshot(
    State(state): State<AppState>,
    // auth via X-Client-Id lookup (no bearer token for TC sync protocol)
    headers: HeaderMap,
) -> Response {
    log_sync_boundary_info(
        "sync.request_received",
        "get_snapshot",
        &headers,
        &state,
        None,
        None,
    );
    let auth = match authenticate_sync_client(&state, &headers).await {
        Ok(a) => a,
        Err(resp) => {
            log_sync_boundary_error("get_snapshot", &headers, &state, None, "auth_failed");
            return resp;
        }
    };
    let user_id = auth.user_id.clone();

    sync_device_if_stale(&state, &auth.device).await;

    let storage = match open_sync_storage(&state, &auth.device) {
        Ok(s) => s,
        Err(status) => {
            log_sync_boundary_error(
                "get_snapshot",
                &headers,
                &state,
                Some(&user_id),
                "sync_storage_unavailable",
            );
            return status.into_response();
        }
    };

    let _in_flight = InFlightGuard::new();
    let start = Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        let guard = storage.lock().unwrap_or_else(|e| e.into_inner());
        guard.get_snapshot()
    })
    .await;
    let elapsed = start.elapsed().as_secs_f64();

    match result {
        Ok(Ok(Some((version_id, snapshot)))) => {
            m::record_sync_op("get_snapshot", elapsed, "ok");
            m::record_sync_body_size("get_snapshot", snapshot.len());
            log_sync_boundary_info(
                "sync.complete",
                "get_snapshot",
                &headers,
                &state,
                Some(&user_id),
                Some("ok"),
            );
            Response::builder()
                .status(StatusCode::OK)
                .header("X-Version-Id", version_id.to_string())
                .header(header::CONTENT_TYPE, SNAPSHOT_CONTENT_TYPE)
                .header(header::CACHE_CONTROL, "no-store")
                .body(axum::body::Body::from(
                    match translate_outbound_canonical_payload(
                        &state,
                        &auth.device,
                        version_id,
                        &snapshot,
                    )
                    .await
                    {
                        Ok(body) => body,
                        Err(status) => {
                            log_sync_boundary_error(
                                "get_snapshot",
                                &headers,
                                &state,
                                Some(&user_id),
                                "payload_translation_failed",
                            );
                            return status.into_response();
                        }
                    },
                ))
                .unwrap()
        }
        Ok(Ok(None)) => {
            m::record_sync_op("get_snapshot", elapsed, "ok");
            log_sync_boundary_info(
                "sync.complete",
                "get_snapshot",
                &headers,
                &state,
                Some(&user_id),
                Some("not_found"),
            );
            StatusCode::NOT_FOUND.into_response()
        }
        Ok(Err(e)) => {
            m::record_sync_op("get_snapshot", elapsed, "error");
            log_sync_boundary_error(
                "get_snapshot",
                &headers,
                &state,
                Some(&user_id),
                "storage_error",
            );
            handle_sync_error(&state, &user_id, &e, "get_snapshot", "sync").into_response()
        }
        Err(e) => {
            m::record_sync_op("get_snapshot", elapsed, "error");
            tracing::error!("get_snapshot task panicked: {e}");
            log_sync_boundary_error(
                "get_snapshot",
                &headers,
                &state,
                Some(&user_id),
                "task_panic",
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_urgency_below_threshold() {
        // Fewer than 100 versions → no urgency
        assert_eq!(snapshot_urgency(0), None);
        assert_eq!(snapshot_urgency(50), None);
        assert_eq!(snapshot_urgency(99), None);
    }

    #[test]
    fn test_snapshot_urgency_low() {
        // 100..499 versions → low urgency
        assert_eq!(snapshot_urgency(100), Some("urgency=low"));
        assert_eq!(snapshot_urgency(250), Some("urgency=low"));
        assert_eq!(snapshot_urgency(499), Some("urgency=low"));
    }

    #[test]
    fn test_snapshot_urgency_high() {
        // 500+ versions → high urgency
        assert_eq!(snapshot_urgency(500), Some("urgency=high"));
        assert_eq!(snapshot_urgency(1000), Some("urgency=high"));
        assert_eq!(snapshot_urgency(u64::MAX), Some("urgency=high"));
    }

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
