use std::collections::HashMap;

use garde::Validate;
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;

/// Distinguish "field omitted" from "field present with explicit `null`".
///
/// `task-write-contract.md` § Clear semantics requires JSON-Merge-Patch
/// behaviour on string-valued user-data fields: explicit `null` clears,
/// omission leaves unchanged. Plain `Option<T>` collapses both into `None`.
/// `Option<Option<T>>` paired with `#[serde(default, deserialize_with =
/// "double_option")]` preserves the distinction.
///
/// - Outer `None` — field absent in JSON → leave unchanged
/// - Outer `Some(None)` — field present, value `null` → clear
/// - Outer `Some(Some(v))` — field present, value `v` → set
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A timestamped note attached to a task. Free-form text — clients render
/// this as markdown (the iOS app does so via its `decodeIfPresent` path).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "entry": "20260501T120000Z",
    "description": "## Sections needed\n- intro\n- conclusion"
}))]
pub struct TaskAnnotation {
    /// Time the annotation was made (Taskwarrior format, YYYYMMDDTHHmmssZ)
    #[schema(example = "20260501T120000Z", pattern = r"^\d{8}T\d{6}Z$")]
    pub entry: String,
    /// Annotation body. Rendered as markdown by the iOS client.
    pub description: String,
}

/// Task as returned by the API — matches iOS TaskItem model
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "uuid": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "description": "Buy milk",
    "project": "PERSONAL.Home",
    "tags": ["shopping", "coles"],
    "priority": "H",
    "due": "20260328T090000Z",
    "urgency": 12.47,
    "depends": [],
    "blocked": false,
    "waiting": false,
    "status": "pending",
    "key": "WORK-15",
    "cmdock_task_scope": "WORK",
    "estimate": "large"
}))]
pub struct TaskItem {
    /// Task UUID
    #[schema(format = "uuid")]
    pub uuid: String,
    pub description: String,
    /// Project name (dot-separated hierarchy, e.g. "PERSONAL.Home")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Priority level
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, example = "H")]
    pub priority: Option<String>,
    /// Due date in Taskwarrior format (YYYYMMDDTHHmmssZ)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "20260328T090000Z", pattern = r"^\d{8}T\d{6}Z$")]
    pub due: Option<String>,
    /// Computed urgency score
    pub urgency: f64,
    /// UUIDs of pending tasks this task depends on (blocked by).
    /// Sorted deterministically. Empty when no unresolved dependencies.
    #[serde(default)]
    pub depends: Vec<String>,
    /// True when the task depends on at least one pending task.
    pub blocked: bool,
    /// True when the task has a future wait date and is not yet actionable.
    pub waiting: bool,
    /// Task status
    #[schema(example = "pending")]
    pub status: String,
    /// User-facing canonical key (`<PREFIX>-<n>`) per
    /// `task-write-contract.md` § Task Keys. **Nullable** on the wire
    /// (`string | null`) per the contract's wire-exposure rule: REST
    /// emits `null` when there is no `committed` row and no non-expired
    /// `pending` row in the allocation table for the task's UUID.
    /// Four transient causes per `task-write-contract.md` § Wire
    /// exposure: pre-migration / migration-in-retry, burned, expired
    /// pending (lookup-time), orphan UDA without allocation row. All
    /// four resolve on the next backfill / reaper / orphan-reconcile
    /// pass. Clients MUST handle `null` gracefully but MUST NOT treat
    /// it as task non-existence.
    #[serde(default)]
    #[schema(
        example = "WORK-15",
        pattern = r"^[A-Z][A-Z0-9]{0,9}-[1-9]\d*$",
        nullable = true,
        required = true
    )]
    pub key: Option<String>,
    /// Canonical Task Scope key prefix for Taskwarrior interoperability.
    /// Derived from the task-key allocation row, not from spoofable TC UDA
    /// content. `null` when `key` is `null`.
    #[serde(default)]
    #[schema(example = "WORK", nullable = true, required = true)]
    pub cmdock_task_scope: Option<String>,
    /// Timestamped annotations attached to the task. Sorted by entry
    /// (oldest first). Empty when the task has no annotations — omitted from
    /// the JSON entirely in that case so existing decoders that use
    /// `decodeIfPresent` keep working.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<TaskAnnotation>,
    /// User-defined attributes (UDAs) not covered by explicit fields.
    /// Emitted as top-level JSON keys. Values are strings (matching TC storage).
    #[serde(flatten, default)]
    pub extra: HashMap<String, String>,
}

