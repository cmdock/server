use axum::body::Bytes;
use axum::{
    extract::{rejection::JsonRejection, Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use taskchampion::Status;
use uuid::Uuid;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use super::models::{
    AddTaskRequest, ModifyTaskRequest, TaskActionResponse, TaskBatchLookupResponse, TaskItem,
    TaskListResponse,
};
use super::mutations::{self, TaskMutationKind};
use super::service;
use crate::app_state::AppState;
use crate::auth::AuthUser;
use crate::metrics as m;
use crate::replica;
use crate::user_runtime::{handle_replica_error, open_user_replica};
use metrics::counter as metric_counter;

#[derive(Deserialize, utoipa::IntoParams)]
pub struct TaskListQuery {
    /// View ID to filter tasks by (looks up filter expression from views table).
    /// Mutually exclusive with `uuids`.
    pub view: Option<String>,
    /// Comma-separated list of canonical UUID strings for batched UUID
    /// lookup per `task-read-contract.md`. Mutually exclusive with `view`.
    /// Min 1, max 100 raw entries before deduplication.
    pub uuids: Option<String>,
}

/// Map an Axum JSON-extractor rejection to a `task-write-contract.md`
/// error code body where appropriate.
///
/// Only deserialisation failures (`JsonDataError`, `JsonSyntaxError`) map
/// to a contract code body. Detection uses substring match on serde's
/// rendered error message — brittle if serde error wording ever changes,
/// but the alternative (downcasting to the inner serde::de::Error) is
/// also fragile across axum versions. The substrings we match on are
/// stable in serde 1.0 today.
///
/// `kind = AddTask` enables the `INVALID_RAW` mapping for missing/empty/
/// control-char `raw` field on the create endpoint; `Modify` doesn't have
/// a required-field analog, so this distinction matters only on add.
/// Other rejection variants (body-size limit → 413, missing Content-Type
/// → 415) pass through their original status with empty body.
fn json_rejection_to_response(rejection: JsonRejection, kind: JsonRejectionKind) -> Response {
    match rejection {
        JsonRejection::JsonDataError(_) | JsonRejection::JsonSyntaxError(_) => {
            let body_text = rejection.body_text();
            let code = if body_text.contains("unknown field") {
                "INVALID_FIELD"
            } else if matches!(kind, JsonRejectionKind::AddTask)
                && body_text.contains("missing field `raw`")
            {
                "INVALID_RAW"
            } else {
                "INVALID_BODY"
            };
            (StatusCode::BAD_REQUEST, code).into_response()
        }
        // BytesRejection (body too large), MissingJsonContentType, etc.
        // keep their original status. Body-size 1 MiB → 413; clients have
        // existing tests that depend on this.
        other => other.into_response(),
    }
}

/// Map a `serde_json::Error` from manual `from_slice` to the same
/// contract codes as `json_rejection_to_response`. Used on the
/// idempotency-aware code path that takes raw `Bytes` and parses
/// manually so the body bytes can be hashed first.
///
/// Substring match on `serde_json`'s rendered message — same brittleness
/// caveat as the rejection mapper, same justification (the substrings
/// are stable in serde 1.0).
fn serde_error_to_response(err: &serde_json::Error, kind: JsonRejectionKind) -> Response {
    let msg = err.to_string();
    let code = if msg.contains("unknown field") {
        "INVALID_FIELD"
    } else if matches!(kind, JsonRejectionKind::AddTask) && msg.contains("missing field `raw`") {
        "INVALID_RAW"
    } else {
        "INVALID_BODY"
    };
    (StatusCode::BAD_REQUEST, code).into_response()
}

/// Serialise a `Response` into a `(status, body_bytes, content_type)` tuple
/// suitable for storing in the idempotency dedup record. Used for replay.
///
/// For our success path the body is always JSON via `Json(...).into_response()`,
/// which uses `application/json` — we hardcode the content type rather than
/// reading it back from the response (consuming a Body asynchronously inside a
/// sync helper is awkward).
fn capture_json_response_bytes<T: serde::Serialize>(body: &T) -> (Vec<u8>, Option<String>) {
    let bytes = serde_json::to_vec(body).expect("serialising owned struct cannot fail");
    (bytes, Some("application/json".to_string()))
}

#[derive(Clone, Copy)]
enum JsonRejectionKind {
    AddTask,
    Modify,
}

/// Validate a string is in canonical Taskwarrior date format
/// (`YYYYMMDDTHHmmssZ`). Returns true if the string parses cleanly via
/// `replica::parse_tw_date` (which uses chrono's strict format parser
/// — out-of-range months / hours etc. are rejected).
fn is_canonical_tw_date(s: &str) -> bool {
    replica::parse_tw_date(s).is_some()
}

/// Validate a string is a canonical UUID per `task-read-contract.md` §
/// Path parameter: lowercase hex, RFC 4122 hyphenated textual representation
/// `[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}` and
/// version-agnostic. Wraps `Uuid::parse_str` (which is more permissive —
/// accepts simple/no-hyphen, braced, URN-prefixed, and uppercase forms)
/// with a strict format check first.
///
/// Returns `Ok(parsed)` only when the string matches the canonical form
/// exactly. The parsed value is returned for callers that need the binary
/// representation (e.g. HashMap lookup against `all_tasks()` output).
///
/// **Hot-path safety**: shape check is a single pass over the bytes (no
/// regex allocation), then defers to `Uuid::parse_str` for the
/// well-formedness check. Allocates nothing on the success path.
fn parse_canonical_uuid(s: &str) -> Result<Uuid, ()> {
    // Canonical form is exactly 36 chars: 8-4-4-4-12 hex with hyphens
    // at positions 8, 13, 18, 23.
    if s.len() != 36 {
        return Err(());
    }
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if b != b'-' {
                    return Err(());
                }
            }
            _ => {
                // Lowercase hex only — uppercase rejected per § Path parameter.
                if !matches!(b, b'0'..=b'9' | b'a'..=b'f') {
                    return Err(());
                }
            }
        }
    }
    // Format check passed; defer to uuid crate for the well-formedness check
    // (which is now redundant on the format axis but cheap and gives us the
    // parsed Uuid for downstream HashMap lookup).
    Uuid::parse_str(s).map_err(|_| ())
}

/// Parse a task-key path-param of the form `<PREFIX>-N` per the task-keys
/// contract. Prefix is `^[A-Za-z][A-Za-z0-9]{0,9}$` (case-insensitive on
/// input — folded to uppercase before lookup); N is `^[1-9][0-9]*$` (no
/// leading zero — `WORK-01` is malformed). On success returns the
/// uppercased prefix and parsed N.
///
/// Returns `None` for any input that is not a syntactically valid key
/// (including UUIDs — callers MUST attempt UUID parse first).
fn parse_task_key(s: &str) -> Option<(String, i64)> {
    let dash = s.find('-')?;
    let (prefix, rest) = s.split_at(dash);
    let n_str = &rest[1..];
    // Prefix: 1..=10 chars, first ASCII alpha, rest ASCII alphanumeric.
    if prefix.is_empty() || prefix.len() > 10 {
        return None;
    }
    let mut chars = prefix.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    // N: non-empty, no leading zero, decimal digits only.
    if n_str.is_empty() {
        return None;
    }
    let mut n_chars = n_str.chars();
    let n_first = n_chars.next()?;
    if !('1'..='9').contains(&n_first) {
        return None;
    }
    if !n_chars.all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: i64 = n_str.parse().ok()?;
    Some((prefix.to_ascii_uppercase(), n))
}

