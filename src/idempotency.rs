//! Idempotency-Key support per `task-write-contract.md` § Idempotency
//! (cmdock/architecture commit a3f242a).
//!
//! Implements the three-phase write-ahead pattern. Storage lives in
//! `config.sqlite` (see `src/store/sqlite/idempotency.rs`); this module
//! owns the HTTP-side glue:
//!
//! - **Header validation** — `validate_header_value` enforces the
//!   contract's ASCII / 1-64 char rule.
//! - **Body fingerprint** — `body_fingerprint` computes SHA-256 over
//!   the handler-visible request body bytes (after content-encoding
//!   decoding by Caddy or middleware).
//! - **Phase 2 outcome enum** — `Phase2Outcome` distinguishes the three
//!   cases the contract requires for rollback decisions: `Success`,
//!   `KnownNoCommit` (validation/business-rule rejection — roll back the
//!   pending row), `Ambiguous` (commit attempt was made; outcome
//!   unknown — leave pending so lookup-time expiry bounds the residual
//!   window).
//! - **Pipeline wrapper** — `run_idempotent` consumes a closure that
//!   runs Phase 2 and returns a `Phase2Outcome` plus the response body
//!   bytes; this module owns Phase 1 (lookup-or-insert), Phase 3
//!   (finalize / rollback), and the audit emissions for each branch
//!   (`first_execution`, `replay`, `in_flight`, `conflict`, `expired`).
//!
//! The contract's 5-min pending-timeout is configurable. The lookup-time
//! expiry is the load-bearing mechanism that bounds the residual
//! duplicate window deterministically — the background reaper is just
//! operational hygiene.

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use crate::app_state::AppState;
use crate::audit;
use crate::store::models::IdempotencyLookupOutcome;

/// Maximum length of an `Idempotency-Key` header value, per contract
/// § Header. Tightening this would silently break clients; loosening
/// requires a contract amendment.
pub const MAX_KEY_LEN: usize = 64;

/// Result of validating an `Idempotency-Key` header value.
#[derive(Debug)]
pub enum HeaderOutcome<'a> {
    /// Header was absent — caller proceeds with normal at-least-once
    /// semantics, no dedup machinery engaged.
    Absent,
    /// Header was present and valid.
    Present(&'a str),
    /// Header was present but malformed (empty, too long, non-ASCII).
    /// Caller returns `400 INVALID_IDEMPOTENCY_KEY`.
    Invalid,
}

/// Look up the `Idempotency-Key` header and validate per § Header.
///
/// Validation rules (literal contract):
/// - Optional. Absent → `Absent`.
/// - Present → must be 1-64 chars, all ASCII (per spec; rejects
///   bytes > 0x7F including UTF-8 multi-byte sequences).
/// - Empty value (`Idempotency-Key:` with no body, or zero-byte value)
///   → `Invalid`.
pub fn header_outcome(headers: &HeaderMap) -> HeaderOutcome<'_> {
    let Some(raw) = headers.get("idempotency-key") else {
        return HeaderOutcome::Absent;
    };
    let Ok(s) = raw.to_str() else {
        // Non-ASCII / non-visible bytes — to_str() rejects > 0x7F.
        return HeaderOutcome::Invalid;
    };
    if s.is_empty() || s.len() > MAX_KEY_LEN {
        return HeaderOutcome::Invalid;
    }
    // ASCII control chars (< 0x20, 0x7F) are technically ASCII but not
    // useful as idempotency keys — reject for safety. The contract says
    // "ASCII string"; we read that as printable ASCII.
    if s.bytes().any(|b| !(0x20..0x7F).contains(&b)) {
        return HeaderOutcome::Invalid;
    }
    HeaderOutcome::Present(s)
}

/// Compute the SHA-256 fingerprint of the handler-visible request body
/// bytes per § Body-conflict comparison. Bytes are hashed exactly as
/// received — no JSON normalisation, no whitespace stripping. Clients
/// that need structural flexibility MUST canonicalise before submission.
pub fn body_fingerprint(body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(body);
    hasher.finalize().into()
}

