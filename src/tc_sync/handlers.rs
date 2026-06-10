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
use crate::merged_sync_gateway::inbound::{GatewayAddVersionOutcome, GatewayVersion};
use crate::merged_sync_gateway::protocol::{self, GatewayChildVersionOutcome};
use crate::metrics as m;

use super::auth::authenticate_sync_client;
use super::edge::{
    log_sync_boundary_error, log_sync_boundary_info, require_content_type,
    HISTORY_SEGMENT_CONTENT_TYPE, SNAPSHOT_CONTENT_TYPE,
};
use super::events;
use super::gateway_thread::{run_gateway_add_personal_version, run_personal_projection};
use super::payloads::{
    ensure_device_bridge_ready, translate_inbound_device_payload_plaintext,
    translate_outbound_plaintext_payload,
};
use super::runtime::{handle_sync_error, InFlightGuard};

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
    if let Err(status) =
        crate::user_runtime::block_quarantined_user(&state, &user_id, "merged_sync")
    {
        log_sync_boundary_error(
            "add_version",
            &headers,
            &state,
            Some(&user_id),
            "user_quarantined",
        );
        return status.into_response();
    }
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
    let body = match translate_inbound_device_payload_plaintext(
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

    let body_len = body.len();
    m::record_sync_body_size("add_version", body_len);
    let before_sync = events::capture_before_sync(&state, &user_id).await;

    let _in_flight = InFlightGuard::new();
    let start = Instant::now();
    let result = run_gateway_add_personal_version(
        state.clone(),
        GatewayVersion {
            user_id: user_id.clone(),
            client_id: auth.device.client_id.clone(),
            parent_version_id,
            history_segment: body.to_vec(),
            request_id: audit::request_id(&headers),
        },
    )
    .await;
    let elapsed = start.elapsed().as_secs_f64();

    add_version_gateway_response(
        AddVersionResponseContext {
            state: &state,
            headers: &headers,
            user_id: &user_id,
            client_id: &auth.device.client_id,
            body_len,
            elapsed,
            before_sync,
        },
        result,
    )
    .await
}

struct AddVersionResponseContext<'a> {
    state: &'a AppState,
    headers: &'a HeaderMap,
    user_id: &'a str,
    client_id: &'a str,
    body_len: usize,
    elapsed: f64,
    before_sync: Option<events::SyncSnapshot>,
}

async fn add_version_gateway_response(
    ctx: AddVersionResponseContext<'_>,
    result: anyhow::Result<GatewayAddVersionOutcome>,
) -> Response {
    match result {
        Ok(GatewayAddVersionOutcome::Accepted { version_id, .. }) => {
            add_version_accepted_response(ctx, version_id).await
        }
        Ok(GatewayAddVersionOutcome::ExpectedParentVersion {
            expected_parent_version_id,
            ..
        }) => add_version_conflict_response(ctx, expected_parent_version_id),
        Ok(GatewayAddVersionOutcome::Rejected { code, .. }) => {
            m::record_sync_op("add_version", ctx.elapsed, "error");
            log_sync_boundary_error(
                "add_version",
                ctx.headers,
                ctx.state,
                Some(ctx.user_id),
                code,
            );
            StatusCode::BAD_REQUEST.into_response()
        }
        Err(e) => {
            m::record_sync_op("add_version", ctx.elapsed, "error");
            log_sync_boundary_error(
                "add_version",
                ctx.headers,
                ctx.state,
                Some(ctx.user_id),
                "gateway_error",
            );
            handle_sync_error(ctx.state, ctx.user_id, &e, "add_version", "merged_sync")
                .into_response()
        }
    }
}