/// Outcome of resolving a task-path parameter to a UUID. Distinct enough
/// from `Result<Uuid, _>` that callers can fire the right metric outcome
/// label (`hit` / `miss` / `malformed`) and the right HTTP response shape
/// (200/404 empty / 400 INVALID_UUID) without re-checking the input form.
enum PathResolveOutcome {
    /// Input parsed (UUID or key) AND resolved to a UUID present in the
    /// allocation table. Caller should proceed with the task lookup.
    Resolved { form: &'static str, uuid: Uuid },
    /// Input parsed as a key but no committed allocation row exists for
    /// (user_id, prefix, n). Per the existence-leak rule, caller MUST
    /// return 404 with empty body — indistinguishable from cross-account.
    KeyMiss,
    /// Input parsed as neither a UUID (under the chosen strictness) nor a
    /// key. Caller returns 400 INVALID_UUID (plain text body for canonical
    /// path; empty body for permissive path to preserve existing wire
    /// shape — see resolver fn docs).
    Malformed,
    /// Storage layer returned an error during key lookup. Caller propagates
    /// 500 INTERNAL_SERVER_ERROR. Distinct from `KeyMiss` so we don't fold
    /// transient DB errors into a 404.
    StoreError,
}

/// Resolve a task path parameter under the **canonical-UUID** strictness
/// rule used by `GET /api/tasks/{uuid}` per `task-read-contract.md` §
/// Path parameter. Tries `parse_canonical_uuid` first (lowercase-hyphenated
/// only — no uppercase / simple / braced / URN forms). On miss, tries the
/// task-key form `<PREFIX>-N` and resolves it via the allocation table.
///
/// Fires `task_keys_resolution_total{form, outcome}` exactly once per call.
async fn resolve_task_path_param_canonical(
    state: &AppState,
    user_id: &str,
    id_str: &str,
) -> PathResolveOutcome {
    if let Ok(uuid) = parse_canonical_uuid(id_str) {
        // Canonical UUID parse OK — for parity with prior behaviour we
        // treat this as `outcome="hit"` regardless of whether the UUID is
        // present in the replica (the caller does the replica lookup and
        // returns 404 empty body on miss). The metric here measures
        // resolver outcome, not replica outcome.
        metric_counter!(
            "task_keys_resolution_total",
            "form" => "uuid",
            "outcome" => "hit"
        )
        .increment(1);
        return PathResolveOutcome::Resolved { form: "uuid", uuid };
    }
    resolve_via_key(state, user_id, id_str).await
}

/// Resolve a task path parameter under the **permissive-UUID** strictness
/// rule used by mutation handlers (`modify`, `complete`, `undo`, `delete`)
/// per Decisions Locked In iter3 — preserves existing acceptance of
/// uppercase / simple / braced / URN UUID forms. On UUID parse miss, tries
/// the task-key form. Tightening to canonical-only is a separate, gated
/// change requiring contract sign-off + iOS audit (see Open Question 1).
///
/// Fires `task_keys_resolution_total{form, outcome}` exactly once per call.
async fn resolve_task_path_param_permissive(
    state: &AppState,
    user_id: &str,
    id_str: &str,
) -> PathResolveOutcome {
    if let Ok(uuid) = Uuid::parse_str(id_str) {
        metric_counter!(
            "task_keys_resolution_total",
            "form" => "uuid",
            "outcome" => "hit"
        )
        .increment(1);
        return PathResolveOutcome::Resolved { form: "uuid", uuid };
    }
    resolve_via_key(state, user_id, id_str).await
}

/// Shared key-resolution path used by both canonical and permissive
/// resolvers. Fires the `form="key"` metric leg.
async fn resolve_via_key(state: &AppState, user_id: &str, id_str: &str) -> PathResolveOutcome {
    let (prefix, n) = match parse_task_key(id_str) {
        Some(parsed) => parsed,
        None => {
            metric_counter!(
                "task_keys_resolution_total",
                "form" => "key",
                "outcome" => "malformed"
            )
            .increment(1);
            return PathResolveOutcome::Malformed;
        }
    };
    match state
        .store
        .lookup_task_uuid_by_key(user_id, &prefix, n)
        .await
    {
        Ok(Some(uuid_str)) => match Uuid::parse_str(&uuid_str) {
            Ok(uuid) => {
                metric_counter!(
                    "task_keys_resolution_total",
                    "form" => "key",
                    "outcome" => "hit"
                )
                .increment(1);
                PathResolveOutcome::Resolved { form: "key", uuid }
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    user_id = %user_id,
                    prefix = %prefix,
                    n = n,
                    "lookup_task_uuid_by_key returned non-UUID string"
                );
                PathResolveOutcome::StoreError
            }
        },
        Ok(None) => {
            metric_counter!(
                "task_keys_resolution_total",
                "form" => "key",
                "outcome" => "miss"
            )
            .increment(1);
            PathResolveOutcome::KeyMiss
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                user_id = %user_id,
                prefix = %prefix,
                n = n,
                "lookup_task_uuid_by_key failed"
            );
            PathResolveOutcome::StoreError
        }
    }
}

