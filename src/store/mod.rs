pub mod error;
pub mod maintenance;
pub mod models;
pub mod sqlite;

use async_trait::async_trait;
use models::*;

pub use error::{ConstraintKind, StoreError};
pub use maintenance::OperatorMaintenanceBackend;

/// Trait abstracting config database access.
///
/// Handlers depend on this trait, not a specific database implementation.
/// Currently backed by SQLite (`SqliteConfigStore`), designed to be
/// swappable to Postgres when scaling for commercial use.
#[async_trait]
pub trait ConfigStore: Send + Sync + 'static {
    // --- Users & Auth ---
    async fn get_user_by_token(&self, token: &str) -> anyhow::Result<Option<UserRecord>>;
    async fn get_user_by_id(&self, user_id: &str) -> anyhow::Result<Option<UserRecord>>;
    async fn get_user_by_username(&self, username: &str) -> anyhow::Result<Option<UserRecord>>;
    async fn list_users(&self) -> anyhow::Result<Vec<UserRecord>>;
    async fn create_user(&self, user: &NewUser) -> Result<UserRecord, StoreError>;
    async fn delete_user(&self, user_id: &str) -> anyhow::Result<bool>;
    async fn get_runtime_policy(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Option<RuntimePolicyRecord>>;
    async fn upsert_runtime_policy(
        &self,
        user_id: &str,
        desired_version: &str,
        desired_policy: &crate::runtime_policy::RuntimePolicy,
        applied_version: Option<&str>,
        applied_policy: Option<&crate::runtime_policy::RuntimePolicy>,
        applied_at: Option<&str>,
    ) -> anyhow::Result<RuntimePolicyRecord>;
    async fn create_api_token(&self, user_id: &str, label: Option<&str>) -> anyhow::Result<String>;
    async fn create_api_token_with_expiry(
        &self,
        user_id: &str,
        label: Option<&str>,
        expires_at: Option<&str>,
        token_bytes: usize,
    ) -> anyhow::Result<String>;
    /// Insert a fresh `api_tokens` row with caller-supplied `label` and
    /// `token_id`, mint cryptographic token material of `token_bytes`
    /// length, and return the issued token + identity. The store has no
    /// domain knowledge of label semantics — domains (e.g.
    /// `ConnectConfigService::issue`, which supplies
    /// `label = "connect-config"` and `token_id = "cc_<hex>"`) own that
    /// vocabulary and the token-id format.
    async fn create_labeled_api_token(
        &self,
        user_id: &str,
        label: &str,
        token_id: &str,
        expires_at: &str,
        token_bytes: usize,
    ) -> anyhow::Result<IssuedApiToken>;
    /// Resolve a bearer `token` against `api_tokens` filtered by
    /// `expected_label`. Returns the row's correlation identity when the
    /// label matches, or `None` when the token doesn't exist or its row's
    /// label is different. Used by the auth hot path to attach domain
    /// telemetry (e.g. connect-config redemption events) to incoming
    /// requests without baking the label into the storage layer.
    async fn lookup_token_correlation(
        &self,
        token: &str,
        expected_label: &str,
    ) -> anyhow::Result<Option<LabeledTokenCorrelation>>;
    /// Record a token use: stamp `first_used_at` if NULL, update
    /// `last_used_at` / `last_used_ip`, and return raw row identity so
    /// the caller can classify the outcome.
    ///
    /// `expected_label` filters the side-effect: only rows whose `label`
    /// matches the supplied value get the UPDATE. Other rows return
    /// `None` (read-only — the caller can treat them as "not my token").
    /// This preserves the pre-refactor behaviour where regular API
    /// tokens were not touched on the auth hot path; only the canonical
    /// label string moves into the caller (typically
    /// `ConnectConfigService::record_use`, which supplies
    /// `CONNECT_CONFIG_LABEL`).
    ///
    /// Returns `None` if no row matches the supplied bearer token, or
    /// if the row's label does not match `expected_label`.
    async fn mark_token_used(
        &self,
        token: &str,
        client_ip: &str,
        expected_label: &str,
    ) -> anyhow::Result<Option<TokenUseRecord>>;
    async fn list_api_tokens(&self, user_id: &str) -> anyhow::Result<Vec<ApiTokenRecord>>;
    async fn revoke_api_token(&self, token_hash: &str) -> anyhow::Result<bool>;

    // --- Views ---
    /// List visible views for a user (excludes hidden/tombstoned views).
    async fn list_views(&self, user_id: &str) -> anyhow::Result<Vec<ViewRecord>>;
    /// List ALL views including hidden tombstones (for reconciliation).
    async fn list_views_all(&self, user_id: &str) -> anyhow::Result<Vec<ViewRecord>>;
    async fn upsert_view(&self, user_id: &str, view: &ViewRecord) -> anyhow::Result<()>;
    async fn delete_view(&self, user_id: &str, id: &str) -> anyhow::Result<bool>;

    // --- Contexts ---
    /// List visible contexts for a user (excludes hidden/tombstoned contexts).
    async fn list_contexts(&self, user_id: &str) -> anyhow::Result<Vec<ContextRecord>>;
    /// List ALL contexts including hidden tombstones (for reconciliation).
    async fn list_contexts_all(&self, user_id: &str) -> anyhow::Result<Vec<ContextRecord>>;
    async fn upsert_context(&self, user_id: &str, ctx: &ContextRecord) -> anyhow::Result<()>;
    async fn delete_context(&self, user_id: &str, id: &str) -> anyhow::Result<bool>;

    // --- Presets ---
    async fn list_presets(&self, user_id: &str) -> anyhow::Result<Vec<PresetRecord>>;
    async fn upsert_preset(&self, user_id: &str, preset: &PresetRecord) -> anyhow::Result<()>;
    async fn delete_preset(&self, user_id: &str, id: &str) -> anyhow::Result<bool>;

    // --- Stores ---
    async fn list_stores(&self, user_id: &str) -> anyhow::Result<Vec<StoreRecord>>;
    async fn upsert_store(&self, user_id: &str, store: &StoreRecord) -> anyhow::Result<()>;
    async fn delete_store(&self, user_id: &str, id: &str) -> anyhow::Result<bool>;

    // --- Shopping Config ---
    async fn get_shopping_config(&self, user_id: &str) -> anyhow::Result<Option<ShoppingRecord>>;
    async fn upsert_shopping_config(
        &self,
        user_id: &str,
        config: &ShoppingRecord,
    ) -> anyhow::Result<()>;
    async fn delete_shopping_config(&self, user_id: &str) -> anyhow::Result<bool>;

    // --- Geofences ---
    async fn list_geofences(&self, user_id: &str) -> anyhow::Result<Vec<GeofenceRecord>>;
    async fn upsert_geofence(&self, user_id: &str, geofence: &GeofenceRecord)
        -> anyhow::Result<()>;
    async fn delete_geofence(&self, user_id: &str, id: &str) -> anyhow::Result<bool>;

    // --- Generic Config (backwards compat) ---
    async fn get_config(
        &self,
        user_id: &str,
        config_type: &str,
    ) -> anyhow::Result<Option<GenericConfigRecord>>;
    async fn upsert_config(
        &self,
        user_id: &str,
        config_type: &str,
        record: &GenericConfigRecord,
    ) -> anyhow::Result<()>;
    async fn delete_config_item(
        &self,
        user_id: &str,
        config_type: &str,
        item_id: &str,
    ) -> anyhow::Result<bool>;

    // --- Replicas (ADR-0001: per-user sync identity + key escrow) ---
    async fn create_replica(
        &self,
        user_id: &str,
        client_id: &str,
        encryption_secret_enc: &str,
    ) -> Result<(), StoreError>;
    async fn get_replica_by_user(&self, user_id: &str) -> anyhow::Result<Option<ReplicaRecord>>;
    async fn get_replica_by_client_id(
        &self,
        client_id: &str,
    ) -> anyhow::Result<Option<ReplicaRecord>>;
    /// Look up a user by their replica's client_id (used by TC sync auth).
    async fn get_user_by_client_id(&self, client_id: &str) -> anyhow::Result<Option<UserRecord>>;
    async fn delete_replica(&self, user_id: &str) -> anyhow::Result<bool>;

    // --- Devices (per-user client_id registry) ---
    async fn list_devices(&self, user_id: &str) -> anyhow::Result<Vec<DeviceRecord>>;
    async fn get_device(&self, client_id: &str) -> anyhow::Result<Option<DeviceRecord>>;
    async fn get_device_by_bootstrap_request(
        &self,
        bootstrap_request_id: &str,
    ) -> anyhow::Result<Option<DeviceRecord>>;
    async fn create_device(
        &self,
        user_id: &str,
        client_id: &str,
        name: &str,
        encryption_secret_enc: Option<&str>,
    ) -> anyhow::Result<()>;
    // TODO: collapse bootstrap params into a struct once the onboarding flow stabilises.
    #[allow(clippy::too_many_arguments)]
    async fn create_bootstrap_device(
        &self,
        user_id: &str,
        client_id: &str,
        name: &str,
        encryption_secret_enc: &str,
        bootstrap_request_id: &str,
        bootstrap_requested_username: Option<&str>,
        bootstrap_create_user_if_missing: bool,
        bootstrap_expires_at: &str,
    ) -> Result<(), StoreError>;
    async fn update_device_name(
        &self,
        user_id: &str,
        client_id: &str,
        name: &str,
    ) -> anyhow::Result<bool>;
    async fn revoke_device(&self, user_id: &str, client_id: &str) -> anyhow::Result<bool>;
    async fn unrevoke_device(&self, user_id: &str, client_id: &str) -> anyhow::Result<bool>;
    async fn delete_device(&self, user_id: &str, client_id: &str) -> anyhow::Result<bool>;
    async fn acknowledge_bootstrap_device(
        &self,
        bootstrap_request_id: &str,
    ) -> anyhow::Result<bool>;
    /// Update last_sync_at and last_sync_ip for a device (called on every successful sync).
    async fn touch_device(&self, client_id: &str, ip: &str) -> anyhow::Result<()>;

    // --- Webhooks ---
    async fn list_webhooks(&self, user_id: &str) -> anyhow::Result<Vec<WebhookRecord>>;
    async fn get_webhook(
        &self,
        user_id: &str,
        webhook_id: &str,
    ) -> anyhow::Result<Option<WebhookRecord>>;
    /// Create a webhook, enforcing the per-user cap atomically on the writer
    /// lane. `limit` is the caller's domain cap (`MAX_WEBHOOKS_PER_USER`) —
    /// passed in so the store never names a webhook-domain constant. Returns
    /// `StoreError::WebhookLimitReached` if the user already has `limit`
    /// webhooks. See #155.
    async fn create_webhook(
        &self,
        webhook: &NewWebhookRecord,
        limit: usize,
    ) -> Result<WebhookRecord, StoreError>;
    async fn update_webhook(
        &self,
        webhook: &UpdateWebhookRecord,
    ) -> Result<Option<WebhookRecord>, StoreError>;
    async fn delete_webhook(&self, user_id: &str, webhook_id: &str) -> anyhow::Result<bool>;
    async fn list_admin_webhooks(&self) -> anyhow::Result<Vec<AdminWebhookRecord>>;
    async fn get_admin_webhook(
        &self,
        webhook_id: &str,
    ) -> anyhow::Result<Option<AdminWebhookRecord>>;
    /// Create an admin webhook, enforcing the global admin-webhook cap
    /// atomically on the writer lane. `limit` is the caller's domain cap
    /// (`MAX_WEBHOOKS_PER_USER`, reused for the global admin set). Returns
    /// `StoreError::WebhookLimitReached` at the cap. See #155.
    async fn create_admin_webhook(
        &self,
        webhook: &NewAdminWebhookRecord,
        limit: usize,
    ) -> Result<AdminWebhookRecord, StoreError>;
    async fn update_admin_webhook(
        &self,
        webhook: &UpdateAdminWebhookRecord,
    ) -> Result<Option<AdminWebhookRecord>, StoreError>;
    async fn delete_admin_webhook(&self, webhook_id: &str) -> anyhow::Result<bool>;
    async fn list_webhook_deliveries(
        &self,
        user_id: &str,
        webhook_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<WebhookDeliveryRecord>>;
    async fn list_admin_webhook_deliveries(
        &self,
        webhook_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<WebhookDeliveryRecord>>;
    async fn record_webhook_delivery(&self, delivery: &WebhookDeliveryRecord)
        -> anyhow::Result<()>;
    async fn record_admin_webhook_delivery(
        &self,
        delivery: &WebhookDeliveryRecord,
    ) -> anyhow::Result<()>;
    async fn purge_webhook_deliveries_older_than(
        &self,
        retention_days: u32,
    ) -> anyhow::Result<usize>;
    async fn mark_webhook_delivery_succeeded(&self, webhook_id: &str) -> anyhow::Result<()>;
    async fn mark_admin_webhook_delivery_succeeded(&self, webhook_id: &str) -> anyhow::Result<()>;
    async fn mark_webhook_delivery_failed(
        &self,
        webhook_id: &str,
        disable_after: u32,
    ) -> anyhow::Result<Option<WebhookFailureState>>;
    async fn mark_admin_webhook_delivery_failed(
        &self,
        webhook_id: &str,
        disable_after: u32,
    ) -> anyhow::Result<Option<WebhookFailureState>>;
    async fn record_webhook_event_history(
        &self,
        user_id: &str,
        task_uuid: &str,
        event_type: &str,
        due_at: &str,
    ) -> anyhow::Result<bool>;
    async fn clear_webhook_event_history(
        &self,
        user_id: &str,
        task_uuid: &str,
    ) -> anyhow::Result<()>;

    // --- Idempotency-Key dedup records (task-write-contract.md § Idempotency) ---
    /// Phase 1 of the three-phase write-ahead pattern. Looks up an existing
    /// dedup record under (user_id, request_path, idempotency_key); if none
    /// exists (or the existing row is a stranded `pending` past
    /// `pending_timeout_seconds`, treated as expired and removed in the same
    /// transaction), inserts a fresh `pending` row with the supplied
    /// fingerprint and a server-generated attempt id.
    ///
    /// Returns the appropriate `IdempotencyLookupOutcome` variant per
    /// § Replay behaviour by record state. The lookup is serialised
    /// (`BEGIN IMMEDIATE` or equivalent) so concurrent retries do not both
    /// observe "no record" and both proceed to fresh execution.
    ///
    /// `now_unix_seconds` is the caller-supplied wall clock — passed in so
    /// tests can advance the clock deterministically rather than depending
    /// on `SystemTime::now()`.
    #[allow(clippy::too_many_arguments)]
    async fn lookup_or_insert_idempotency_pending(
        &self,
        user_id: &str,
        request_path: &str,
        idempotency_key: &str,
        body_fingerprint: &[u8; 32],
        pending_timeout_seconds: u32,
        completed_retention_hours: u32,
        now_unix_seconds: i64,
    ) -> anyhow::Result<IdempotencyLookupOutcome>;

    /// Phase 3 of the three-phase write-ahead pattern. Updates the dedup
    /// record from `pending` to `completed`, attaching the response payload.
    /// The update is **conditioned on `attempt_id`**: a stale Phase 3 from
    /// a superseded attempt finds no matching row and is silently discarded
    /// (returns `false`).
    ///
    /// Returns `Ok(true)` when exactly one row was updated, `Ok(false)`
    /// when zero (stale finalizer or row already evolved). Both are
    /// non-error outcomes; the caller does not retry on `false`.
    #[allow(clippy::too_many_arguments)]
    async fn finalize_idempotency_completed(
        &self,
        user_id: &str,
        request_path: &str,
        idempotency_key: &str,
        attempt_id: &str,
        status_code: u16,
        response_body: &[u8],
        content_type: Option<&str>,
    ) -> anyhow::Result<bool>;

    /// Roll back a `pending` row when Phase 2 failed with a known-no-commit
    /// outcome (validation rejection, business-rule error, error raised
    /// before any TC commit attempt was made). Conditioned on `attempt_id`
    /// and `state='pending'` so this cannot remove a `completed` row or
    /// a successor pending row from a fresh retry.
    ///
    /// Returns `Ok(true)` when exactly one row was deleted; `Ok(false)`
    /// otherwise. Both are non-error outcomes.
    ///
    /// **Must NOT be called on Phase 2 ambiguous-outcome failures** —
    /// those leave the row pending so the lookup-time expiry rule bounds
    /// the residual window.
    async fn rollback_idempotency_pending(
        &self,
        user_id: &str,
        request_path: &str,
        idempotency_key: &str,
        attempt_id: &str,
    ) -> anyhow::Result<bool>;

    /// Background-pruner: delete `completed` rows older than
    /// `retention_hours`. Called alongside the existing webhook delivery
    /// log purge. Returns the count of rows deleted.
    async fn prune_idempotency_completed(
        &self,
        retention_hours: u32,
        now_unix_seconds: i64,
    ) -> anyhow::Result<usize>;

    /// Background-reaper: delete stranded `pending` rows older than
    /// `pending_timeout_seconds`. Operational hygiene only — the
    /// lookup-time expiry rule already treats these as expired
    /// regardless of reaper status. Returns the count of rows deleted.
    async fn prune_idempotency_stranded_pending(
        &self,
        pending_timeout_seconds: u32,
        now_unix_seconds: i64,
    ) -> anyhow::Result<usize>;

    // --- Task Keys (server#130, task-write-contract.md § Task Keys) ---
    /// Two-phase allocation start. Under `BEGIN IMMEDIATE`, computes
    /// `MAX(n)+1` over **all states** for `(user_id, prefix)` (burned rows
    /// included so rollback gaps cannot be reused) and inserts a row with
    /// `state='pending', task_uuid=NULL`. Returns `(n, attempt_id)` where
    /// `attempt_id` is a server-generated UUID used to guard
    /// `attach_task_uuid_to_pending` / `commit_task_key` / `burn_task_key`
    /// against stale finalisers (see CLAUDE.md § Idempotency-Key §
    /// Attempt-id guards Phase 3).
    async fn reserve_task_key_pending(
        &self,
        user_id: &str,
        prefix: &str,
    ) -> Result<(i64, String), StoreError>;

    /// Single-write allocation start: reserves `MAX(n)+1` and attaches
    /// `task_uuid` in one config-DB transaction so recovery never sees an
    /// unbound reservation for an already-accepted create. Used by BOTH the
    /// merged-sync gateway inbound path and the REST `service::add_task` create
    /// path (the latter since #148 — collapses the former reserve+attach into
    /// one write).
    async fn reserve_task_key_pending_for_uuid(
        &self,
        user_id: &str,
        prefix: &str,
        task_uuid: &str,
    ) -> Result<(i64, String), StoreError>;

    /// Two-phase allocation midpoint. Sets `task_uuid` on the previously
    /// reserved pending row (state stays `pending`). Must run BEFORE TC
    /// commit so the row is recoverable by the reaper / Phase 5 drift
    /// recovery if `commit_task_key` later fails. Asserts rows-affected
    /// == 1; mismatch returns `AllocationStaleFinalizer`.
    async fn attach_task_uuid_to_pending(
        &self,
        user_id: &str,
        prefix: &str,
        n: i64,
        attempt_id: &str,
        task_uuid: &str,
    ) -> Result<(), StoreError>;

    /// Two-phase allocation commit. Transitions state `pending → committed`
    /// (UPDATE conditioned on `state='pending' AND attempt_id=? AND
    /// task_uuid IS NOT NULL`). Idempotent: if the row is already
    /// `committed` with the same `attempt_id`, returns Ok — this is
    /// load-bearing for the reaper-race regression test (the reaper may
    /// finalise pending rows during the create's TC commit window; the
    /// resumed create's call sees the already-committed row and treats it
    /// as success).
    async fn commit_task_key(
        &self,
        user_id: &str,
        prefix: &str,
        n: i64,
        attempt_id: &str,
    ) -> Result<(), StoreError>;

    /// Burn a pending row (state `pending → burned`). Used by rollback
    /// paths and the reaper. Idempotent if already burned with the same
    /// attempt_id. Burned rows persist forever — the next reservation's
    /// `MAX(n)` cannot reuse this N.
    async fn burn_task_key(
        &self,
        user_id: &str,
        prefix: &str,
        n: i64,
        attempt_id: &str,
    ) -> Result<(), StoreError>;

    /// DB primitive for the reaper coordinator (lives in
    /// `src/task_keys/reaper.rs` — coordination requires `AppState` for
    /// per-user mutation locks + TC scan, hence the split). Returns
    /// candidate rows whose `created_at < now - pending_timeout_seconds`
    /// up to `batch_limit`, sorted by `user_id` so the coordinator can
    /// group efficiently.
    async fn select_stale_pending_task_keys(
        &self,
        now_unix_seconds: i64,
        pending_timeout_seconds: u32,
        batch_limit: usize,
    ) -> Result<Vec<StalePendingCandidate>, StoreError>;

    /// Read the user's prefix (None until backfill / signup assigns one).
    async fn get_user_prefix(&self, user_id: &str) -> Result<Option<String>, StoreError>;

    /// Set the user's prefix. Rejects with `StoreError::PrefixLocked` if
    /// **either** any allocation row exists for this user (any state)
    /// OR `users.task_keys_migrated_at IS NOT NULL` — the
    /// pre-allocation-only mutability rule per § Set-prefix immutability.
    /// Rejects with `StoreError::Constraint(Unique { USERS_PREFIX })` if
    /// the prefix is taken by another user.
    async fn set_user_prefix(&self, user_id: &str, prefix: &str) -> Result<(), StoreError>;

    /// List users with a NULL prefix — used by the startup-routine
    /// backfill (C4) to enumerate work.
    async fn users_without_prefix(&self) -> Result<Vec<UserWithoutPrefix>, StoreError>;

    /// Return the Runtime User's active Personal Task Scope, if already
    /// materialised. S1 transition read model: every prefixed user should have
    /// exactly one row, enforced by `idx_task_scopes_personal_owner`.
    async fn get_personal_task_scope_for_user(
        &self,
        user_id: &str,
    ) -> Result<Option<TaskScopeRecord>, StoreError>;

    /// Resolve a key prefix to a Task Scope visible to the Runtime User.
    /// S3 personal-only implementation returns the user's active Personal
    /// Task Scope when `prefix` matches its `key_prefix`; future Team Task
    /// Scope support extends this method with local membership checks.
    async fn lookup_task_scope_by_prefix_for_user(
        &self,
        user_id: &str,
        prefix: &str,
    ) -> Result<Option<TaskScopeRecord>, StoreError>;

    /// Idempotently materialise the Runtime User's active Personal Task Scope
    /// from `users.prefix`. Requires the prefix to exist; callers must run the
    /// prefix backfill/allocation routine first. Returns whether this call
    /// inserted the row or found an existing one. Concurrency safety comes from
    /// the task_scopes unique indexes, not from process-local sequencing; owner
    /// races re-read and return the existing row.
    async fn ensure_personal_task_scope_for_user(
        &self,
        user_id: &str,
    ) -> Result<PersonalTaskScopeEnsure, StoreError>;

    /// List prefixed Runtime Users that do not yet have an active Personal
    /// Task Scope. Used by startup backfill; excludes NULL-prefix users so the
    /// prefix-backfill ordering remains explicit.
    async fn list_users_pending_personal_task_scope(
        &self,
    ) -> Result<Vec<UserMissingPersonalTaskScope>, StoreError>;

    /// S2 repair primitive: stamp legacy allocation rows with the active
    /// Personal Task Scope resolved by `(user_id, prefix)`. Applies to all
    /// states, including `burned`, because burned rows are part of the
    /// no-reuse counter history. Idempotent; returns rows changed this pass.
    async fn backfill_task_key_allocation_task_scope_ids(&self) -> Result<usize, StoreError>;

    /// Readiness invariant for S2: after migrations + prefix backfill +
    /// Personal Task Scope backfill + allocation-scope backfill, this should
    /// be zero before S3 routes lookups by Task Scope.
    async fn count_task_key_allocations_missing_task_scope_id(&self) -> Result<usize, StoreError>;

    /// S3 Task Scope-routed key resolution. Looks up the canonical UUID for
    /// `(task_scope_id, n)`. `WHERE state='committed'` only; pending and
    /// burned rows do NOT resolve. Returns None on miss.
    async fn lookup_task_uuid_by_task_scope_key(
        &self,
        task_scope_id: &str,
        n: i64,
    ) -> Result<Option<String>, StoreError>;

    /// Compatibility wrapper for current REST key resolution. Resolves
    /// `(user_id, prefix)` to a visible active Task Scope, gates on the S2
    /// `task_scope_id` readiness invariant for this Runtime User, then calls
    /// `lookup_task_uuid_by_task_scope_key` with `(task_scope_id, n)`.
    async fn lookup_task_uuid_by_key(
        &self,
        user_id: &str,
        prefix: &str,
        n: i64,
    ) -> Result<Option<String>, StoreError>;

    /// Phase 2 ambiguous recovery + Phase 5 drift recovery primitive.
    /// Looks up any allocation row by task_uuid (across all states except
    /// burned; burned rows have `task_uuid` either NULL or stale — caller
    /// shouldn't see them).
    async fn lookup_task_key_by_uuid(
        &self,
        user_id: &str,
        task_uuid: &str,
    ) -> Result<Option<(String, KeyState)>, StoreError>;

    /// Phase 2 batch projection primitive — list/read endpoints call this
    /// once per request and pass the resulting map into `task_to_item`.
    /// Returns `<PREFIX>-<n>` for each `task_uuid` that has a `committed`
    /// allocation row. Chunks internally to stay within
    /// SQLITE_MAX_VARIABLE_NUMBER (compile-time constant).
    async fn lookup_task_keys_by_uuids(
        &self,
        user_id: &str,
        task_uuids: &[String],
    ) -> Result<std::collections::HashMap<String, String>, StoreError>;

    /// S3 Task Scope-routed REST projection primitive. Returns the projected
    /// key per `task_uuid` for rows in one visible Task Scope that REST should
    /// expose:
    ///
    /// - `committed` rows always.
    /// - `pending` rows whose `created_at` is within the
    ///   `pending_timeout_seconds` window (lookup-time expiry rule per
    ///   `task-write-contract.md` § REST projects from the allocation table).
    ///
    /// `burned` rows are filtered at the SQL layer. The logical namespace is
    /// `(task_scope_id, n)` rather than `user_id`.
    async fn lookup_task_keys_for_projection_by_task_scope(
        &self,
        task_scope_id: &str,
        task_uuids: &[String],
        now_unix_seconds: i64,
        pending_timeout_seconds: u32,
    ) -> Result<std::collections::HashMap<String, String>, StoreError>;

    /// Compatibility wrapper for current REST projection. Gates on the S2
    /// `task_scope_id` readiness invariant for this Runtime User, resolves the
    /// active Personal Task Scope, then calls
    /// `lookup_task_keys_for_projection_by_task_scope`.
    async fn lookup_task_keys_for_projection(
        &self,
        user_id: &str,
        task_uuids: &[String],
        now_unix_seconds: i64,
        pending_timeout_seconds: u32,
    ) -> Result<std::collections::HashMap<String, String>, StoreError>;

    /// Phase 5b sync-bridge drift-recovery primitive. Returns every
    /// non-burned allocation row whose `task_uuid` is in `task_uuids`,
    /// carrying `(task_uuid, key, state, attempt_id, created_at)`.
    /// Distinct from `lookup_task_keys_by_uuids`, which is the projection
    /// path (committed-only, key-only return). The drift path needs the
    /// full row metadata to decide between `value_mismatch`,
    /// `post_commit_finalize`, and `pending_with_drift` per
    /// `task-write-contract.md` § Drift recovery.
    ///
    /// Chunks internally to stay within `SQLITE_MAX_VARIABLE_NUMBER`,
    /// mirroring `lookup_task_keys_by_uuids`. Burned rows are filtered
    /// out at the SQL level. Caller applies pending-expiry comparison on
    /// `created_at` if it cares — store layer just returns whatever is
    /// physically present.
    async fn lookup_task_keys_for_drift(
        &self,
        user_id: &str,
        task_uuids: &[String],
    ) -> Result<Vec<crate::store::models::DriftAllocationRow>, StoreError>;

    /// Phase 4 backfill primitive — read the migration timestamp.
    /// Returns `Some(iso8601)` once `mark_user_task_keys_migrated` has
    /// run, `None` otherwise. Backfill enters via the
    /// `RuntimeRecoveryCoordinator`'s in-memory cache; this DB call is
    /// the cache-miss path + the post-lock double-check.
    async fn get_user_task_keys_migrated_at(
        &self,
        user_id: &str,
    ) -> Result<Option<String>, StoreError>;

    /// Phase 4 backfill commit. Sets `users.task_keys_migrated_at` to
    /// the current UTC time. Idempotent: safe to call again on an
    /// already-migrated user (the column is monotonically populated;
    /// re-marking is a no-op overwrite with the new timestamp, which
    /// the backfill flow does not exercise but the DB tolerates).
    async fn mark_user_task_keys_migrated(&self, user_id: &str) -> Result<(), StoreError>;

    /// Phase 4 backfill helper — read `MAX(n)` over **all states** for
    /// `(user_id, prefix)`. Returns `0` when the user has no allocation
    /// rows yet. Phase B uses this to compute the canonical key per task
    /// before Phase A+C inserts the rows; both phases run under the
    /// per-user mutation lock so the value is stable across the call
    /// boundary (`add_task` takes the same lock; the reaper takes the
    /// same lock).
    async fn max_n_for_user_prefix(&self, user_id: &str, prefix: &str) -> Result<i64, StoreError>;

    /// Phase 4 atomic backfill commit. In ONE `BEGIN IMMEDIATE` config-DB
    /// transaction:
    ///   1. Verify the user row still exists and read the current
    ///      `users.prefix`. Rejects with `BackfillUserMissing` on a
    ///      concurrent `delete_user`.
    ///   2. Verify the current `users.prefix` equals the `prefix`
    ///      argument (rejects with `BackfillPrefixChanged` if a
    ///      concurrent admin `set-prefix` raced between Phase B's
    ///      precompute and this commit).
    ///   3. Verify the in-transaction `MAX(n)` over all states for
    ///      `(user_id, prefix)` matches `expected_max_n` (rejects with
    ///      `BackfillMaxChanged` on a multi-process / admin-restore race
    ///      that bumped `n` between Phase B and this commit).
    ///   4. INSERT one row per `task_uuid` as `state='committed'`,
    ///      `task_uuid` set, fresh `attempt_id` per row, `n` running
    ///      from `expected_max_n + 1` upward in input order, and
    ///      `committed_at = strftime(...)`.
    ///   5. UPDATE `users.task_keys_migrated_at = strftime(...)`.
    ///
    /// Returns `(task_uuid, n)` pairs in input order. The caller must
    /// hold the per-user mutation lock and must filter out task_uuids
    /// that already have a non-burned allocation row — duplicate
    /// `task_uuid` triggers the `idx_task_key_allocations_uuid` UNIQUE
    /// constraint and the whole transaction rolls back, leaving
    /// `task_keys_migrated_at` unchanged so the next backfill retry runs
    /// from a clean slate. The Phase B UDA writes that precede this
    /// commit are idempotent and safe to retry.
    async fn commit_backfill_allocations_for_user(
        &self,
        user_id: &str,
        prefix: &str,
        expected_max_n: i64,
        task_uuids_in_order: &[String],
    ) -> Result<Vec<(String, i64)>, StoreError>;

    /// Phase 4 backfill helper — list every `state='pending'` row for
    /// `user_id` whose `task_uuid` is non-NULL (i.e. an in-flight or
    /// crashed `add_task` reservation that already attached its UUID
    /// before TC commit). Returns `(prefix, n, attempt_id, task_uuid)`
    /// rows. Used by the backfill flow to reconcile pending-attached
    /// rows under the per-user mutation lock so they don't collide with
    /// the atomic Phase A+C `INSERT` via `idx_task_key_allocations_uuid`.
    async fn list_pending_attached_task_keys_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<PendingAttachedKey>, StoreError>;

    // --- Merged sync gateway journal (server#143) ---
    async fn create_merged_sync_journal_attempt(
        &self,
        attempt: &NewMergedSyncJournalAttempt,
    ) -> anyhow::Result<MergedSyncJournalRecord>;

    /// Forward-only compare-and-swap transition. The store updates only when
    /// `journal_id`, `attempt_id`, and `from_state` all match. A `None` return
    /// means a stale finalizer/recovery attempt or illegal current state lost
    /// the race and no row was overwritten.
    async fn transition_merged_sync_journal(
        &self,
        transition: MergedSyncJournalTransition<'_>,
    ) -> anyhow::Result<Option<MergedSyncJournalRecord>>;

    async fn get_merged_sync_journal(
        &self,
        journal_id: &str,
    ) -> anyhow::Result<Option<MergedSyncJournalRecord>>;

    async fn list_merged_sync_journal_for_user(
        &self,
        user_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MergedSyncJournalRecord>>;

    /// Recovery scan primitive: returns only non-terminal rows, oldest first,
    /// so newer finalized/failed history cannot hide older recoverable work
    /// behind an operator/admin display limit.
    async fn list_nonterminal_merged_sync_journal_for_user(
        &self,
        user_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MergedSyncJournalRecord>>;

    async fn count_merged_sync_journal_states_for_user(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Vec<MergedSyncJournalStateCount>>;

    // --- Migrations ---
    async fn run_migrations(&self) -> anyhow::Result<()>;
}