/// Response for task mutations (add, done, undo, delete, modify)
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "success": true,
    "output": "Created task a1b2c3d4-e5f6-7890-abcd-ef1234567890.",
    "key": "WORK-15"
}))]
pub struct TaskActionResponse {
    pub success: bool,
    pub output: String,
    /// User-facing canonical key (`<PREFIX>-<n>`) for the affected task.
    /// Populated on `add_task` once the allocation row is committed.
    /// Lifecycle endpoints (`done`, `undo`, `delete`, `modify`) leave
    /// this `None` — their action responses don't surface the key today;
    /// clients that need it should consult the projected `TaskItem.key`
    /// in the webhook payload or via a subsequent GET. Omitted from the
    /// JSON when `None` so legacy decoders are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "WORK-15", pattern = r"^[A-Z][A-Z0-9]{0,9}-[1-9]\d*$")]
    pub key: Option<String>,
}

/// Response body for `GET /api/tasks?uuids=<csv>` per
/// `task-read-contract.md` § Response. Partial-success at the HTTP level —
/// status is `200 OK` whenever the request is well-formed and authenticated;
/// missing UUIDs are conveyed in the `missing` array, not as an error.
///
/// Both arrays preserve the request-order position of the corresponding
/// UUID; deduped duplicates appear once at first-occurrence position.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "found": [{
        "uuid": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "description": "Buy milk",
        "urgency": 12.47,
        "blocked": false,
        "waiting": false,
        "status": "pending"
    }],
    "missing": ["c3d4e5f6-a7b8-9012-cdef-345678901234"]
}))]
pub struct TaskBatchLookupResponse {
    /// Tasks resolved against the authenticated identity's replica.
    /// Ordered by request position (not UUID-ascending).
    pub found: Vec<TaskItem>,
    /// UUIDs from the request that did not resolve — either unknown to
    /// the server or owned by a different identity. Kept indistinguishable
    /// per `task-read-contract.md` § Visibility rule (existence-leak rule).
    /// Ordered by request position.
    pub missing: Vec<String>,
}

/// Polymorphic 200 body for `GET /api/tasks`. Two shapes per
/// `task-read-contract.md` § GET /api/tasks request shapes:
///
/// - **`Vec<TaskItem>`** — when the request has no query params or
///   `?view=<id>` is supplied. UUID-ascending sort.
/// - **`TaskBatchLookupResponse`** — when `?uuids=<csv>` is supplied.
///   Request-order preservation in `found`/`missing`.
///
/// Documented as `oneOf` in the generated OpenAPI spec; clients pick the
/// shape based on which query parameter they sent.
///
/// This type is **doc-only** — never constructed at runtime; the handler
/// emits one of the two shapes directly. Its purpose is to give utoipa a
/// single response schema that captures the polymorphic behaviour for
/// generated clients (codex review #109 important issue 1).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum TaskListResponse {
    /// Pending list (no params) or view-scoped (`?view=<id>`) shape.
    Pending(Vec<TaskItem>),
    /// Batched lookup (`?uuids=<csv>`) shape.
    Batch(TaskBatchLookupResponse),
}

/// Request body for POST /api/tasks.
///
/// Strict-recognise per `task-write-contract.md` § Request body: unknown
/// top-level fields are rejected with `400 INVALID_FIELD`.
#[derive(Debug, Deserialize, ToSchema, Validate)]
#[serde(deny_unknown_fields)]
#[schema(example = json!({"raw": "project:PERSONAL.Home +shopping +coles wait:7d Buy milk"}))]
pub struct AddTaskRequest {
    /// Raw Taskwarrior syntax: project:X +tag priority:H due:date Description
    #[garde(
        length(min = 1, max = 4096),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub raw: String,
}

