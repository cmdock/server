use serde::{Deserialize, Serialize};

use crate::merged_sync_gateway::journal::{GatewayJournalState, GatewayRecoveryStatus};
use crate::runtime_policy::RuntimePolicy;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub created_at: String,
}

pub struct NewUser {
    pub username: String,
    pub password_hash: String,
}

/// Task Scope kind as stored in `task_scopes.kind`. S1 enables only Personal
/// Task Scopes; future Team Task Scopes must relax the DB CHECK constraints and
/// add a variant here in the same slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskScopeKind {
    Personal,
}

impl TaskScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Self::try_from(s).ok()
    }
}

impl TryFrom<&str> for TaskScopeKind {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "personal" => Ok(Self::Personal),
            _ => Err("unknown task scope kind"),
        }
    }
}

/// Lifecycle state for `task_scopes.status`. S1 creates active Personal Task
/// Scopes only; disabled/deleted are schema-reserved for later operator/team
/// flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskScopeStatus {
    Active,
    Disabled,
    Deleted,
}

impl TaskScopeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
            Self::Deleted => "deleted",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Self::try_from(s).ok()
    }
}

impl TryFrom<&str> for TaskScopeStatus {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "active" => Ok(Self::Active),
            "disabled" => Ok(Self::Disabled),
            "deleted" => Ok(Self::Deleted),
            _ => Err("unknown task scope status"),
        }
    }
}

/// Durable Task Scope row from `task_scopes`. The S1 materialisation paths are
/// `ensure_personal_task_scope_for_user` and `backfill_personal_task_scopes`;
/// allocation rows start referencing this stable identity in S2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskScopeRecord {
    pub id: String,
    pub kind: TaskScopeKind,
    pub owner_runtime_user_id: Option<String>,
    pub owner_team_id: Option<String>,
    pub key_prefix: String,
    /// Reserved for future physical canonical storage roots. S1 leaves this
    /// NULL; backup/storage-layout work in later slices decides who writes it.
    pub storage_path: Option<String>,
    pub status: TaskScopeStatus,
    pub created_at: String,
    /// Updated when mutable Task Scope metadata changes (future status/prefix
    /// changes). S1 creation sets it once and does not update it again.
    pub updated_at: String,
}

/// Result of idempotently ensuring a Personal Task Scope. `created` reflects
/// the actual INSERT effect in the store transaction, so metrics/audit labels
/// do not rely on race-prone caller pre-checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonalTaskScopeEnsure {
    pub scope: TaskScopeRecord,
    pub created: bool,
}

/// Three-state row model for task_key_allocations per
/// `task-write-contract.md` § Task Keys. See `migrations/025_*` for the
/// state-transition rationale (burn rows persist forever so `MAX(n)` can't
/// reuse numbers; pending → committed at TC-commit-success;
/// pending → burned on rollback / reaper expiry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Pending,
    Committed,
    Burned,
}

impl KeyState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Committed => "committed",
            Self::Burned => "burned",
        }
    }

    /// Parse a state value as stored on disk in `task_key_allocations.state`.
    /// Returns `None` for unknown values (which the schema CHECK constraint
    /// should already prevent — defence-in-depth).
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "committed" => Some(Self::Committed),
            "burned" => Some(Self::Burned),
            _ => None,
        }
    }
}

/// Returned by `select_stale_pending_task_keys`. The reaper coordinator
/// (Phase 1 C3, in `src/task_keys/reaper.rs`) consumes these and decides
/// per-row whether to commit (TC has matching UDA) or burn (no match).
#[derive(Debug, Clone)]
pub struct StalePendingCandidate {
    pub user_id: String,
    pub prefix: String,
    pub n: i64,
    pub attempt_id: String,
    /// `None` if the reservation never reached `attach_task_uuid_to_pending`
    /// — the caller can burn unconditionally (no in-flight create can be
    /// past attach without holding the per-user mutation lock the reaper
    /// also acquires).
    pub task_uuid: Option<String>,
}