/// Resolve a mutation handler's path parameter via the permissive
/// resolver, mapping non-`Resolved` outcomes to the legacy mutation-path
/// wire shape (empty 400 / empty 404 / empty 500) and emitting the
/// matching audit event via `log_rejected`. Returns the resolved UUID on
/// success or the `StatusCode` to return on failure (caller decides the
/// concrete `Response`/`Result<T, StatusCode>` shape since
/// `complete_task` etc. return `Result<Json<...>, StatusCode>` while
/// `modify_task` returns `Response`).
///
/// Audit reasons:
/// - `"invalid_uuid"` — malformed input (neither permissive UUID nor
///   syntactically valid key). Preserves existing convention so audit
///   dashboards keep working.
/// - `"unknown_key"` — input parsed as a key but has no committed
///   allocation row. Distinct from `"invalid_uuid"` so operators can
///   tell genuine format errors apart from cross-account / unknown-task
///   probes.
async fn resolve_mutation_path_param_or_audit(
    state: &AppState,
    auth: &AuthUser,
    headers: &HeaderMap,
    kind: TaskMutationKind,
    id_str: &str,
) -> Result<Uuid, StatusCode> {
    // Phase 4 entry-point gate. MUST run before key resolution: a first
    // access of the form `POST /api/tasks/WORK-1/done` against an
    // unmigrated user would otherwise fall through to `KeyMiss → 404`
    // because no allocation row exists yet. The gate is a single
    // DashMap lookup on the cache hot path; it's safe to run before
    // input parsing because malformed input still completes fast (the
    // cache hit is independent of the request body).
    ensure_user_task_keys_migrated_or_500(state, &auth.user_id).await?;

    match resolve_task_path_param_permissive(state, &auth.user_id, id_str).await {
        PathResolveOutcome::Resolved { uuid, .. } => Ok(uuid),
        PathResolveOutcome::Malformed => {
            mutations::log_rejected(headers, state, &auth.user_id, kind, None, "invalid_uuid");
            Err(StatusCode::BAD_REQUEST)
        }
        PathResolveOutcome::KeyMiss => {
            mutations::log_rejected(headers, state, &auth.user_id, kind, None, "unknown_key");
            Err(StatusCode::NOT_FOUND)
        }
        PathResolveOutcome::StoreError => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Validate that recognised raw-syntax date attributes (`wait:`, `scheduled:`)
/// parse via the broad `parse_date_value`. Per `task-write-contract.md`:
/// recognised attributes that fail to parse return `400 INVALID_DATE`.
///
/// Only the **new** attributes are validated here — `due:` continues to
/// silently drop on bad parse to preserve existing behaviour per the
/// arch ruling on #100 (retrofit tracked at #105).
fn validate_task_scope_fields(
    canonical_prefix: &str,
    cmdock_task_scope: Option<&str>,
) -> Result<(), &'static str> {
    if let Some(scope) = cmdock_task_scope {
        if !scope.eq_ignore_ascii_case(canonical_prefix) {
            return Err("INVALID_TASK_SCOPE");
        }
    }
    Ok(())
}

async fn canonical_task_scope_prefix_for_user(
    state: &AppState,
    user_id: &str,
) -> Result<String, StatusCode> {
    state
        .store
        .get_user_prefix(user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, user_id = %user_id, "get_user_prefix failed during task-scope validation");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or_else(|| {
            tracing::error!(user_id = %user_id, "missing user prefix during task-scope validation");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

fn validate_raw_recognised_dates(parsed: &super::parser::ParsedTask) -> Result<(), &'static str> {
    if let Some(s) = parsed.wait.as_deref() {
        if super::dates::parse_date_value(s).is_none() {
            return Err("INVALID_DATE");
        }
    }
    if let Some(s) = parsed.scheduled.as_deref() {
        if super::dates::parse_date_value(s).is_none() {
            return Err("INVALID_DATE");
        }
    }
    Ok(())
}

/// One of the three valid request shapes for `GET /api/tasks` per
/// `task-read-contract.md` § GET /api/tasks request shapes.
enum ListTasksDispatch {
    /// `GET /api/tasks` — pending list (default-views-contract).
    PendingList,
    /// `GET /api/tasks?view=<id>` — view-scoped read (default-views-contract).
    ViewScoped(String),
    /// `GET /api/tasks?uuids=<csv>` — batched UUID lookup (this contract).
    BatchedUuids(String),
}

/// Parse and dispatch the `GET /api/tasks` query string per
/// `task-read-contract.md` § GET /api/tasks request shapes:
///
/// - No params → pending list (preserved for backwards compatibility).
/// - `?view=<id>` → view-scoped read.
/// - `?uuids=<csv>` → batched lookup.
///
/// Returns `Err(reason)` (a static reason for tracing) when the query is
/// ill-formed: both `view` and `uuids` supplied, any key repeated, or an
/// unknown query key supplied. Caller maps `Err` to `400 INVALID_QUERY_PARAM`
/// (plain-text body, per § Wire-body convention).
///
/// This is the dispatch surface — strict-recognise on query parameters
/// is enforced here so the existing view path also benefits from the
/// rejection of unknown keys (CHANGELOG behaviour-change). The empty
/// `uuids` value (`?uuids=`) maps to `EMPTY_UUIDS` *inside* the batch
/// path, not here, because the contract gives that case its own code.
fn parse_list_tasks_query(raw: Option<&str>) -> Result<ListTasksDispatch, &'static str> {
    let Some(raw) = raw.filter(|s| !s.is_empty()) else {
        return Ok(ListTasksDispatch::PendingList);
    };

    let mut view: Option<String> = None;
    let mut uuids: Option<String> = None;

    for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
        let key = key.into_owned();
        let value = value.into_owned();
        match key.as_str() {
            "view" => {
                if view.is_some() {
                    return Err("view repeated");
                }
                view = Some(value);
            }
            "uuids" => {
                if uuids.is_some() {
                    return Err("uuids repeated");
                }
                uuids = Some(value);
            }
            other => {
                tracing::debug!(query_key = %other, "unknown query parameter");
                return Err("unknown query param");
            }
        }
    }

    match (view, uuids) {
        (Some(_), Some(_)) => Err("view and uuids both supplied"),
        // Treat empty `view=` as no-op (legacy lenient behaviour preserved
        // for the view path — clients have historically sent `?view=` with
        // no value to mean "no filter").
        (Some(v), None) if v.is_empty() => Ok(ListTasksDispatch::PendingList),
        (Some(v), None) => Ok(ListTasksDispatch::ViewScoped(v)),
        (None, Some(u)) => Ok(ListTasksDispatch::BatchedUuids(u)),
        (None, None) => Ok(ListTasksDispatch::PendingList),
    }
}

fn invalid_query_param(reason: &'static str) -> Response {
    tracing::debug!(reason, "INVALID_QUERY_PARAM");
    (StatusCode::BAD_REQUEST, "INVALID_QUERY_PARAM").into_response()
}

/// `GET /api/tasks` dispatcher.
///
/// Accepts three valid request shapes per `task-read-contract.md` §
/// GET /api/tasks request shapes:
///
/// 1. **No query params** — returns the user's pending-task list.
///    Backwards-compatible with pre-contract behaviour.
/// 2. **`?view=<id>`** — view-scoped read; applies the named view's
///    Taskwarrior filter expression. Owned by `default-views-contract.md`.
/// 3. **`?uuids=<csv>`** — batched UUID lookup; returns `{found, missing}`
///    with request-order preservation. Owned by `task-read-contract.md`.
///
/// `view` and `uuids` are mutually exclusive (`400 INVALID_QUERY_PARAM`
/// when both supplied). Unknown query parameters are rejected with
/// `400 INVALID_QUERY_PARAM` per the documented-whitelist principle.
///
/// **Response shape diverges by dispatch**:
/// - PendingList / ViewScoped → `Vec<TaskItem>` (existing shape).
/// - BatchedUuids → `TaskBatchLookupResponse` (`{found, missing}`).
///
/// Ordering: pending and view-scoped paths sort UUID-ascending per
/// `default-views-contract.md` § Sort ownership. Batched lookup
/// preserves request-order in `found` and `missing` per
/// `task-read-contract.md` § Sort Ownership.
#[utoipa::path(
    get,
    path = "/api/tasks",
    operation_id = "listTasks",
    params(TaskListQuery),
    responses(
        (status = 200, description = "Tasks. Body shape is polymorphic per request shape: `Vec<TaskItem>` for no-params and `?view=<id>`; `TaskBatchLookupResponse` for `?uuids=<csv>`. See `TaskListResponse` (oneOf).", body = TaskListResponse),
        (status = 400, description = "Invalid query — body is `INVALID_QUERY_PARAM` (mutually-exclusive view+uuids, repeated keys, unknown params), `EMPTY_UUIDS` (`?uuids=` empty), `TOO_MANY_UUIDS` (raw entries exceed the configured cap, default 100), or `INVALID_UUID` (malformed UUID/empty CSV segment in batch — note: must be canonical lowercase hyphenated form per `task-read-contract.md` § Path parameter)"),
        (status = 401, description = "Unauthorised — body: `Invalid token` or `Missing Authorization header` (plain text)"),
        (status = 404, description = "View not found (when `?view=<id>` is specified) — body: empty (no Content-Type, Content-Length: 0)"),
        (status = 500, description = "Internal server error")
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id))]
pub async fn list_tasks(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let dispatch = match parse_list_tasks_query(raw.as_deref()) {
        Ok(d) => d,
        Err(reason) => return invalid_query_param(reason),
    };
    if let Err(status) = ensure_user_task_keys_migrated_or_500(&state, &auth.user_id).await {
        return status.into_response();
    }
    match dispatch {
        ListTasksDispatch::PendingList => list_pending(&state, &auth).await,
        ListTasksDispatch::ViewScoped(view_id) => list_view_scoped(&state, &auth, &view_id).await,
        ListTasksDispatch::BatchedUuids(csv) => {
            list_batched_uuids(&state, &auth, &headers, &csv).await
        }
    }
}