/// Outcome of Phase 2 (the actual mutation). The contract requires this
/// distinction explicitly: known-no-commit vs ambiguous-outcome drives
/// whether the pending dedup record is rolled back or left in place.
///
/// Implementation note: do NOT reduce this to a `Result<T, E>` — the
/// `KnownNoCommit` and `Ambiguous` branches both produce error responses
/// to the client but have OPPOSITE rollback behaviour. A naive
/// `Result<_, AppError>` collapses them and would land non-conforming.
#[derive(Debug)]
pub enum Phase2Outcome {
    /// Mutation succeeded. Phase 3 will finalize the dedup record with
    /// the response payload, then the response is returned to the client.
    Success {
        /// Final HTTP status to return.
        status: StatusCode,
        /// Response body bytes. These are stored in the dedup record and
        /// replayed verbatim on subsequent retries within the retention
        /// window.
        response_body: Vec<u8>,
        /// Optional `Content-Type` header to store + replay.
        content_type: Option<String>,
    },
    /// Mutation **definitely did not commit** — validation rejection,
    /// business-rule error, or any error raised before the underlying
    /// store's commit was called. Phase 3 rolls back the pending row so
    /// a subsequent retry runs fresh (does not get permanently blocked
    /// by a transient validation failure).
    KnownNoCommit {
        /// Final HTTP status to return.
        status: StatusCode,
        /// Response body bytes (the error payload). Returned to the
        /// client; NOT stored in the dedup record (since the row is
        /// rolled back).
        response_body: Vec<u8>,
        /// Optional `Content-Type` header for the error response.
        content_type: Option<String>,
    },
    /// Mutation outcome **is unknown** — commit attempt was made; the
    /// reply may have been lost mid-commit, the process may have been
    /// killed in the commit window, or the underlying library returned
    /// an error that does not distinguish "did not commit" from
    /// "committed but reply lost".
    ///
    /// Phase 3 **does not roll back the pending row**. Subsequent
    /// retries within the pending-timeout return `503 IDEMPOTENCY_IN_FLIGHT`;
    /// retries after the timeout run fresh (with the residual narrow
    /// window of duplicate-creation if the original did commit). This
    /// is intentional — rolling back here would let an immediate retry
    /// duplicate a successful commit.
    Ambiguous {
        /// Final HTTP status to return (typically 5xx).
        status: StatusCode,
        /// Response body bytes for the client.
        response_body: Vec<u8>,
        /// Optional `Content-Type` for the error response.
        content_type: Option<String>,
    },
}

/// Construct the `503 IDEMPOTENCY_IN_FLIGHT` response per the contract.
/// `Retry-After` value comes from `task_write.idempotency_retry_after_seconds`.
pub fn in_flight_response(retry_after_seconds: u32) -> Response {
    let mut resp = (StatusCode::SERVICE_UNAVAILABLE, "IDEMPOTENCY_IN_FLIGHT").into_response();
    if let Ok(value) = HeaderValue::from_str(&retry_after_seconds.to_string()) {
        resp.headers_mut().insert("retry-after", value);
    }
    resp
}

/// Replay a stored `completed` response per the contract. Status, body
/// bytes, and stored `Content-Type` / `Content-Length` are returned
/// verbatim. Per-attempt headers (`X-Request-ID`, `Date`, tracing) are
/// regenerated by the outer middleware and are not our concern here.
pub fn replay_response(
    status_code: u16,
    body: Vec<u8>,
    content_type: Option<String>,
    content_length: Option<i64>,
) -> Response {
    let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);
    let mut resp = (status, body).into_response();
    if let Some(ct) = content_type {
        if let Ok(v) = HeaderValue::from_str(&ct) {
            resp.headers_mut().insert("content-type", v);
        }
    }
    // Replay stored Content-Length verbatim per the contract. Hyper
    // would normally regenerate this from the body length, but the spec
    // says stored headers are replayed; surfacing the original is
    // closer to "byte-identical replay" semantics.
    if let Some(cl) = content_length {
        if let Ok(v) = HeaderValue::from_str(&cl.to_string()) {
            resp.headers_mut().insert("content-length", v);
        }
    }
    resp
}

