//! Typed errors from `ConfigStore` operations.
//!
//! Replaces caller-side substring matching on raw SQLite error text (which
//! couples domain code to SQLite's specific error message format). See
//! ADR-0002 § P4 sub-fix 3.
//!
//! Methods that surface domain-meaningful constraint violations (e.g.
//! "duplicate webhook URL", "username already taken") return
//! `Result<T, StoreError>`. Backends inspect their native error and produce
//! `StoreError::Constraint(ConstraintKind::Unique { resource })` with a
//! stable resource label; callers `match` on the label.
//!
//! Resource label scheme: dot-separated `<table>.<column-or-index>`,
//! mirroring the SQLite UNIQUE-constraint error format. Composite indexes
//! (e.g. `(user_id, url)` on `webhooks`) get a single combined label
//! (`webhooks.user_url`).

use std::fmt;

/// Internal typed error used inside SQLite `conn.call` closures when an
/// allocation cannot resolve the active Personal Task Scope. The backend maps
/// this through to `StoreError::AllocationTaskScopeMissing` after preserving
/// the error chain.
#[derive(Debug, Clone)]
pub struct MissingTaskScopeForAllocationError {
    pub user_id: String,
    pub prefix: String,
}

impl fmt::Display for MissingTaskScopeForAllocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no active Personal Task Scope for user_id={} prefix={}; refusing task-key allocation",
            self.user_id, self.prefix
        )
    }
}

impl std::error::Error for MissingTaskScopeForAllocationError {}

/// Internal typed error returned from inside the `create_webhook` /
/// `create_admin_webhook` writer closure when the per-user (resp. global)
/// webhook count is already at the configured cap. The backend maps this
/// through to `StoreError::WebhookLimitReached` after preserving the error
/// chain (mirrors [`MissingTaskScopeForAllocationError`]). The domain limit
/// constant stays in `webhooks::api`; only the numeric `limit` reaches the
/// store, so the store layer never names a webhook-domain constant.
#[derive(Debug, Clone)]
pub struct WebhookLimitReachedError {
    pub limit: usize,
}

impl fmt::Display for WebhookLimitReachedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "webhook limit reached ({} already registered); refusing create",
            self.limit
        )
    }
}

impl std::error::Error for WebhookLimitReachedError {}