/// Phase 4 entry-point gate: run the personal Task Scope lazy task-keys backfill
/// if it hasn't completed for this user. Fast-path is a `DashMap` lookup;
/// slow-path runs synchronously under the per-user mutation lock. Returns
/// `INTERNAL_SERVER_ERROR` on backfill failure (already logged inside).
async fn ensure_user_task_keys_migrated_or_500(
    state: &AppState,
    user_id: &str,
) -> Result<(), StatusCode> {
    crate::task_keys::backfill::ensure_user_task_keys_migrated(state, user_id)
        .await
        .map_err(|e| {
            tracing::error!(
                error = %e,
                user_id = %user_id,
                "task-keys backfill failed in read handler"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// Wrapper around `ConfigStore::lookup_task_keys_for_projection` that
/// reads `now` and `pending_timeout_seconds` from the runtime context.
/// `pending_timeout_seconds` reuses the same config value the reaper
/// observes (`task_write.idempotency_pending_timeout_seconds`) per
/// `task-write-contract.md` § REST projects from the allocation table.
async fn lookup_projection_keys(
    state: &AppState,
    user_id: &str,
    task_uuids: &[String],
) -> Result<HashMap<String, String>, crate::store::StoreError> {
    let now = chrono::Utc::now().timestamp();
    let pending_timeout = state.config.task_write.idempotency_pending_timeout_seconds;
    state
        .store
        .lookup_task_keys_for_projection(user_id, task_uuids, now, pending_timeout)
        .await
}

/// `GET /api/tasks` — no query params. Returns pending list.
async fn list_pending(state: &AppState, auth: &AuthUser) -> Response {
    let rep_arc = match open_user_replica(state, &auth.user_id, "api").await {
        Ok(arc) => arc,
        Err(status) => return status.into_response(),
    };
    let read_start = Instant::now();
    let pending = {
        let mut rep = rep_arc.lock().await;
        match rep.pending_tasks().await {
            Ok(v) => {
                m::record_replica_op("pending_tasks", read_start.elapsed().as_secs_f64(), "ok");
                v
            }
            Err(e) => {
                m::record_replica_op("pending_tasks", read_start.elapsed().as_secs_f64(), "error");
                let status = handle_replica_error(state, &auth.user_id, &e, "pending_tasks", "api");
                return status.into_response();
            }
        }
    };
    let pending_uuids: HashSet<Uuid> = pending
        .iter()
        .filter(|t| t.get_status() == Status::Pending)
        .map(|t| t.get_uuid())
        .collect();
    // Single batch lookup of allocation keys keyed by canonical UUID
    // string — the projection looks up `task.get_uuid().to_string()`.
    // Source of truth is `task_key_allocations`, NOT TC's `cmdock_key`
    // UDA (per `task-write-contract.md` § Task Keys).
    let task_uuid_strs: Vec<String> = pending
        .iter()
        .filter(|t| t.get_status() == Status::Pending)
        .map(|t| t.get_uuid().to_string())
        .collect();
    let task_keys = match lookup_projection_keys(state, &auth.user_id, &task_uuid_strs).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "lookup_task_keys_for_projection failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut result: Vec<TaskItem> = pending
        .iter()
        // TC's pending_tasks() should already exclude deleted tasks,
        // but enforce the API contract here in case it returns stale rows.
        .filter(|task| task.get_status() == Status::Pending)
        .map(|t| crate::tasks::projection::task_to_item(t, Some(&pending_uuids), Some(&task_keys)))
        .collect();
    // UUID-ascending — default-views-contract § Sort ownership.
    result.sort_unstable_by(|a, b| a.uuid.cmp(&b.uuid));
    Json(result).into_response()
}

/// `GET /api/tasks?view=<id>` — view-scoped read.
async fn list_view_scoped(state: &AppState, auth: &AuthUser, view_id: &str) -> Response {
    let rep_arc = match open_user_replica(state, &auth.user_id, "api").await {
        Ok(arc) => arc,
        Err(status) => return status.into_response(),
    };
    let view = match crate::views::resolve_view(state.store.as_ref(), &auth.user_id, view_id).await
    {
        Ok(Some(v)) => v,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("Failed to resolve view: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let read_start = Instant::now();
    let all = {
        let mut rep = rep_arc.lock().await;
        match rep.all_tasks().await {
            Ok(v) => {
                m::record_replica_op("all_tasks", read_start.elapsed().as_secs_f64(), "ok");
                v
            }
            Err(e) => {
                m::record_replica_op("all_tasks", read_start.elapsed().as_secs_f64(), "error");
                let status = handle_replica_error(state, &auth.user_id, &e, "all_tasks", "api");
                return status.into_response();
            }
        }
    };

    let filter_start = Instant::now();
    let tasks_scanned = all.len();
    let pending_uuids: HashSet<Uuid> = all
        .values()
        .filter(|t| t.get_status() == Status::Pending)
        .map(|t| t.get_uuid())
        .collect();
    // Thread `eval_ctx.now` through filter parsing so relative/named dates
    // resolve against the same reference time used for evaluation (#127).
    let eval_ctx = super::filter::EvalCtx::new();
    let parsed_filter = super::filter::parse_filter_at(&view.filter, eval_ctx.now);
    let filtered: Vec<&taskchampion::Task> = all
        .values()
        .filter(|t| super::filter::matches_with_context(t, &parsed_filter, &eval_ctx))
        .collect();
    // Single batch lookup over the filtered set (not the whole replica).
    let task_uuid_strs: Vec<String> = filtered.iter().map(|t| t.get_uuid().to_string()).collect();
    let task_keys = match lookup_projection_keys(state, &auth.user_id, &task_uuid_strs).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "lookup_task_keys_for_projection failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut result: Vec<TaskItem> = filtered
        .iter()
        .map(|t| crate::tasks::projection::task_to_item(t, Some(&pending_uuids), Some(&task_keys)))
        .collect();
    result.sort_unstable_by(|a, b| a.uuid.cmp(&b.uuid));
    m::record_filter_eval(
        filter_start.elapsed().as_secs_f64(),
        tasks_scanned,
        result.len(),
    );
    Json(result).into_response()
}

/// `GET /api/tasks?uuids=<csv>` — batched UUID lookup.
///
/// Implements the validation pipeline defined in `task-read-contract.md`
/// § Limits as a literal step machine:
///
/// 1. Parse CSV into raw entry list.
/// 2. `EMPTY_UUIDS` if list is empty after parse.
/// 3. `TOO_MANY_UUIDS` if `len(raw_entries) > batch_max_uuids`.
/// 4. `INVALID_UUID` if any entry fails syntactic validation
///    (including empty segments from leading/trailing/consecutive commas).
///    The offending index is recorded in tracing/audit; **not** in the wire body.
/// 5. Deduplicate the syntactically-valid list, preserving first-occurrence order.
/// 6. Resolve each unique UUID against the per-user replica.
///
/// Cap is applied **before** dedupe per the contract — 101 copies of the
/// same UUID returns `TOO_MANY_UUIDS`, not a one-element lookup.
///
/// Response: `{found: TaskItem[], missing: string[]}` with request-order
/// preservation.
async fn list_batched_uuids(
    state: &AppState,
    auth: &AuthUser,
    headers: &HeaderMap,
    csv: &str,
) -> Response {
    // Step 1: parse CSV. Whitespace around segments is trimmed before parsing.
    // Empty value (`?uuids=`) is detected here as zero entries → EMPTY_UUIDS.
    let raw_entries: Vec<&str> = if csv.is_empty() {
        Vec::new()
    } else {
        csv.split(',').map(str::trim).collect()
    };

    // Step 2: empty-after-parse → EMPTY_UUIDS.
    if raw_entries.is_empty() {
        return (StatusCode::BAD_REQUEST, "EMPTY_UUIDS").into_response();
    }

    // Step 3: cap. Applied to raw entries **before** dedup per contract.
    let cap = state.config.task_read.batch_max_uuids;
    if raw_entries.len() > cap {
        tracing::debug!(raw_entries = raw_entries.len(), cap, "TOO_MANY_UUIDS");
        return (StatusCode::BAD_REQUEST, "TOO_MANY_UUIDS").into_response();
    }

    // Step 4: syntactic validation. Empty segments (from leading, trailing,
    // or consecutive commas) fail Uuid::parse_str and surface as INVALID_UUID
    // with the offending index recorded in tracing — **not** in the wire body.
    let mut validated: Vec<Uuid> = Vec::with_capacity(raw_entries.len());
    for (idx, entry) in raw_entries.iter().enumerate() {
        match parse_canonical_uuid(entry) {
            Ok(u) => validated.push(u),
            Err(_) => {
                tracing::debug!(invalid_index = idx, "INVALID_UUID");
                // Audit: offending segment index recorded here for diagnostics
                // per task-read-contract.md § Limits step 4. Index is NOT
                // surfaced in the wire body — it travels via audit + tracing.
                // Common audit fields per docs/reference/audit-reference.md §
                // Event Shape: action, source, user_id, client_ip, request_id.
                let client_ip =
                    crate::audit::client_ip(headers, state.config.server.trust_forwarded_headers);
                let request_id = crate::audit::request_id(headers);
                tracing::warn!(
                    target: "audit",
                    action = "task.read.batch.invalid_uuid",
                    source = "api",
                    user_id = %auth.user_id,
                    client_ip = %client_ip,
                    request_id = ?request_id,
                    invalid_index = idx,
                );
                return (StatusCode::BAD_REQUEST, "INVALID_UUID").into_response();
            }
        }
    }

    // Step 5: deduplicate, preserving first-occurrence order.
    let mut seen: HashSet<Uuid> = HashSet::with_capacity(validated.len());
    let mut deduped: Vec<Uuid> = Vec::with_capacity(validated.len());
    for u in validated {
        if seen.insert(u) {
            deduped.push(u);
        }
    }

    // Step 6: resolve. Single all_tasks() fetch + HashMap lookup; cheaper
    // than N async TC calls under the replica mutex per the review (#109
    // implementation note 8).
    let rep_arc = match open_user_replica(state, &auth.user_id, "api").await {
        Ok(arc) => arc,
        Err(status) => return status.into_response(),
    };
    let read_start = Instant::now();
    let all = {
        let mut rep = rep_arc.lock().await;
        match rep.all_tasks().await {
            Ok(v) => {
                m::record_replica_op("all_tasks", read_start.elapsed().as_secs_f64(), "ok");
                v
            }
            Err(e) => {
                m::record_replica_op("all_tasks", read_start.elapsed().as_secs_f64(), "error");
                let status = handle_replica_error(state, &auth.user_id, &e, "all_tasks", "api");
                return status.into_response();
            }
        }
    };
    let pending_uuids: HashSet<Uuid> = all
        .values()
        .filter(|t| t.get_status() == Status::Pending)
        .map(|t| t.get_uuid())
        .collect();

    // Batch lookup keys for the resolved subset only — request-order
    // preserved per `task-read-contract.md` § Sort ownership.
    let resolved_uuid_strs: Vec<String> = deduped
        .iter()
        .filter(|u| all.contains_key(u))
        .map(|u| u.as_hyphenated().to_string())
        .collect();
    let task_keys = match lookup_projection_keys(state, &auth.user_id, &resolved_uuid_strs).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "lookup_task_keys_for_projection failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut found: Vec<TaskItem> = Vec::with_capacity(deduped.len());
    let mut missing: Vec<String> = Vec::new();
    for u in &deduped {
        match all.get(u) {
            Some(t) => found.push(crate::tasks::projection::task_to_item(
                t,
                Some(&pending_uuids),
                Some(&task_keys),
            )),
            None => missing.push(u.as_hyphenated().to_string()),
        }
    }

    Json(TaskBatchLookupResponse { found, missing }).into_response()
}

/// `GET /api/tasks/{uuid}` — singleton task lookup by UUID.
///
/// Returns the task if it exists in the authenticated identity's replica,
/// at any status (pending, completed, deleted). The contract permits
/// "have UUID, can read state at any status" because UUIDs are
/// effectively unguessable.
///
/// **Existence-leak rule**: unknown UUIDs and cross-account UUIDs return
/// **identical** `404` with empty body. The server MUST NOT distinguish
/// the two cases. Empty body (per the existing iOS-load-bearing 404
/// convention) makes this trivial to enforce — there is no body content
/// that could leak existence by varying.
///
/// Path-parameter validation runs **before** `open_user_replica` so that
/// malformed UUIDs short-circuit cleanly without surfacing quarantine
/// `503` or other replica errors as side channels (review item 1).
#[utoipa::path(
    get,
    path = "/api/tasks/{uuid}",
    operation_id = "getTaskById",
    params(
        ("uuid" = String, Path, description = "Canonical UUID string (lowercase hyphenated RFC 4122 textual representation) OR a task key in the form `<PREFIX>-N` (e.g. `WORK-15`). Prefix is case-insensitive (`work-15` resolves identically); the UUID branch remains canonical-only — uppercase / simple / braced / URN forms are rejected.")
    ),
    responses(
        (status = 200, description = "Task found", body = TaskItem),
        (status = 400, description = "Path parameter is neither a canonical UUID per the contract (lowercase hyphenated RFC 4122 textual representation `[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}` — uppercase, simple/no-hyphen, braced, and URN forms are rejected) nor a syntactically valid task key (`<PREFIX>-N`, prefix `^[A-Za-z][A-Za-z0-9]{0,9}$`, N `^[1-9][0-9]*$`). Body: `INVALID_UUID` (plain text, bare code)."),
        (status = 401, description = "Unauthorised — body: `Invalid token` or `Missing Authorization header` (plain text)"),
        (status = 404, description = "Task not found in the authenticated identity's replica (unknown UUID, unknown key, OR cross-account identifier — all indistinguishable per existence-leak rule). Body: empty (no Content-Type, Content-Length: 0)."),
        (status = 500, description = "Internal server error")
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id, uuid = %uuid_str))]
pub async fn get_task_by_id(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(uuid_str): Path<String>,
) -> Response {
    // Phase 4 entry-point gate: ensure pre-feature tasks have allocation
    // rows + cmdock_key UDA before projection so wire `key` is non-null.
    // Fast-path is a single DashMap lookup. Runs BEFORE path-param
    // resolution so the `KeyMiss` 404 path can rely on populated keys
    // for cross-account existence-leak parity.
    if let Err(status) = ensure_user_task_keys_migrated_or_500(&state, &auth.user_id).await {
        return status.into_response();
    }

    // Resolve the path parameter **before** opening the replica. Avoids
    // leaking quarantine 503 / replica errors as side channels for
    // malformed input. Canonical-strict UUID form per
    // task-read-contract.md § Path parameter; key form `<PREFIX>-N` per
    // task-keys contract. Key resolution misses map to 404 empty body
    // so the existence-leak rule applies uniformly to unknown-UUID and
    // unknown-key. Store errors during key lookup map to 500 — never
    // folded into 404 (would silently change visibility semantics on
    // transient DB failures).
    let uuid = match resolve_task_path_param_canonical(&state, &auth.user_id, &uuid_str).await {
        PathResolveOutcome::Resolved { uuid, .. } => uuid,
        PathResolveOutcome::Malformed => {
            return (StatusCode::BAD_REQUEST, "INVALID_UUID").into_response()
        }
        PathResolveOutcome::KeyMiss => return StatusCode::NOT_FOUND.into_response(),
        PathResolveOutcome::StoreError => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let rep_arc = match open_user_replica(&state, &auth.user_id, "api").await {
        Ok(arc) => arc,
        Err(status) => return status.into_response(),
    };

    // We need pending_uuids for accurate `blocked` semantics. Fetch all
    // tasks once — TC's get_task is cheap on a hot replica, but
    // `blocked` is computed from the pending-set so we need the full
    // map regardless. Mirrors the list_tasks pattern.
    let read_start = Instant::now();
    let all: HashMap<Uuid, taskchampion::Task> = {
        let mut rep = rep_arc.lock().await;
        match rep.all_tasks().await {
            Ok(v) => {
                m::record_replica_op("all_tasks", read_start.elapsed().as_secs_f64(), "ok");
                v
            }
            Err(e) => {
                m::record_replica_op("all_tasks", read_start.elapsed().as_secs_f64(), "error");
                let status = handle_replica_error(&state, &auth.user_id, &e, "all_tasks", "api");
                return status.into_response();
            }
        }
    };

    let task = match all.get(&uuid) {
        Some(t) => t,
        // Existence-leak rule: unknown UUID and cross-account UUID are
        // indistinguishable. Single account today; the empty-body 404 also
        // pre-empts any future cross-account leak via body content.
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let pending_uuids: HashSet<Uuid> = all
        .values()
        .filter(|t| t.get_status() == Status::Pending)
        .map(|t| t.get_uuid())
        .collect();
    // Singleton key lookup via the same projection primitive the list
    // endpoints use — `committed` rows surface always; `pending` rows
    // surface iff still within `pending_timeout` per
    // `task-write-contract.md` § REST projects from the allocation table.
    // Store errors propagate as 500 — the allocation table is the source
    // of truth for `key`, so silently omitting it on a transient DB error
    // would violate the contract for clients that rely on `key` being
    // present whenever the task has a non-expired allocation row.
    let uuid_str = task.get_uuid().to_string();
    let task_keys = match lookup_projection_keys(&state, &auth.user_id, &[uuid_str.clone()]).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "lookup_task_keys_for_projection failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    Json(crate::tasks::projection::task_to_item(
        task,
        Some(&pending_uuids),
        Some(&task_keys),
    ))
    .into_response()
}

/// Phase 2 of the idempotency state machine for `POST /api/tasks` (also
/// the no-key code path). Pure with respect to dedup — it parses, validates,
/// runs the service, fires webhooks/audit on success, and serialises the
/// response. Returns the `Phase2Outcome` the wrapper needs to drive
/// finalize/rollback decisions.
///
/// **Why an enum, not Result:** the contract requires distinguishing
/// `KnownNoCommit` (validation/business-rule rejection — pending row
/// rolled back) from `Ambiguous` (commit attempted, outcome unknown —
/// pending row LEFT IN PLACE). Collapsing both into an `Err` with a
/// status code would lose the distinction.
///
/// Service-layer 4xx (`ServiceError { phase: PreCommit, .. }`) maps to
/// `KnownNoCommit`. Genuine ambiguity surfaces from
/// `service::add_task` when `commit_task_key` fails after a successful
/// TC commit (the row stays pending+uuid-attached and the reaper's
/// TC-scan recovers it) — `phase: AmbiguousCommit` maps to `Ambiguous`.
/// `Replica::commit_operations` errors map the same way.
async fn execute_add_task(
    state: &AppState,
    auth: &AuthUser,
    headers: &HeaderMap,
    body_bytes: Vec<u8>,
) -> crate::idempotency::Phase2Outcome {
    use crate::idempotency::Phase2Outcome;

    // JSON deserialise + strict-recognise mapping. Substring-match the
    // serde error per the existing pattern.
    let body: AddTaskRequest = match serde_json::from_slice(&body_bytes) {
        Ok(b) => b,
        Err(e) => {
            mutations::log_rejected(
                headers,
                state,
                &auth.user_id,
                TaskMutationKind::Create,
                None,
                "invalid_payload",
            );
            let resp = serde_error_to_response(&e, JsonRejectionKind::AddTask);
            return phase2_known_no_commit(resp).await;
        }
    };

    // Garde validation (single-field — any failure → INVALID_RAW).
    if crate::validation::validate_or_bad_request(&body, "Invalid task payload").is_err() {
        mutations::log_rejected(
            headers,
            state,
            &auth.user_id,
            TaskMutationKind::Create,
            None,
            "invalid_payload",
        );
        return phase2_known_no_commit((StatusCode::BAD_REQUEST, "INVALID_RAW").into_response())
            .await;
    }

    // Recognised-date attribute validation (wait:, scheduled: in the raw
    // string). Bad parse → INVALID_DATE. Contract § parse_raw recognised
    // attributes; ADR-0011 § Per-Attribute-Family Evolution.
    let parsed_dates_check = super::parser::parse_raw(&body.raw);
    if let Err(code) = validate_raw_recognised_dates(&parsed_dates_check) {
        mutations::log_rejected(
            headers,
            state,
            &auth.user_id,
            TaskMutationKind::Create,
            None,
            "invalid_date",
        );
        return phase2_known_no_commit((StatusCode::BAD_REQUEST, code).into_response()).await;
    }

    let canonical_prefix = match canonical_task_scope_prefix_for_user(state, &auth.user_id).await {
        Ok(prefix) => prefix,
        Err(status) => return phase2_known_no_commit(status.into_response()).await,
    };
    if let Err(code) = validate_task_scope_fields(
        &canonical_prefix,
        parsed_dates_check.cmdock_task_scope.as_deref(),
    ) {
        mutations::log_rejected(
            headers,
            state,
            &auth.user_id,
            TaskMutationKind::Create,
            None,
            "invalid_task_scope",
        );
        return phase2_known_no_commit((StatusCode::BAD_REQUEST, code).into_response()).await;
    }

    let outcome = match service::add_task(state, &auth.user_id, &body).await {
        Ok(outcome) => outcome,
        Err(err) => {
            mutations::log_failed_status(
                headers,
                state,
                &auth.user_id,
                TaskMutationKind::Create,
                None,
                err.status,
            );
            // ServiceError carries a CommitPhase classification — Phase 2
            // failure mapping per task-write-contract.md § Failure handling.
            return phase2_from_service_error(err).await;
        }
    };

    // Webhooks + audit fire ONCE per first-execution; dedup replays do
    // NOT re-fire them.
    mutations::finalize_success(
        state,
        headers,
        &auth.user_id,
        outcome.kind,
        outcome.uuid,
        outcome.task_item,
        outcome.changed_fields,
        outcome.audit,
    )
    .await;

    let response_payload = TaskActionResponse {
        success: true,
        output: format!("Created task {}.", outcome.uuid),
        key: outcome.key,
    };
    let (response_body, content_type) = capture_json_response_bytes(&response_payload);
    Phase2Outcome::Success {
        status: StatusCode::OK,
        response_body,
        content_type,
    }
}

/// Map a `service::ServiceError` to the appropriate `Phase2Outcome`
/// variant per `task-write-contract.md` § Failure handling.
///
/// `CommitPhase::PreCommit` → `KnownNoCommit` (validation/business-rule
/// rejection raised before any TC commit attempt — pending dedup row
/// gets rolled back so a retry runs fresh).
///
/// `CommitPhase::AmbiguousCommit` → `Ambiguous` (commit was attempted
/// but outcome is unknown — pending dedup row LEFT IN PLACE so
/// lookup-time expiry bounds the residual window). Critical for #114
/// codex iteration 1: previously every service error was misclassified
/// as KnownNoCommit, allowing immediate retries to duplicate a possibly-
/// committed mutation.
async fn phase2_from_service_error(
    err: service::ServiceError,
) -> crate::idempotency::Phase2Outcome {
    let resp = err.status.into_response();
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();
    let content_type = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match err.phase {
        service::CommitPhase::PreCommit => crate::idempotency::Phase2Outcome::KnownNoCommit {
            status: parts.status,
            response_body: bytes,
            content_type,
        },
        service::CommitPhase::AmbiguousCommit => crate::idempotency::Phase2Outcome::Ambiguous {
            status: parts.status,
            response_body: bytes,
            content_type,
        },
    }
}

/// Capture an existing `Response` as `Phase2Outcome::KnownNoCommit`.
/// Reads the response body via `axum::body::to_bytes` so the dedup
/// rollback machinery can return the same bytes to the client.
async fn phase2_known_no_commit(resp: Response) -> crate::idempotency::Phase2Outcome {
    use crate::idempotency::Phase2Outcome;
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();
    let content_type = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Phase2Outcome::KnownNoCommit {
        status: parts.status,
        response_body: bytes,
        content_type,
    }
}

/// Add a new task using Taskwarrior raw syntax.
///
/// Body strict-recognise per `task-write-contract.md` § Request body:
/// unknown top-level fields → `400 INVALID_FIELD`. Recognised raw-syntax
/// attributes (project, +tag, priority, due, wait, scheduled) are extracted;
/// other tokens fall through to the description (lenient-drop deviation).
///
/// Idempotency-Key support per § Idempotency: when the header is
/// supplied, retries with the same key replay the original response
/// without re-creating a task. See `task-write-contract.md` § Server
/// behaviour for the three-phase pattern. Key absent → at-least-once
/// retry semantics (existing behaviour).
#[utoipa::path(
    post,
    path = "/api/tasks",
    operation_id = "addTask",
    request_body = AddTaskRequest,
    params(
        ("Idempotency-Key" = Option<String>, Header, description = "Optional ASCII string (1-64 chars) for exactly-once-per-key retry semantics. Same key + same body within 24h → replays original response. Same key + different body → 409 IDEMPOTENCY_KEY_CONFLICT. Same key with original still in-flight → 503 IDEMPOTENCY_IN_FLIGHT.")
    ),
    responses(
        (status = 200, description = "Task created", body = TaskActionResponse),
        (status = 400, description = "Invalid task payload — body `INVALID_FIELD` (unknown top-level field), `INVALID_RAW` (raw missing/empty/control-chars), `INVALID_DATE` (recognised raw-syntax date attribute fails to parse), `INVALID_TASK_SCOPE` (`cmdock_task_scope` / deprecated `cmdock_account` does not match the user's current Task Scope prefix), `INVALID_BODY` (malformed JSON), or `INVALID_IDEMPOTENCY_KEY` (header empty / >64 chars / non-printable-ASCII)"),
        (status = 401, description = "Unauthorised"),
        (status = 409, description = "Idempotency-Key replayed with different body. Body: `IDEMPOTENCY_KEY_CONFLICT`."),
        (status = 503, description = "Idempotency-Key in flight (original attempt still running or stranded by process death). Body: `IDEMPOTENCY_IN_FLIGHT`. Carries `Retry-After` header (default 5s)."),
        (status = 500, description = "Internal server error")
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id))]
pub async fn add_task(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    body_bytes: Bytes,
) -> Response {
    let key = match crate::idempotency::header_outcome(&headers) {
        crate::idempotency::HeaderOutcome::Absent => None,
        crate::idempotency::HeaderOutcome::Present(s) => Some(s.to_string()),
        crate::idempotency::HeaderOutcome::Invalid => {
            return (StatusCode::BAD_REQUEST, "INVALID_IDEMPOTENCY_KEY").into_response();
        }
    };

    let bytes = body_bytes.to_vec();

    if let Some(key) = key {
        let state_for_phase2 = state.clone();
        let auth_for_phase2 = auth.clone();
        let headers_for_phase2 = headers.clone();
        crate::idempotency::run_idempotent(
            &state,
            &auth.user_id,
            "/api/tasks",
            &headers,
            &key,
            &bytes,
            move |b| async move {
                execute_add_task(&state_for_phase2, &auth_for_phase2, &headers_for_phase2, b).await
            },
        )
        .await
    } else {
        // No-key path: run the inner pipeline directly and unwrap to a
        // wire response. Mirror the dedup wrapper's "outcome → response"
        // mapping.
        match execute_add_task(&state, &auth, &headers, bytes).await {
            crate::idempotency::Phase2Outcome::Success {
                status,
                response_body,
                content_type,
            }
            | crate::idempotency::Phase2Outcome::KnownNoCommit {
                status,
                response_body,
                content_type,
            }
            | crate::idempotency::Phase2Outcome::Ambiguous {
                status,
                response_body,
                content_type,
            } => {
                let mut resp = (status, response_body).into_response();
                if let Some(ct) = content_type {
                    if let Ok(v) = axum::http::HeaderValue::from_str(&ct) {
                        resp.headers_mut().insert("content-type", v);
                    }
                }
                resp
            }
        }
    }
}

/// Mark a task as completed.
///
/// Uses POST (not PUT/PATCH) for backwards compatibility with the iOS app.
/// Returns 409 Conflict if the task was concurrently deleted by another request.
#[utoipa::path(
    post,
    path = "/api/tasks/{uuid}/done",
    operation_id = "completeTask",
    params(("uuid" = String, Path, description = "Task UUID OR task key (`<PREFIX>-N`, prefix case-insensitive). UUID branch is permissive — accepts canonical, uppercase, simple/no-hyphen, braced, and URN forms (preserved for backwards compatibility).")),
    responses(
        (status = 200, description = "Task completed", body = TaskActionResponse),
        (status = 400, description = "Path parameter is neither a permissive UUID nor a syntactically valid task key (`<PREFIX>-N`). Body: empty (legacy iOS-compatible mutation 400 — `INVALID_UUID` plain-text body is reserved for the read singleton)."),
        (status = 401, description = "Unauthorised"),
        (status = 404, description = "Task not found — UUID/key resolved but no matching task in the authenticated identity's replica (unknown UUID, unknown key, OR cross-account identifier — all indistinguishable per existence-leak rule). Body: empty."),
        (status = 409, description = "Conflict — task was concurrently modified or deleted"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id, uuid = %uuid_str))]
pub async fn complete_task(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(uuid_str): Path<String>,
) -> Result<Json<TaskActionResponse>, StatusCode> {
    let uuid = resolve_mutation_path_param_or_audit(
        &state,
        &auth,
        &headers,
        TaskMutationKind::Complete,
        &uuid_str,
    )
    .await?;
    let outcome = match service::complete_task(&state, &auth.user_id, uuid).await {
        Ok(outcome) => outcome,
        Err(status) => {
            mutations::log_failed_status(
                &headers,
                &state,
                &auth.user_id,
                TaskMutationKind::Complete,
                Some(uuid),
                status,
            );
            return Err(status);
        }
    };

    mutations::finalize_success(
        &state,
        &headers,
        &auth.user_id,
        outcome.kind,
        outcome.uuid,
        outcome.task_item,
        outcome.changed_fields,
        outcome.audit,
    )
    .await;

    Ok(Json(TaskActionResponse {
        success: true,
        output: format!("Completed task {}.", outcome.uuid),
        key: None,
    }))
}

/// Soft-delete a task (sets status to deleted).
///
/// Uses POST (not DELETE) for backwards compatibility with the iOS app.
/// Returns 409 Conflict if the task was concurrently modified.
#[utoipa::path(
    post,
    path = "/api/tasks/{uuid}/undo",
    operation_id = "undoTask",
    params(("uuid" = String, Path, description = "Task UUID OR task key (`<PREFIX>-N`, prefix case-insensitive). UUID branch is permissive — accepts canonical, uppercase, simple/no-hyphen, braced, and URN forms (preserved for backwards compatibility).")),
    responses(
        (status = 200, description = "Task marked pending again", body = TaskActionResponse),
        (status = 400, description = "Path parameter is neither a permissive UUID nor a syntactically valid task key. Body: empty."),
        (status = 401, description = "Unauthorised"),
        (status = 404, description = "Task not found — UUID/key resolved but no matching task in the authenticated identity's replica (unknown identifier OR cross-account — indistinguishable per existence-leak rule). Body: empty."),
        (status = 409, description = "Conflict — task is not currently completed"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id, uuid = %uuid_str))]