/// Wall clock for Phase 1's `now`. Centralised so tests can swap to a
/// deterministic clock if needed (for now the integration tests use
/// short retention/timeout windows + sleeps).
pub fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Background pruner. Runs periodically (every `IDEMPOTENCY_PRUNER_INTERVAL`)
/// and prunes BOTH `completed` rows past their retention window AND
/// `pending` rows past their stranded-pending timeout.
///
/// Operational hygiene only — the lookup-time expiry mechanism in
/// `lookup_or_insert_idempotency_pending` is the load-bearing
/// correctness primitive. A delayed or temporarily-failing reaper
/// does not extend the residual duplicate window. Codex iteration 1
/// flagged this distinction explicitly.
pub fn start(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        // First tick after IDEMPOTENCY_PRUNER_INTERVAL to avoid running
        // immediately at startup (let the system warm up first).
        let interval_dur = std::time::Duration::from_secs(60 * 5); // 5 minutes
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + interval_dur, interval_dur);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            prune_once(&state).await;
        }
    });
}

/// Run one round of pruning. Exposed (rather than inlined into `start`)
/// so tests can drive it deterministically.
pub async fn prune_once(state: &AppState) {
    let now = now_unix_seconds();
    let cfg = &state.config.task_write;

    match state
        .store
        .prune_idempotency_completed(cfg.idempotency_retention_hours, now)
        .await
    {
        Ok(deleted) if deleted > 0 => {
            tracing::debug!(deleted, "Pruned completed idempotency records");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "Failed to prune completed idempotency records");
        }
    }

    match state
        .store
        .prune_idempotency_stranded_pending(cfg.idempotency_pending_timeout_seconds, now)
        .await
    {
        Ok(deleted) if deleted > 0 => {
            tracing::info!(
                target: "audit",
                action = "task.write.idempotent.stranded_reaped",
                source = "system",
                deleted,
            );
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "Failed to prune stranded pending idempotency records");
        }
    }

    // Task-key allocation reaper (server#130). Shares the existing reaper
    // tick — runs at the same 5-minute cadence, no extra background task
    // needed.
    let _ = crate::task_keys::run_reaper_pass(state).await;
}