async fn add_version_accepted_response(
    ctx: AddVersionResponseContext<'_>,
    version_id: Uuid,
) -> Response {
    let urgency_count = match protocol::versions_since_snapshot(ctx.state, ctx.user_id).await {
        Ok(count) => count,
        Err(err) => {
            m::record_sync_op("add_version", ctx.elapsed, "error");
            log_sync_boundary_error(
                "add_version",
                ctx.headers,
                ctx.state,
                Some(ctx.user_id),
                "snapshot_urgency_failed",
            );
            return handle_sync_error(ctx.state, ctx.user_id, &err, "add_version", "merged_sync")
                .into_response();
        }
    };
    ctx.state
        .runtime_sync
        .mark_canonical_changed_and_device_synced(ctx.user_id, ctx.client_id);
    m::record_sync_op("add_version", ctx.elapsed, "ok");
    log_sync_boundary_info(
        "sync.complete",
        "add_version",
        ctx.headers,
        ctx.state,
        Some(ctx.user_id),
        Some("ok"),
    );
    tracing::info!(
        target: "audit",
        action = "sync.add_version",
        source = "api",
        user_id = %ctx.user_id,
        client_ip = %audit::client_ip(ctx.headers, ctx.state.config.server.trust_forwarded_headers),
        version_id = %version_id,
        body_bytes = ctx.body_len,
        sync_runtime = "merged_gateway",
    );
    events::emit_sync_completed_if_changed(
        ctx.state,
        ctx.user_id,
        audit::request_id(ctx.headers),
        ctx.before_sync,
    )
    .await;
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("X-Version-Id", version_id.to_string())
        .header(header::CONTENT_TYPE, HISTORY_SEGMENT_CONTENT_TYPE);
    let urgency = protocol::snapshot_urgency_header(urgency_count);
    let urgency_level = urgency.map(|value| {
        if value.contains("high") {
            "high"
        } else {
            "low"
        }
    });
    m::record_merged_gateway_snapshot_age(urgency_count, urgency_level.unwrap_or("none"));
    if let Some(urgency) = urgency {
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

fn add_version_conflict_response(
    ctx: AddVersionResponseContext<'_>,
    expected_parent_version_id: Uuid,
) -> Response {
    m::record_sync_op("add_version", ctx.elapsed, "conflict");
    m::record_sync_conflict();
    log_sync_boundary_error(
        "add_version",
        ctx.headers,
        ctx.state,
        Some(ctx.user_id),
        "conflict",
    );
    Response::builder()
        .status(StatusCode::CONFLICT)
        .header(
            "X-Parent-Version-Id",
            expected_parent_version_id.to_string(),
        )
        .body(axum::body::Body::empty())
        .unwrap()
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
    if let Err(status) =
        crate::user_runtime::block_quarantined_user(&state, &user_id, "merged_sync")
    {
        log_sync_boundary_error(
            "get_child_version",
            &headers,
            &state,
            Some(&user_id),
            "user_quarantined",
        );
        return status.into_response();
    }
    if let Err(status) = ensure_device_bridge_ready(&state, &auth.device) {
        log_sync_boundary_error(
            "get_child_version",
            &headers,
            &state,
            Some(&user_id),
            "device_bridge_not_ready",
        );
        return status.into_response();
    }

    if state
        .runtime_sync
        .device_needs_sync(&user_id, &auth.device.client_id)
    {
        if let Err(err) = run_personal_projection(state.clone(), user_id.clone()).await {
            tracing::warn!(user_id = %user_id, error = %err, "merged personal projection before get-child failed");
        } else {
            state
                .runtime_sync
                .mark_device_synced_to_current(&user_id, &auth.device.client_id);
        }
    }

    let _in_flight = InFlightGuard::new();
    let start = Instant::now();
    let result = protocol::get_child_version(&state, &user_id, parent_version_id).await;
    let elapsed = start.elapsed().as_secs_f64();

    get_child_version_gateway_response(
        GetChildVersionResponseContext {
            state: &state,
            headers: &headers,
            user_id: &user_id,
            device: &auth.device,
            elapsed,
        },
        result,
    )
    .await
}

struct GetChildVersionResponseContext<'a> {
    state: &'a AppState,
    headers: &'a HeaderMap,
    user_id: &'a str,
    device: &'a crate::store::models::DeviceRecord,
    elapsed: f64,
}

async fn get_child_version_gateway_response(
    ctx: GetChildVersionResponseContext<'_>,
    result: anyhow::Result<GatewayChildVersionOutcome>,
) -> Response {
    match result {
        Ok(GatewayChildVersionOutcome::Version {
            version_id,
            parent_version_id,
            history_segment,
        }) => {
            get_child_version_found_response(ctx, version_id, parent_version_id, history_segment)
                .await
        }
        Ok(GatewayChildVersionOutcome::NotFound) => {
            get_child_version_empty_response(ctx, StatusCode::NOT_FOUND, "not_found")
        }
        Ok(GatewayChildVersionOutcome::Gone) => {
            get_child_version_empty_response(ctx, StatusCode::GONE, "gone")
        }
        Err(e) => {
            m::record_sync_op("get_child_version", ctx.elapsed, "error");
            log_sync_boundary_error(
                "get_child_version",
                ctx.headers,
                ctx.state,
                Some(ctx.user_id),
                "gateway_error",
            );
            handle_sync_error(
                ctx.state,
                ctx.user_id,
                &e,
                "get_child_version",
                "merged_sync",
            )
            .into_response()
        }
    }
}

async fn get_child_version_found_response(
    ctx: GetChildVersionResponseContext<'_>,
    version_id: Uuid,
    parent_version_id: Uuid,
    history_segment: Vec<u8>,
) -> Response {
    m::record_sync_op("get_child_version", ctx.elapsed, "ok");
    m::record_sync_body_size("get_child_version", history_segment.len());
    log_sync_boundary_info(
        "sync.complete",
        "get_child_version",
        ctx.headers,
        ctx.state,
        Some(ctx.user_id),
        Some("ok"),
    );
    let body = match translate_outbound_plaintext_payload(
        ctx.state,
        ctx.device,
        parent_version_id,
        &history_segment,
    )
    .await
    {
        Ok(body) => body,
        Err(status) => {
            log_sync_boundary_error(
                "get_child_version",
                ctx.headers,
                ctx.state,
                Some(ctx.user_id),
                "payload_translation_failed",
            );
            return status.into_response();
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("X-Version-Id", version_id.to_string())
        .header("X-Parent-Version-Id", parent_version_id.to_string())
        .header(header::CONTENT_TYPE, HISTORY_SEGMENT_CONTENT_TYPE)
        .header(header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from(body))
        .unwrap()
}

fn get_child_version_empty_response(
    ctx: GetChildVersionResponseContext<'_>,
    status: StatusCode,
    outcome: &'static str,
) -> Response {
    m::record_sync_op("get_child_version", ctx.elapsed, "ok");
    log_sync_boundary_info(
        "sync.complete",
        "get_child_version",
        ctx.headers,
        ctx.state,
        Some(ctx.user_id),
        Some(outcome),
    );
    status.into_response()
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
    if let Err(status) =
        crate::user_runtime::block_quarantined_user(&state, &user_id, "merged_sync")
    {
        log_sync_boundary_error(
            "add_snapshot",
            &headers,
            &state,
            Some(&user_id),
            "user_quarantined",
        );
        return status.into_response();
    }
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
    let body = match translate_inbound_device_payload_plaintext(
        &state,
        &auth.device,
        version_id,
        body.as_ref(),
    )
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

    let _in_flight = InFlightGuard::new();
    let start = Instant::now();
    let result = protocol::add_snapshot(&state, &user_id, version_id, body.to_vec()).await;
    let elapsed = start.elapsed().as_secs_f64();

    add_snapshot_response(
        AddSnapshotResponseContext {
            state: &state,
            headers: &headers,
            user_id: &user_id,
            version_id,
            body_len,
            elapsed,
        },
        result,
    )
}

struct AddSnapshotResponseContext<'a> {
    state: &'a AppState,
    headers: &'a HeaderMap,
    user_id: &'a str,
    version_id: Uuid,
    body_len: usize,
    elapsed: f64,
}

fn add_snapshot_response(
    ctx: AddSnapshotResponseContext<'_>,
    result: anyhow::Result<bool>,
) -> Response {
    match result {
        Ok(true) => add_snapshot_accepted_response(ctx),
        Ok(false) => {
            m::record_sync_op("add_snapshot", ctx.elapsed, "error");
            log_sync_boundary_error(
                "add_snapshot",
                ctx.headers,
                ctx.state,
                Some(ctx.user_id),
                "invalid_version",
            );
            StatusCode::BAD_REQUEST.into_response()
        }
        Err(e) => {
            m::record_sync_op("add_snapshot", ctx.elapsed, "error");
            log_sync_boundary_error(
                "add_snapshot",
                ctx.headers,
                ctx.state,
                Some(ctx.user_id),
                "gateway_error",
            );
            handle_sync_error(ctx.state, ctx.user_id, &e, "add_snapshot", "merged_sync")
                .into_response()
        }
    }
}

fn add_snapshot_accepted_response(ctx: AddSnapshotResponseContext<'_>) -> Response {
    m::record_sync_op("add_snapshot", ctx.elapsed, "ok");
    log_sync_boundary_info(
        "sync.complete",
        "add_snapshot",
        ctx.headers,
        ctx.state,
        Some(ctx.user_id),
        Some("ok"),
    );
    tracing::info!(
        target: "audit",
        action = "sync.snapshot",
        source = "api",
        user_id = %ctx.user_id,
        client_ip = %audit::client_ip(ctx.headers, ctx.state.config.server.trust_forwarded_headers),
        version_id = %ctx.version_id,
        body_bytes = ctx.body_len,
        sync_runtime = "merged_gateway",
    );
    StatusCode::OK.into_response()
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
    if let Err(status) =
        crate::user_runtime::block_quarantined_user(&state, &user_id, "merged_sync")
    {
        log_sync_boundary_error(
            "get_snapshot",
            &headers,
            &state,
            Some(&user_id),
            "user_quarantined",
        );
        return status.into_response();
    }
    if let Err(status) = ensure_device_bridge_ready(&state, &auth.device) {
        log_sync_boundary_error(
            "get_snapshot",
            &headers,
            &state,
            Some(&user_id),
            "device_bridge_not_ready",
        );
        return status.into_response();
    }

    if state
        .runtime_sync
        .device_needs_sync(&user_id, &auth.device.client_id)
    {
        if let Err(err) = run_personal_projection(state.clone(), user_id.clone()).await {
            tracing::warn!(user_id = %user_id, error = %err, "merged personal projection before snapshot failed");
        } else {
            state
                .runtime_sync
                .mark_device_synced_to_current(&user_id, &auth.device.client_id);
        }
    }

    let _in_flight = InFlightGuard::new();
    let start = Instant::now();
    let result = protocol::get_snapshot(&state, &user_id).await;
    let elapsed = start.elapsed().as_secs_f64();

    get_snapshot_response(
        GetSnapshotResponseContext {
            state: &state,
            headers: &headers,
            user_id: &user_id,
            device: &auth.device,
            elapsed,
        },
        result,
    )
    .await
}

struct GetSnapshotResponseContext<'a> {
    state: &'a AppState,
    headers: &'a HeaderMap,
    user_id: &'a str,
    device: &'a crate::store::models::DeviceRecord,
    elapsed: f64,
}

async fn get_snapshot_response(
    ctx: GetSnapshotResponseContext<'_>,
    result: anyhow::Result<Option<(Uuid, Vec<u8>)>>,
) -> Response {
    match result {
        Ok(Some((version_id, snapshot))) => {
            get_snapshot_found_response(ctx, version_id, snapshot).await
        }
        Ok(None) => {
            m::record_sync_op("get_snapshot", ctx.elapsed, "ok");
            log_sync_boundary_info(
                "sync.complete",
                "get_snapshot",
                ctx.headers,
                ctx.state,
                Some(ctx.user_id),
                Some("not_found"),
            );
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            m::record_sync_op("get_snapshot", ctx.elapsed, "error");
            log_sync_boundary_error(
                "get_snapshot",
                ctx.headers,
                ctx.state,
                Some(ctx.user_id),
                "gateway_error",
            );
            handle_sync_error(ctx.state, ctx.user_id, &e, "get_snapshot", "merged_sync")
                .into_response()
        }
    }
}

async fn get_snapshot_found_response(
    ctx: GetSnapshotResponseContext<'_>,
    version_id: Uuid,
    snapshot: Vec<u8>,
) -> Response {
    m::record_sync_op("get_snapshot", ctx.elapsed, "ok");
    m::record_sync_body_size("get_snapshot", snapshot.len());
    log_sync_boundary_info(
        "sync.complete",
        "get_snapshot",
        ctx.headers,
        ctx.state,
        Some(ctx.user_id),
        Some("ok"),
    );
    let body =
        match translate_outbound_plaintext_payload(ctx.state, ctx.device, version_id, &snapshot)
            .await
        {
            Ok(body) => body,
            Err(status) => {
                log_sync_boundary_error(
                    "get_snapshot",
                    ctx.headers,
                    ctx.state,
                    Some(ctx.user_id),
                    "payload_translation_failed",
                );
                return status.into_response();
            }
        };
    Response::builder()
        .status(StatusCode::OK)
        .header("X-Version-Id", version_id.to_string())
        .header(header::CONTENT_TYPE, SNAPSHOT_CONTENT_TYPE)
        .header(header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from(body))
        .unwrap()
}