pub async fn undo_task(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(uuid_str): Path<String>,
) -> Result<Json<TaskActionResponse>, StatusCode> {
    let uuid = resolve_mutation_path_param_or_audit(
        &state,
        &auth,
        &headers,
        TaskMutationKind::Undo,
        &uuid_str,
    )
    .await?;
    let outcome = match service::undo_task(&state, &auth.user_id, uuid).await {
        Ok(outcome) => outcome,
        Err(status) => {
            mutations::log_failed_status(
                &headers,
                &state,
                &auth.user_id,
                TaskMutationKind::Undo,
                Some(uuid),
                status,
            );
            return Err(status);
        }
    };

    mutations::finalize_success(
        &state,
        &headers,
        &auth.user_id,
        outcome.kind,
        outcome.uuid,
        outcome.task_item,
        outcome.changed_fields,
        outcome.audit,
    )
    .await;

    Ok(Json(TaskActionResponse {
        success: true,
        output: format!("Reopened task {}.", outcome.uuid),
        key: None,
    }))
}

/// Soft-delete a task (sets status to deleted).
///
/// Uses POST (not DELETE) for backwards compatibility with the iOS app.
/// Returns 409 Conflict if the task was concurrently modified.
#[utoipa::path(
    post,
    path = "/api/tasks/{uuid}/delete",
    operation_id = "deleteTask",
    params(("uuid" = String, Path, description = "Task UUID OR task key (`<PREFIX>-N`, prefix case-insensitive). UUID branch is permissive — accepts canonical, uppercase, simple/no-hyphen, braced, and URN forms (preserved for backwards compatibility).")),
    responses(
        (status = 200, description = "Task deleted", body = TaskActionResponse),
        (status = 400, description = "Path parameter is neither a permissive UUID nor a syntactically valid task key. Body: empty."),
        (status = 401, description = "Unauthorised"),
        (status = 404, description = "Task not found — UUID/key resolved but no matching task in the authenticated identity's replica (unknown identifier OR cross-account — indistinguishable per existence-leak rule). Body: empty."),
        (status = 409, description = "Conflict — task was concurrently modified"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id, uuid = %uuid_str))]