/// Run the dedup state machine and dispatch to the appropriate response
/// branch. The caller supplies a closure that performs Phase 2 (the
/// actual mutation) and returns a `Phase2Outcome`.
///
/// Audit events are emitted at each branch per the contract guidance:
/// - `task.write.idempotent.first_execution` on Phase 1 fresh insert
/// - `task.write.idempotent.replay` on completed-row hit with matching fp
/// - `task.write.idempotent.conflict` on fingerprint mismatch (409)
/// - `task.write.idempotent.in_flight` on pending-row hit with matching fp (503)
/// - `task.write.idempotent.expired_replaced` when lookup-time expiry fires
///
/// The `expired` audit is fired implicitly via fresh-execution branch
/// when an existing-but-expired row was replaced.
pub async fn run_idempotent<F, Fut>(
    state: &AppState,
    user_id: &str,
    request_path: &str,
    headers: &HeaderMap,
    idempotency_key: &str,
    body_bytes: &[u8],
    phase2: F,
) -> Response
where
    F: FnOnce(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Phase2Outcome>,
{
    let fingerprint = body_fingerprint(body_bytes);
    let cfg = &state.config.task_write;
    let now = now_unix_seconds();
    let client_ip = audit::client_ip(headers, state.config.server.trust_forwarded_headers);
    let request_id = audit::request_id(headers);

    let outcome = match state
        .store
        .lookup_or_insert_idempotency_pending(
            user_id,
            request_path,
            idempotency_key,
            &fingerprint,
            cfg.idempotency_pending_timeout_seconds,
            cfg.idempotency_retention_hours,
            now,
        )
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(error = %e, "Phase 1 lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let metric_op = if request_path.ends_with("/modify") {
        "modify_task"
    } else {
        "add_task"
    };

    match outcome {
        IdempotencyLookupOutcome::Replay {
            status_code,
            response_body,
            content_type,
            content_length,
        } => {
            tracing::info!(
                target: "audit",
                action = "task.write.idempotent.replay",
                source = "api",
                user_id = %user_id,
                client_ip = %client_ip,
                request_id = ?request_id,
                request_path = %request_path,
                idempotency_key = %idempotency_key,
            );
            crate::metrics::record_idempotency_outcome(metric_op, "replay");
            replay_response(status_code, response_body, content_type, content_length)
        }
        IdempotencyLookupOutcome::Conflict => {
            tracing::warn!(
                target: "audit",
                action = "task.write.idempotent.conflict",
                source = "api",
                user_id = %user_id,
                client_ip = %client_ip,
                request_id = ?request_id,
                request_path = %request_path,
                idempotency_key = %idempotency_key,
            );
            crate::metrics::record_idempotency_outcome(metric_op, "conflict");
            (StatusCode::CONFLICT, "IDEMPOTENCY_KEY_CONFLICT").into_response()
        }
        IdempotencyLookupOutcome::InFlight => {
            tracing::info!(
                target: "audit",
                action = "task.write.idempotent.in_flight",
                source = "api",
                user_id = %user_id,
                client_ip = %client_ip,
                request_id = ?request_id,
                request_path = %request_path,
                idempotency_key = %idempotency_key,
            );
            crate::metrics::record_idempotency_outcome(metric_op, "in_flight");
            in_flight_response(cfg.idempotency_retry_after_seconds)
        }
        IdempotencyLookupOutcome::FreshExecution { attempt_id } => {
            tracing::info!(
                target: "audit",
                action = "task.write.idempotent.first_execution",
                source = "api",
                user_id = %user_id,
                client_ip = %client_ip,
                request_id = ?request_id,
                request_path = %request_path,
                idempotency_key = %idempotency_key,
                attempt_id = %attempt_id,
            );

            crate::metrics::record_idempotency_outcome(metric_op, "first_execution");
            let phase2_start = std::time::Instant::now();
            let phase2_result = phase2(body_bytes.to_vec()).await;
            crate::metrics::record_idempotency_phase2(
                metric_op,
                phase2_start.elapsed().as_secs_f64(),
            );

            match phase2_result {
                Phase2Outcome::Success {
                    status,
                    response_body,
                    content_type,
                } => {
                    match state
                        .store
                        .finalize_idempotency_completed(
                            user_id,
                            request_path,
                            idempotency_key,
                            &attempt_id,
                            status.as_u16(),
                            &response_body,
                            content_type.as_deref(),
                        )
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            // Stale-finalizer discard: the row was already
                            // replaced by a fresh retry's pending row. The
                            // attempt-id guard correctly prevents data
                            // corruption; the discard is intentional but
                            // operationally useful to surface for diagnostics.
                            tracing::info!(
                                user_id = %user_id,
                                attempt_id = %attempt_id,
                                request_path = %request_path,
                                "Phase 3 finalize discarded — superseded by fresh retry"
                            );
                        }
                        Err(e) => {
                            // Phase 3 failure: mutation persisted but the
                            // dedup record is stuck in `pending`. Lookup-time
                            // expiry will treat it as expired after
                            // pending-timeout. Until then, retries get 503.
                            // This is the residual narrow window per the
                            // contract's § Exactly-once-per-key SLA.
                            tracing::error!(
                                error = %e,
                                user_id = %user_id,
                                attempt_id = %attempt_id,
                                "Phase 3 finalize failed — dedup record stuck pending"
                            );
                        }
                    }
                    let mut resp = (status, response_body).into_response();
                    if let Some(ct) = content_type {
                        if let Ok(v) = HeaderValue::from_str(&ct) {
                            resp.headers_mut().insert("content-type", v);
                        }
                    }
                    resp
                }
                Phase2Outcome::KnownNoCommit {
                    status,
                    response_body,
                    content_type,
                } => {
                    let _ = state
                        .store
                        .rollback_idempotency_pending(
                            user_id,
                            request_path,
                            idempotency_key,
                            &attempt_id,
                        )
                        .await
                        .map_err(|e| {
                            tracing::warn!(
                                error = %e,
                                user_id = %user_id,
                                attempt_id = %attempt_id,
                                "Phase 1 rollback failed — pending row may linger"
                            );
                        });
                    let mut resp = (status, response_body).into_response();
                    if let Some(ct) = content_type {
                        if let Ok(v) = HeaderValue::from_str(&ct) {
                            resp.headers_mut().insert("content-type", v);
                        }
                    }
                    resp
                }
                Phase2Outcome::Ambiguous {
                    status,
                    response_body,
                    content_type,
                } => {
                    // Pending row left in place per § Failure handling —
                    // lookup-time expiry will free it after the timeout.
                    let mut resp = (status, response_body).into_response();
                    if let Some(ct) = content_type {
                        if let Ok(v) = HeaderValue::from_str(&ct) {
                            resp.headers_mut().insert("content-type", v);
                        }
                    }
                    resp
                }
            }
        }
    }
}