/// Returned by `list_pending_attached_task_keys_for_user` for the
/// Phase 4 backfill reconciliation step. A pending row with a non-NULL
/// `task_uuid` is a previously crashed (or in-flight) `add_task` that
/// already attached its UUID to the allocation row before TC commit;
/// the backfill, holding the per-user mutation lock, finalises these
/// rows (write/check `cmdock_key` UDA → `commit_task_key`) before
/// computing fresh allocations so the atomic Phase A+C commit can't
/// collide on `idx_task_key_allocations_uuid`.
#[derive(Debug, Clone)]
pub struct PendingAttachedKey {
    pub prefix: String,
    pub n: i64,
    pub attempt_id: String,
    pub task_uuid: String,
}

/// Returned by `lookup_task_keys_for_drift` — Phase 5b sync-bridge drift
/// recovery primitive. Carries every non-burned allocation row for the
/// requested task UUIDs along with the metadata the drift-recovery decision
/// table needs: state (pending/committed), attempt_id (for
/// `commit_task_key` finalisation on the `pending`-with-matching branch),
/// and `created_at` (for lookup-time pending-expiry filtering).
///
/// Burned rows are intentionally excluded — drift recovery never operates
/// on burned allocations (they don't carry a current `cmdock_key`
/// claim). `task_uuid` is non-NULL by construction: the partial unique
/// index `idx_task_key_allocations_uuid` covers `state IN
/// ('pending','committed')` and the lookup filters on the same states.
#[derive(Debug, Clone)]
pub struct DriftAllocationRow {
    pub task_uuid: String,
    pub prefix: String,
    pub key: String,
    pub state: KeyState,
    pub attempt_id: String,
    /// `created_at` value as stored on disk (`%Y-%m-%dT%H:%M:%fZ`). Caller
    /// applies pending-expiry comparison if needed; the store layer just
    /// passes the value through.
    pub created_at: String,
}

/// Returned by `users_without_prefix` for the startup-routine prefix
/// backfill (called from C4). Carries id + username only — derive_prefix
/// doesn't need anything else.
#[derive(Debug, Clone)]
pub struct UserWithoutPrefix {
    pub id: String,
    pub username: String,
}

/// Returned by `list_users_pending_personal_task_scope` for the S1 Task Scope
/// startup backfill. Carries the already-assigned prefix so the backfill does
/// not need an N+1 `get_user_prefix` loop.
#[derive(Debug, Clone)]
pub struct UserMissingPersonalTaskScope {
    pub id: String,
    pub username: String,
    pub prefix: String,
}

/// Input for creating a merged-sync gateway journal attempt. The store mints
/// no domain values here — the gateway supplies UUID-like strings for
/// `journal_id` and `attempt_id` so recovery can correlate logs and stale
/// finalizers can be rejected by exact attempt match.
#[derive(Debug, Clone)]
pub struct NewMergedSyncJournalAttempt {
    pub journal_id: String,
    pub user_id: String,
    pub client_id: String,
    pub attempt_id: String,
    pub parent_version_id: String,
    /// Raw plaintext inbound TaskChampion history segment. Stored at receive
    /// time so forward recovery can replay accepted source intent without
    /// reparsing protocol storage or guessing after restart.
    pub inbound_history_segment: Vec<u8>,
}

/// Optional operator-readable diagnostics attached to terminal or recoverable
/// gateway journal states.
#[derive(Debug, Clone, Default)]
pub struct MergedSyncJournalDiagnostic {
    pub code: Option<String>,
    pub message: Option<String>,
}

/// Forward-only compare-and-swap transition request for a journal row.
#[derive(Debug, Clone)]
pub struct MergedSyncJournalTransition<'a> {
    pub journal_id: &'a str,
    pub attempt_id: &'a str,
    pub from_state: GatewayJournalState,
    pub to_state: GatewayJournalState,
    pub merged_version_id: Option<&'a str>,
    pub recovery_status: GatewayRecoveryStatus,
    pub diagnostic: Option<&'a MergedSyncJournalDiagnostic>,
}