/// Errors from `ConfigStore` operations whose call sites need to introspect
/// the failure mode (vs just propagating it). Other errors stay in
/// `anyhow::Error` until a future migration.
#[derive(Debug)]
pub enum StoreError {
    /// A schema constraint fired. Inspect `kind` for the specific class.
    Constraint(ConstraintKind),
    /// `set_user_prefix` rejected: the prefix is locked because
    /// `task_key_allocations` already has rows for this user (any state)
    /// OR `users.task_keys_migrated_at IS NOT NULL`. Returned as
    /// `409 PREFIX_LOCKED` plain-text body. See `task-write-contract.md`
    /// § Set-prefix immutability.
    PrefixLocked,
    /// Allocation tried to reserve a key under `(user_id, prefix)` but S2
    /// could not resolve that compatibility namespace to the user's active
    /// Personal Task Scope. Callers must repair scope materialisation; the
    /// store deliberately did not insert a NULL `task_scope_id` row.
    AllocationTaskScopeMissing { user_id: String, prefix: String },
    /// S3 lookup/projection refused to route by Task Scope because relevant
    /// allocation rows still have NULL `task_scope_id`. Startup/backfill must
    /// repair the S2 invariant before Task Scope-routed reads are safe.
    MissingTaskScopeId { user_id: String, count: usize },
    /// `commit_task_key` / `burn_task_key` / `attach_task_uuid_to_pending`
    /// found no row matching the (user, prefix, n, attempt_id, expected
    /// state) tuple — either the row was already finalised by a different
    /// attempt (the reaper or a stale finalizer), or the row never
    /// existed. The mutation is silently dropped at the call site; the
    /// reaper-race regression test asserts this is benign.
    AllocationStaleFinalizer,
    /// Phase 4 backfill: `commit_backfill_allocations_for_user` observed
    /// the user row missing inside the commit transaction. A concurrent
    /// `delete_user` ran between the lock acquire and the commit. The
    /// caller drops the in-flight backfill; the reaper / next access
    /// will re-evaluate cleanly post-eviction.
    BackfillUserMissing,
    /// Phase 4 backfill: `commit_backfill_allocations_for_user` observed
    /// `MAX(n)` shifted between the Phase B precompute and the Phase A+C
    /// commit. Single-process deployments under the per-user mutation
    /// lock cannot trip this — surfaces the multi-process / admin-restore
    /// race as an explicit failure rather than silently committing
    /// out-of-canonical keys.
    BackfillMaxChanged { expected: i64, actual: i64 },
    /// Phase 4 backfill: `commit_backfill_allocations_for_user` observed
    /// `users.prefix` shifted between the Phase B precompute and the
    /// Phase A+C commit. A concurrent admin `set-prefix` ran in the
    /// gap between the backfill's prefix read and the commit
    /// transaction. The commit aborts; the next backfill access reads
    /// the new prefix and re-runs Phase B + Phase A+C cleanly.
    BackfillPrefixChanged {
        expected: String,
        actual: Option<String>,
    },
    /// `create_webhook` / `create_admin_webhook` refused: the user (resp.
    /// global) webhook count is already at the cap passed by the caller. The
    /// count-and-insert run atomically on the single writer lane, so this is
    /// the authoritative cap enforcement (the former handler-side
    /// count-then-insert precheck had a TOCTOU). Handlers map this to
    /// `WebhookApiError::LimitReached` (422 LIMIT_REACHED). `limit` echoes the
    /// configured cap. See #155.
    WebhookLimitReached { limit: usize },
    /// Anything else — implementation/transport failure, IO, etc.
    Other(anyhow::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintKind {
    /// A UNIQUE index/constraint fired. `resource` is a stable label
    /// identifying the index (see module docs for the scheme). Callers
    /// match on the literal string.
    Unique { resource: &'static str },
    /// Any other constraint class (foreign key, NOT NULL, CHECK).
    /// Currently no caller distinguishes these; kept generic to avoid
    /// growing variants for unmapped cases.
    Other,
}

/// Stable resource labels for the constraints inspected by today's callers.
/// Adding a new label here is the one-line edit a future caller needs;
/// mapping logic lives in the backend.
pub mod resources {
    pub const USERS_USERNAME: &str = "users.username";
    pub const USERS_PREFIX: &str = "users.prefix";
    /// Partial unique index `idx_task_scopes_personal_owner` on active
    /// Personal Task Scope owner. SQLite reports the indexed column list, not
    /// the index name, so the stable label is `task_scopes.owner_runtime_user_id`.
    /// Fires when concurrent ensure paths both try to materialise a Personal
    /// Task Scope for the same Runtime User.
    /// TODO(team scopes): if another partial unique index ever uses this same
    /// column list, distinguish by operation context because SQLite does not
    /// expose the partial index name in the constraint message.
    pub const TASK_SCOPES_PERSONAL_OWNER: &str = "task_scopes.owner_runtime_user_id";
    /// Partial unique index `idx_task_scopes_key_prefix_active` on active Task
    /// Scope prefix. SQLite reports the indexed column list, not the index
    /// name, so the stable label is `task_scopes.key_prefix`. Fires if a scope
    /// for another owner already claims the prefix resolved from `users.prefix`.
    /// TODO(team scopes): if another partial unique index ever uses this same
    /// column list, distinguish by operation context because SQLite does not
    /// expose the partial index name in the constraint message.
    pub const TASK_SCOPES_KEY_PREFIX: &str = "task_scopes.key_prefix";
    pub const REPLICAS_USER_ID: &str = "replicas.user_id";
    pub const DEVICES_BOOTSTRAP_REQUEST_ID: &str = "devices.bootstrap_request_id";
    pub const WEBHOOKS_USER_URL: &str = "webhooks.user_url";
    pub const ADMIN_WEBHOOKS_URL: &str = "admin_webhooks.url";
    /// Composite primary key on `task_key_allocations(user_id, prefix, n)`.
    /// Fires from `reserve_task_key_pending` only as a defence-in-depth
    /// guard — the reservation path computes `MAX(n)+1` under
    /// `BEGIN IMMEDIATE`, so a collision here would indicate a serious
    /// invariant violation (concurrent reservation under a non-immediate
    /// transaction or missing `BEGIN IMMEDIATE`).
    pub const TASK_KEY_ALLOCATIONS_USER_PREFIX_N: &str = "task_key_allocations.user_id_prefix_n";
    /// Partial unique index on `task_key_allocations.task_uuid`. Fires from
    /// `attach_task_uuid_to_pending` if a task UUID is double-attached
    /// (two pending rows trying to claim the same TC UUID).
    pub const TASK_KEY_ALLOCATIONS_TASK_UUID: &str = "task_key_allocations.task_uuid";
    /// S2 Task Scope namespace uniqueness on `(task_scope_id, n)`. During
    /// dual-write this should never fire because compatibility paths still
    /// compute `MAX(n)+1` under `BEGIN IMMEDIATE`; a hit indicates namespace
    /// drift between legacy and Task Scope counters.
    pub const TASK_KEY_ALLOCATIONS_TASK_SCOPE_N: &str = "task_key_allocations.task_scope_n";
}

impl StoreError {
    /// Convenience: build a unique-constraint error for the given resource.
    pub fn unique(resource: &'static str) -> Self {
        StoreError::Constraint(ConstraintKind::Unique { resource })
    }

    /// True iff this is a unique-constraint violation on the named
    /// resource. Useful for `if let` plus message-free checks.
    pub fn is_unique(&self, resource: &str) -> bool {
        matches!(
            self,
            StoreError::Constraint(ConstraintKind::Unique { resource: r }) if *r == resource
        )
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Constraint(ConstraintKind::Unique { resource }) => {
                write!(f, "unique constraint violation on {resource}")
            }
            StoreError::Constraint(ConstraintKind::Other) => write!(f, "constraint violation"),
            StoreError::PrefixLocked => write!(f, "prefix is locked (allocations exist)"),
            StoreError::AllocationTaskScopeMissing { user_id, prefix } => write!(
                f,
                "no active Personal Task Scope for user_id={user_id} prefix={prefix}; refusing task-key allocation"
            ),
            StoreError::MissingTaskScopeId { user_id, count } => write!(
                f,
                "MISSING_TASK_SCOPE_ID: {count} task-key allocation row(s) for user_id={user_id} still have NULL task_scope_id"
            ),
            StoreError::AllocationStaleFinalizer => {
                write!(
                    f,
                    "task-key allocation finaliser raced or has wrong attempt"
                )
            }
            StoreError::BackfillUserMissing => {
                write!(
                    f,
                    "task-key backfill: user row was deleted between lock acquire and commit"
                )
            }
            StoreError::BackfillMaxChanged { expected, actual } => {
                write!(
                    f,
                    "task-key backfill: MAX(n) shifted from {expected} to {actual} between \
                     Phase B precompute and Phase A+C commit"
                )
            }
            StoreError::BackfillPrefixChanged { expected, actual } => {
                write!(
                    f,
                    "task-key backfill: users.prefix shifted from {expected:?} to {actual:?} \
                     between Phase B precompute and Phase A+C commit"
                )
            }
            StoreError::WebhookLimitReached { limit } => {
                write!(f, "webhook limit reached (cap {limit})")
            }
            StoreError::Other(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // anyhow::Error already chains; expose its source for parity
            // with non-typed errors elsewhere.
            StoreError::Other(err) => err.source(),
            _ => None,
        }
    }
}

impl From<anyhow::Error> for StoreError {
    fn from(err: anyhow::Error) -> Self {
        StoreError::Other(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_helper_round_trips() {
        let err = StoreError::unique(resources::USERS_USERNAME);
        assert!(err.is_unique(resources::USERS_USERNAME));
        assert!(!err.is_unique(resources::WEBHOOKS_USER_URL));
    }

    #[test]
    fn display_includes_resource_label() {
        let err = StoreError::unique(resources::WEBHOOKS_USER_URL);
        assert!(err.to_string().contains("webhooks.user_url"));
    }

    #[test]
    fn other_wraps_anyhow() {
        let err: StoreError = anyhow::anyhow!("kaboom").into();
        assert!(matches!(err, StoreError::Other(_)));
        assert_eq!(err.to_string(), "kaboom");
    }
}