pub async fn delete_task(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(uuid_str): Path<String>,
) -> Result<Json<TaskActionResponse>, StatusCode> {
    let uuid = resolve_mutation_path_param_or_audit(
        &state,
        &auth,
        &headers,
        TaskMutationKind::Delete,
        &uuid_str,
    )
    .await?;
    let outcome = match service::delete_task(&state, &auth.user_id, uuid).await {
        Ok(outcome) => outcome,
        Err(status) => {
            mutations::log_failed_status(
                &headers,
                &state,
                &auth.user_id,
                TaskMutationKind::Delete,
                Some(uuid),
                status,
            );
            return Err(status);
        }
    };

    mutations::finalize_success(
        &state,
        &headers,
        &auth.user_id,
        outcome.kind,
        outcome.uuid,
        outcome.task_item,
        outcome.changed_fields,
        outcome.audit,
    )
    .await;

    Ok(Json(TaskActionResponse {
        success: true,
        output: format!("Deleted task {}.", outcome.uuid),
        key: None,
    }))
}

/// Phase 2 of the idempotency state machine for `POST /api/tasks/{uuid}/modify`
/// (and the no-key code path). Same shape as `execute_add_task`.
async fn execute_modify_task(
    state: &AppState,
    auth: &AuthUser,
    headers: &HeaderMap,
    uuid: Uuid,
    body_bytes: Vec<u8>,
) -> crate::idempotency::Phase2Outcome {
    use crate::idempotency::Phase2Outcome;

    let body: ModifyTaskRequest = match serde_json::from_slice(&body_bytes) {
        Ok(b) => b,
        Err(e) => {
            mutations::log_rejected(
                headers,
                state,
                &auth.user_id,
                TaskMutationKind::Modify,
                Some(uuid),
                "invalid_payload",
            );
            return phase2_known_no_commit(serde_error_to_response(&e, JsonRejectionKind::Modify))
                .await;
        }
    };

    if crate::validation::validate_or_bad_request(&body, "Invalid task payload").is_err() {
        mutations::log_rejected(
            headers,
            state,
            &auth.user_id,
            TaskMutationKind::Modify,
            Some(uuid),
            "invalid_payload",
        );
        return phase2_known_no_commit(StatusCode::BAD_REQUEST.into_response()).await;
    }

    let canonical_prefix = match canonical_task_scope_prefix_for_user(state, &auth.user_id).await {
        Ok(prefix) => prefix,
        Err(status) => return phase2_known_no_commit(status.into_response()).await,
    };
    if let Err(code) =
        validate_task_scope_fields(&canonical_prefix, body.cmdock_task_scope.as_deref())
    {
        mutations::log_rejected(
            headers,
            state,
            &auth.user_id,
            TaskMutationKind::Modify,
            Some(uuid),
            "invalid_task_scope",
        );
        return phase2_known_no_commit((StatusCode::BAD_REQUEST, code).into_response()).await;
    }

    // Canonical-only date validation for `wait` / `scheduled` per
    // `task-write-contract.md` § Date format on modify.
    if let Some(Some(s)) = body.wait.as_ref() {
        if !is_canonical_tw_date(s) {
            mutations::log_rejected(
                headers,
                state,
                &auth.user_id,
                TaskMutationKind::Modify,
                Some(uuid),
                "invalid_date",
            );
            return phase2_known_no_commit(
                (StatusCode::BAD_REQUEST, "INVALID_DATE").into_response(),
            )
            .await;
        }
    }
    if let Some(Some(s)) = body.scheduled.as_ref() {
        if !is_canonical_tw_date(s) {
            mutations::log_rejected(
                headers,
                state,
                &auth.user_id,
                TaskMutationKind::Modify,
                Some(uuid),
                "invalid_date",
            );
            return phase2_known_no_commit(
                (StatusCode::BAD_REQUEST, "INVALID_DATE").into_response(),
            )
            .await;
        }
    }

    let parsed_depends = match service::parse_modify_dependencies(uuid, body.depends.as_ref()) {
        Ok(value) => value,
        Err(reason) => {
            mutations::log_rejected(
                headers,
                state,
                &auth.user_id,
                TaskMutationKind::Modify,
                Some(uuid),
                reason,
            );
            return phase2_known_no_commit(StatusCode::BAD_REQUEST.into_response()).await;
        }
    };

    let outcome =
        match service::modify_task(state, &auth.user_id, uuid, &body, parsed_depends).await {
            Ok(outcome) => outcome,
            Err(err) => {
                mutations::log_failed_status(
                    headers,
                    state,
                    &auth.user_id,
                    TaskMutationKind::Modify,
                    Some(uuid),
                    err.status,
                );
                return phase2_from_service_error(err).await;
            }
        };

    mutations::finalize_success(
        state,
        headers,
        &auth.user_id,
        outcome.kind,
        outcome.uuid,
        outcome.task_item,
        outcome.changed_fields,
        outcome.audit,
    )
    .await;

    let response_payload = TaskActionResponse {
        success: true,
        output: format!("Modified task {}.", outcome.uuid),
        key: None,
    };
    let (response_body, content_type) = capture_json_response_bytes(&response_payload);
    Phase2Outcome::Success {
        status: StatusCode::OK,
        response_body,
        content_type,
    }
}