/// Durable merged-sync gateway journal row.
#[derive(Debug, Clone)]
pub struct MergedSyncJournalRecord {
    pub journal_id: String,
    pub user_id: String,
    pub client_id: String,
    pub attempt_id: String,
    pub parent_version_id: String,
    pub inbound_history_segment: Vec<u8>,
    pub merged_version_id: Option<String>,
    pub state: GatewayJournalState,
    pub recovery_status: GatewayRecoveryStatus,
    pub diagnostic_code: Option<String>,
    pub diagnostic_message: Option<String>,
    pub state_version: i64,
    pub created_at: String,
    pub updated_at: String,
    pub finalized_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MergedSyncJournalStateCount {
    pub state: GatewayJournalState,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePolicyRecord {
    pub user_id: String,
    pub desired_version: String,
    pub desired_policy: RuntimePolicy,
    pub applied_version: Option<String>,
    pub applied_policy: Option<RuntimePolicy>,
    pub applied_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewRecord {
    pub id: String,
    pub label: String,
    pub icon: String,
    pub filter: String,
    pub group_by: Option<String>,
    pub context_filtered: bool,
    /// Display mode: "list" (flat), "grouped" (grouped by project/tags)
    #[serde(default = "default_display_mode")]
    pub display_mode: String,
    pub sort_order: i32,
    /// "builtin" (seeded by server) or "user" (created via API)
    #[serde(default = "default_origin")]
    pub origin: String,
    /// True if user has customised a builtin view's filter/label/icon
    #[serde(default)]
    pub user_modified: bool,
    /// True if user explicitly deleted a builtin view (tombstone — prevents re-seeding)
    #[serde(default)]
    pub hidden: bool,
    /// Which default viewset version created/last updated this builtin view
    #[serde(default)]
    pub template_version: i32,
    /// Binds a context-filtered view to a specific ContextDefinition.
    /// When set, clients auto-apply the bound context's projectPrefixes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
}

fn default_display_mode() -> String {
    "list".to_string()
}

fn default_origin() -> String {
    "user".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRecord {
    pub id: String,
    pub label: String,
    /// JSON-encoded array of project prefixes
    pub project_prefixes: Vec<String>,
    pub color: Option<String>,
    pub icon: Option<String>,
    pub sort_order: i32,
    /// "builtin" (seeded by server) or "user" (created via API)
    #[serde(default = "default_origin")]
    pub origin: String,
    /// True if user has customised a builtin context
    #[serde(default)]
    pub user_modified: bool,
    /// True if user explicitly deleted a builtin context (tombstone — prevents re-seeding)
    #[serde(default)]
    pub hidden: bool,
    /// Which default contextset version created/last updated this builtin context
    #[serde(default)]
    pub template_version: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetRecord {
    pub id: String,
    pub label: String,
    pub raw_suffix: String,
    pub sort_order: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreRecord {
    pub id: String,
    pub label: String,
    pub tag: String,
    pub sort_order: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShoppingRecord {
    pub project: String,
    pub default_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeofenceRecord {
    pub id: String,
    pub label: String,
    pub latitude: f64,
    pub longitude: f64,
    pub radius: f64,
    #[serde(rename = "type")]
    pub geofence_type: String,
    pub context_id: Option<String>,
    pub view_id: Option<String>,
    pub store_tag: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericConfigRecord {
    pub version: Option<String>,
    pub items_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaRecord {
    /// UUID, also used as client_id for TC sync protocol
    pub id: String,
    pub user_id: String,
    /// Encryption secret encrypted with master key (base64-encoded ciphertext)
    pub encryption_secret_enc: String,
    pub label: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiTokenRecord {
    pub token_hash: String,
    pub user_id: String,
    pub label: Option<String>,
    pub token_id: Option<String>,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub first_used_at: Option<String>,
    pub last_used_at: Option<String>,
    pub last_used_ip: Option<String>,
}

/// Identity row for a label-filtered token lookup. Returned by
/// [`ConfigStore::lookup_token_correlation`] when the bearer token resolves
/// to a row whose `label` matches the caller-supplied filter. The store
/// has no domain knowledge of *which* label was matched — it just hands
/// back the row identity. Domain classification (e.g. "this is a
/// connect-config token") lives in the caller's service layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LabeledTokenCorrelation {
    pub user_id: String,
    pub token_id: String,
    pub credential_hash_prefix: String,
    pub expires_at: Option<String>,
    pub is_expired: bool,
}

/// Newly minted API token returned from
/// [`ConfigStore::create_labeled_api_token`]. The caller supplied the
/// `token_id` and `label` (single source of truth for connect-config:
/// `ConnectConfigService::issue`); the store generated the cryptographic
/// `token` material and wrote the row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssuedApiToken {
    pub token: String,
    pub token_id: String,
    pub credential_hash_prefix: String,
    pub expires_at: String,
}

/// Raw row returned by `ConfigStore::mark_token_used`. The store records
/// the use (sets `first_used_at` if NULL, updates `last_used_*`) and
/// returns label + identity so the service layer can classify the
/// outcome (label match → FirstUse / RepeatUse / NotConnectConfig).
///
/// The store does **not** know about the `"connect-config"` label
/// semantically; it just returns whatever label the row had. Single
/// source of truth for the label string lives at the service layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenUseRecord {
    pub user_id: String,
    pub token_id: String,
    pub label: Option<String>,
    pub credential_hash_prefix: String,
    pub expires_at: Option<String>,
    /// True iff `first_used_at` was NULL prior to this call (i.e. this
    /// invocation transitioned the row into the "used" state).
    pub was_first_use: bool,
}

/// A registered device (physical client that syncs via TC protocol).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// TC client_id (UUID) — identifies this device in sync requests
    pub client_id: String,
    pub user_id: String,
    pub name: String,
    /// Per-device encryption secret, encrypted with master key (base64).
    pub encryption_secret_enc: Option<String>,
    pub registered_at: String,
    pub last_sync_at: Option<String>,
    pub last_sync_ip: Option<String>,
    /// "active" or "revoked"
    pub status: String,
    pub bootstrap_request_id: Option<String>,
    pub bootstrap_status: Option<String>,
    pub bootstrap_requested_username: Option<String>,
    pub bootstrap_create_user_if_missing: Option<bool>,
    pub bootstrap_expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookRecord {
    pub id: String,
    pub user_id: String,
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub enabled: bool,
    pub consecutive_failures: u32,
    pub secret_enc: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminWebhookRecord {
    pub id: String,
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub enabled: bool,
    pub consecutive_failures: u32,
    pub secret_enc: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct NewWebhookRecord {
    pub id: String,
    pub user_id: String,
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub enabled: bool,
    pub secret_enc: String,
}

#[derive(Debug, Clone)]
pub struct NewAdminWebhookRecord {
    pub id: String,
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub enabled: bool,
    pub secret_enc: String,
}

#[derive(Debug, Clone)]
pub struct UpdateWebhookRecord {
    pub id: String,
    pub user_id: String,
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub enabled: bool,
    pub secret_enc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpdateAdminWebhookRecord {
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDeliveryRecord {
    pub delivery_id: String,
    pub webhook_id: String,
    pub event_id: String,
    pub event: String,
    pub timestamp: String,
    pub status: String,
    pub response_status: Option<u16>,
    pub attempt: u32,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebhookFailureState {
    pub consecutive_failures: u32,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebhookEventHistoryRecord {
    pub user_id: String,
    pub task_uuid: String,
    pub event_type: String,
    pub due_at: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebhookSyncSummary {
    pub tasks_changed: usize,
    pub created: usize,
    pub completed: usize,
    pub deleted: usize,
    pub modified: usize,
}

/// Outcome of `lookup_or_insert_idempotency_pending` per
/// `task-write-contract.md` § Server behaviour. Returned by the lookup
/// transaction to drive the handler's state machine:
///
/// - `FreshExecution`: no row existed (or the existing row was a stranded
///   `pending` past its timeout — removed in the same transaction).
///   Caller proceeds to Phase 2 (mutation) and Phase 3 (finalize).
/// - `Replay`: a `completed` row exists with matching fingerprint within
///   the retention window. Caller returns the stored response verbatim.
/// - `Conflict`: a row exists (any state) with a mismatched fingerprint.
///   Caller returns `409 IDEMPOTENCY_KEY_CONFLICT`.
/// - `InFlight`: a `pending` row exists within its timeout with matching
///   fingerprint. Caller returns `503 IDEMPOTENCY_IN_FLIGHT` with
///   `Retry-After`.
#[derive(Debug, Clone)]
pub enum IdempotencyLookupOutcome {
    FreshExecution {
        /// Server-generated UUID; must be passed back to Phase 3 to guard
        /// against stale-finalizer races (§ Server behaviour Phase 3).
        attempt_id: String,
    },
    Replay {
        status_code: u16,
        response_body: Vec<u8>,
        content_type: Option<String>,
        /// `Content-Length` from the original response. Hyper regenerates
        /// this from the body but the contract says headers are replayed
        /// verbatim, so we plumb the stored value through.
        content_length: Option<i64>,
    },
    Conflict,
    InFlight,
}