/// Request body for POST /api/tasks/{uuid}/modify.
///
/// Only provided fields are updated; omitted fields are left unchanged.
/// String-valued user-data fields where clearing is meaningful
/// (`project`, `priority`, `due`, `wait`, `scheduled`) honour
/// JSON-Merge-Patch semantics: explicit JSON `null` clears the value;
/// omission leaves unchanged. List-valued fields (`tags`, `depends`)
/// use empty-array clear semantics. `description` is not clearable —
/// the contract requires it to be non-empty when provided.
///
/// Strict-recognise per `task-write-contract.md` § Strict-recognise:
/// unknown top-level fields are rejected with `400 INVALID_FIELD`.
#[derive(Debug, Deserialize, ToSchema, Validate)]
#[serde(deny_unknown_fields)]
#[schema(example = json!({
    "priority": "M",
    "tags": ["shopping", "woolworths"],
    "wait": "20260601T090000Z",
    "depends": ["a1b2c3d4-e5f6-7890-abcd-ef1234567890"]
}))]
pub struct ModifyTaskRequest {
    /// Canonical Task Scope key prefix. Accepted only when it matches the
    /// task's current scope; cross-scope moves are a separate `/move` surface.
    /// Supplying a conflicting value returns `400 INVALID_TASK_SCOPE`.
    #[serde(default)]
    #[schema(example = "WORK")]
    #[garde(skip)]
    pub cmdock_task_scope: Option<String>,
    /// Deprecated compatibility alias. Read-only for writes: omitted is OK,
    /// matching `cmdock_task_scope` is tolerated for mixed legacy clients, and
    /// a conflicting value returns `400 INVALID_TASK_SCOPE`.
    #[serde(default)]
    #[schema(example = "WORK")]
    #[garde(skip)]
    pub cmdock_account: Option<String>,
    /// Due date. Explicit `null` clears.
    /// Continues to accept the broader date parser (named dates, ISO,
    /// relative durations) on set — canonical-only on `wait` / `scheduled`
    /// is a separate contract asymmetry tracked under § Date format on modify.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(example = "20260330T090000Z", pattern = r"^\d{8}T\d{6}Z$")]
    #[garde(inner(inner(
        length(min = 1, max = 64),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )))]
    pub due: Option<Option<String>>,
    /// Priority: H (high), M (medium), L (low). Explicit `null` clears.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(example = "M")]
    #[garde(inner(inner(
        length(min = 1, max = 8),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )))]
    pub priority: Option<Option<String>>,
    /// Project name (dot-separated hierarchy). Explicit `null` clears.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(example = "PERSONAL.Health")]
    #[garde(inner(inner(
        length(min = 1, max = 255),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )))]
    pub project: Option<Option<String>>,
    /// Replace all tags with this list. Empty array clears.
    #[garde(custom(crate::validation::optional_tag_list))]
    pub tags: Option<Vec<String>>,
    /// Replace all task dependencies with this list of task UUIDs.
    /// Use an empty array to clear all dependencies.
    #[garde(custom(crate::validation::optional_uuid_list))]
    pub depends: Option<Vec<String>>,
    /// Task description. Null-clears not applicable (description must be non-empty).
    #[garde(inner(
        length(min = 1, max = 4096),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    ))]
    pub description: Option<String>,
    /// Wait-until date in Taskwarrior canonical format (YYYYMMDDTHHmmssZ).
    /// Explicit JSON `null` clears the wait date; omission leaves unchanged.
    /// The broader date parser used by `parse_raw` does NOT apply here —
    /// canonical format only (see `task-write-contract.md` § Date format on modify).
    /// Validation is performed at the handler layer so non-canonical input
    /// returns the contract-specified `INVALID_DATE` body code.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(example = "20260608T090000Z", pattern = r"^\d{8}T\d{6}Z$")]
    #[garde(skip)]
    pub wait: Option<Option<String>>,
    /// Scheduled-for date in Taskwarrior canonical format (YYYYMMDDTHHmmssZ).
    /// Explicit JSON `null` clears the scheduled date; omission leaves unchanged.
    /// Canonical format only (see `task-write-contract.md` § Date format on modify).
    /// Validation at the handler layer (returns `INVALID_DATE` on non-canonical).
    #[serde(default, deserialize_with = "double_option")]
    #[schema(example = "20260608T090000Z", pattern = r"^\d{8}T\d{6}Z$")]
    #[garde(skip)]
    pub scheduled: Option<Option<String>>,
}