/// Modify task fields. Only provided fields are updated.
///
/// Uses POST (not PATCH) for backwards compatibility with the iOS app.
/// Returns 409 Conflict if the task was concurrently deleted.
///
/// Idempotency-Key support per `task-write-contract.md` § Idempotency:
/// retries with the same key + same body replay the original response.
/// See `add_task` docs for the full state machine.
#[utoipa::path(
    post,
    path = "/api/tasks/{uuid}/modify",
    operation_id = "modifyTask",
    params(
        ("uuid" = String, Path, description = "Task UUID OR task key (`<PREFIX>-N`, prefix case-insensitive). UUID branch is permissive — accepts canonical, uppercase, simple/no-hyphen, braced, and URN forms (preserved for backwards compatibility)."),
        ("Idempotency-Key" = Option<String>, Header, description = "Optional ASCII string (1-64 chars) for exactly-once-per-key retry semantics. See `POST /api/tasks` for full state-machine docs."),
    ),
    request_body = ModifyTaskRequest,
    responses(
        (status = 200, description = "Task modified", body = TaskActionResponse),
        (status = 400, description = "Invalid path parameter or task payload. Path: empty body when neither a permissive UUID nor a syntactically valid task key (`<PREFIX>-N`). Payload: plain-text `INVALID_FIELD` (unknown top-level field), `INVALID_DATE` (wait/scheduled value not in canonical YYYYMMDDTHHmmssZ), `INVALID_TASK_SCOPE` (`cmdock_task_scope` / deprecated `cmdock_account` does not match the task's current Task Scope prefix), `INVALID_BODY` (malformed JSON), or `INVALID_IDEMPOTENCY_KEY` (header empty / >64 chars / non-printable-ASCII)."),
        (status = 401, description = "Unauthorised"),
        (status = 404, description = "Task not found — UUID/key resolved but no matching task in the authenticated identity's replica (unknown identifier OR cross-account — indistinguishable per existence-leak rule). Body: empty."),
        (status = 409, description = "Either task was concurrently deleted, or Idempotency-Key replayed with different body (`IDEMPOTENCY_KEY_CONFLICT`)"),
        (status = 503, description = "Idempotency-Key in flight. Body: `IDEMPOTENCY_IN_FLIGHT`."),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tasks"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id, uuid = %uuid_str))]
pub async fn modify_task(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(uuid_str): Path<String>,
    body_bytes: Bytes,
) -> Response {
    let uuid = match resolve_mutation_path_param_or_audit(
        &state,
        &auth,
        &headers,
        TaskMutationKind::Modify,
        &uuid_str,
    )
    .await
    {
        Ok(uuid) => uuid,
        Err(status) => return status.into_response(),
    };

    let key = match crate::idempotency::header_outcome(&headers) {
        crate::idempotency::HeaderOutcome::Absent => None,
        crate::idempotency::HeaderOutcome::Present(s) => Some(s.to_string()),
        crate::idempotency::HeaderOutcome::Invalid => {
            return (StatusCode::BAD_REQUEST, "INVALID_IDEMPOTENCY_KEY").into_response();
        }
    };

    let bytes = body_bytes.to_vec();

    if let Some(key) = key {
        let state_for_phase2 = state.clone();
        let auth_for_phase2 = auth.clone();
        let headers_for_phase2 = headers.clone();
        // Use the modify path (literal) as the dedup tuple component —
        // INCLUDES the {uuid} so two different tasks with the same key
        // are independent dedup records. Per § Limitations: tuple
        // includes request_path; encoding the UUID into request_path is
        // the natural fit for the parametrised modify endpoint.
        let request_path = format!("/api/tasks/{uuid}/modify");
        crate::idempotency::run_idempotent(
            &state,
            &auth.user_id,
            &request_path,
            &headers,
            &key,
            &bytes,
            move |b| async move {
                execute_modify_task(
                    &state_for_phase2,
                    &auth_for_phase2,
                    &headers_for_phase2,
                    uuid,
                    b,
                )
                .await
            },
        )
        .await
    } else {
        match execute_modify_task(&state, &auth, &headers, uuid, bytes).await {
            crate::idempotency::Phase2Outcome::Success {
                status,
                response_body,
                content_type,
            }
            | crate::idempotency::Phase2Outcome::KnownNoCommit {
                status,
                response_body,
                content_type,
            }
            | crate::idempotency::Phase2Outcome::Ambiguous {
                status,
                response_body,
                content_type,
            } => {
                let mut resp = (status, response_body).into_response();
                if let Some(ct) = content_type {
                    if let Ok(v) = axum::http::HeaderValue::from_str(&ct) {
                        resp.headers_mut().insert("content-type", v);
                    }
                }
                resp
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_task_key_accepts_canonical_form() {
        assert_eq!(parse_task_key("WORK-1"), Some(("WORK".into(), 1)));
        assert_eq!(parse_task_key("WORK-15"), Some(("WORK".into(), 15)));
        assert_eq!(parse_task_key("A-9"), Some(("A".into(), 9)));
        assert_eq!(
            parse_task_key("ABCDEFGHIJ-99"),
            Some(("ABCDEFGHIJ".into(), 99))
        );
    }

    #[test]
    fn parse_task_key_case_folds_prefix_to_uppercase() {
        assert_eq!(parse_task_key("work-15"), Some(("WORK".into(), 15)));
        assert_eq!(parse_task_key("Work-15"), Some(("WORK".into(), 15)));
        assert_eq!(parse_task_key("woRK-1"), Some(("WORK".into(), 1)));
    }

    #[test]
    fn parse_task_key_accepts_alphanumeric_prefix_after_first() {
        assert_eq!(parse_task_key("U2-3"), Some(("U2".into(), 3)));
        assert_eq!(parse_task_key("A1B2C3-7"), Some(("A1B2C3".into(), 7)));
    }

    #[test]
    fn parse_task_key_rejects_leading_zero_n() {
        assert_eq!(parse_task_key("WORK-0"), None);
        assert_eq!(parse_task_key("WORK-01"), None);
        assert_eq!(parse_task_key("WORK-007"), None);
    }

    #[test]
    fn parse_task_key_rejects_missing_or_empty_n() {
        assert_eq!(parse_task_key("WORK-"), None);
        assert_eq!(parse_task_key("WORK"), None);
    }

    #[test]
    fn parse_task_key_rejects_missing_or_invalid_prefix() {
        assert_eq!(parse_task_key("-15"), None);
        assert_eq!(parse_task_key("1WORK-15"), None);
        assert_eq!(parse_task_key("_WORK-15"), None);
    }

    #[test]
    fn parse_task_key_rejects_prefix_too_long() {
        assert_eq!(parse_task_key("ABCDEFGHIJK-1"), None);
    }

    #[test]
    fn parse_task_key_rejects_non_ascii_alphanumeric_chars() {
        assert_eq!(parse_task_key("WORK!-1"), None);
        assert_eq!(parse_task_key("WO RK-1"), None);
        assert_eq!(parse_task_key("WORKé-1"), None);
    }

    #[test]
    fn parse_task_key_rejects_canonical_uuid() {
        assert_eq!(parse_task_key("550e8400-e29b-41d4-a716-446655440000"), None);
    }

    #[test]
    fn parse_task_key_rejects_negative_or_huge_n() {
        assert_eq!(parse_task_key("WORK--1"), None);
        assert_eq!(parse_task_key("WORK-99999999999999999999999999999"), None);
    }

    #[test]
    fn parse_task_key_dash_in_n_position_invalid() {
        assert_eq!(parse_task_key("WORK-1-2"), None);
    }
}
