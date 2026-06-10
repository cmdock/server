use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio_rusqlite::Connection;

use super::models::*;
use super::ConfigStore;

#[path = "sqlite/auth.rs"]
mod auth;
#[path = "sqlite/config.rs"]
mod config;
#[path = "sqlite/devices.rs"]
mod devices;
#[path = "sqlite/maintenance.rs"]
mod maintenance;
#[path = "sqlite/runtime.rs"]
mod runtime;
#[path = "sqlite/task_keys.rs"]
mod task_keys;
#[path = "sqlite/task_scopes.rs"]
mod task_scopes;
#[path = "sqlite/webhooks.rs"]
mod webhooks;

mod idempotency;
mod merged_sync_journal;

/// Hash a bearer token for storage/lookup (tokens are never stored in plaintext)
pub(super) fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub(super) fn delete_user_owned_rows(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    user_id: &str,
) -> Result<(), rusqlite::Error> {
    let result = match table {
        "devices" => tx.execute("DELETE FROM devices WHERE user_id = ?1", [&user_id]),
        "api_tokens" => tx.execute("DELETE FROM api_tokens WHERE user_id = ?1", [&user_id]),
        "user_runtime_policies" => tx.execute(
            "DELETE FROM user_runtime_policies WHERE user_id = ?1",
            [&user_id],
        ),
        "views" => tx.execute("DELETE FROM views WHERE user_id = ?1", [&user_id]),
        "contexts" => tx.execute("DELETE FROM contexts WHERE user_id = ?1", [&user_id]),
        "presets" => tx.execute("DELETE FROM presets WHERE user_id = ?1", [&user_id]),
        "stores" => tx.execute("DELETE FROM stores WHERE user_id = ?1", [&user_id]),
        "replicas" => tx.execute("DELETE FROM replicas WHERE user_id = ?1", [&user_id]),
        "sync_clients" => tx.execute("DELETE FROM sync_clients WHERE user_id = ?1", [&user_id]),
        "shopping_config" => {
            tx.execute("DELETE FROM shopping_config WHERE user_id = ?1", [&user_id])
        }
        "config" => tx.execute("DELETE FROM config WHERE user_id = ?1", [&user_id]),
        "webhooks" => tx.execute("DELETE FROM webhooks WHERE user_id = ?1", [&user_id]),
        "merged_sync_journal" => tx.execute(
            "DELETE FROM merged_sync_journal WHERE user_id = ?1",
            [&user_id],
        ),
        // The FK also has ON DELETE CASCADE; delete explicitly so user cleanup
        // remains correct in legacy/inline schemas where FK enforcement or the
        // table shape may differ.
        "task_scopes" => tx.execute(
            "DELETE FROM task_scopes WHERE owner_runtime_user_id = ?1",
            [&user_id],
        ),
        _ => unreachable!("delete_user_owned_rows only accepts internal allowlisted tables"),
    };

    match result {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(_, Some(ref msg))) if msg.contains("no such table") => {
            Ok(())
        }
        Err(err) => Err(err),
    }
}

pub(super) type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// Convert tokio_rusqlite::Error<BoxErr> to anyhow::Error, preserving the
/// underlying error chain so callers can downcast to `rusqlite::Error` via
/// `anyhow::Error::chain()`.
pub(super) fn map_err(e: tokio_rusqlite::Error<BoxErr>) -> anyhow::Error {
    match e {
        tokio_rusqlite::Error::ConnectionClosed => anyhow::anyhow!("Connection closed"),
        tokio_rusqlite::Error::Close((_, e)) => anyhow::anyhow!("Close error: {e}"),
        tokio_rusqlite::Error::Error(box_err) => anyhow_from_box(box_err),
        _ => anyhow::anyhow!("Unknown tokio-rusqlite error"),
    }
}

/// Wrap a `Box<dyn Error + Send + Sync>` into an `anyhow::Error` while
/// preserving the original error as the `source()` chain — required so
/// callers can downcast through to e.g. `rusqlite::Error`. We can't pass
/// the box directly to `anyhow::Error::from`/`new` because anyhow's blanket
/// impls require `E: Sized + Error`, and trait-object boxes don't satisfy
/// that signature even though `Box<T>` itself is sized.
fn anyhow_from_box(box_err: BoxErr) -> anyhow::Error {
    struct BoxedSource(BoxErr);
    impl std::fmt::Debug for BoxedSource {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Debug::fmt(&self.0, f)
        }
    }
    impl std::fmt::Display for BoxedSource {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Display::fmt(&self.0, f)
        }
    }
    impl std::error::Error for BoxedSource {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            // Walk INTO the boxed error's chain so downcasts of
            // `anyhow::Error::chain()` find e.g. `rusqlite::Error`.
            Some(self.0.as_ref())
        }
    }
    anyhow::Error::new(BoxedSource(box_err))
}

/// Walk the error chain and produce a `StoreError`. If a UNIQUE constraint
/// violation is found whose constraint name matches a known resource, returns
/// `StoreError::Constraint(Unique { resource })`. Otherwise wraps via
/// `StoreError::Other(map_err(...))`.
///
/// The constraint-name mapping table is the single point of coupling between
/// SQLite's error format and our domain labels. Adding a new label here is
/// the one-line edit required when a new caller wants to introspect.
pub(super) fn store_err_from_anyhow(err: anyhow::Error) -> crate::store::StoreError {
    if let Some(missing) = missing_task_scope_from_chain(err.as_ref()) {
        return crate::store::StoreError::AllocationTaskScopeMissing {
            user_id: missing.user_id.clone(),
            prefix: missing.prefix.clone(),
        };
    }
    if let Some(limit_err) = webhook_limit_from_chain(err.as_ref()) {
        return crate::store::StoreError::WebhookLimitReached {
            limit: limit_err.limit,
        };
    }
    if let Some(resource) = unique_resource_from_chain(err.as_ref()) {
        return crate::store::StoreError::unique(resource);
    }
    crate::store::StoreError::Other(err)
}

fn webhook_limit_from_chain<'a>(
    err: &'a (dyn std::error::Error + 'static),
) -> Option<&'a crate::store::error::WebhookLimitReachedError> {
    let mut cursor: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cursor {
        if let Some(limit_err) = e.downcast_ref::<crate::store::error::WebhookLimitReachedError>() {
            return Some(limit_err);
        }
        cursor = e.source();
    }
    None
}

fn missing_task_scope_from_chain<'a>(
    err: &'a (dyn std::error::Error + 'static),
) -> Option<&'a crate::store::error::MissingTaskScopeForAllocationError> {
    let mut cursor: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cursor {
        if let Some(missing) =
            e.downcast_ref::<crate::store::error::MissingTaskScopeForAllocationError>()
        {
            return Some(missing);
        }
        cursor = e.source();
    }
    None
}

fn unique_resource_from_chain(err: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    let mut cursor: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cursor {
        if let Some(rusqlite_err) = e.downcast_ref::<rusqlite::Error>() {
            return rusqlite_unique_resource(rusqlite_err);
        }
        cursor = e.source();
    }
    None
}

fn rusqlite_unique_resource(err: &rusqlite::Error) -> Option<&'static str> {
    use crate::store::error::resources;

    let rusqlite::Error::SqliteFailure(ffi_err, msg) = err else {
        return None;
    };
    if !matches!(ffi_err.code, rusqlite::ErrorCode::ConstraintViolation) {
        return None;
    }
    let msg = msg.as_deref().unwrap_or("");
    let constraint = msg.strip_prefix("UNIQUE constraint failed: ")?.trim();
    Some(match constraint {
        "users.username" => resources::USERS_USERNAME,
        "users.prefix" => resources::USERS_PREFIX,
        // SQLite reports partial UNIQUE-index failures by indexed column list,
        // not by index name (`idx_task_scopes_*`).
        "task_scopes.owner_runtime_user_id" => resources::TASK_SCOPES_PERSONAL_OWNER,
        "task_scopes.key_prefix" => resources::TASK_SCOPES_KEY_PREFIX,
        "replicas.user_id" => resources::REPLICAS_USER_ID,
        "devices.bootstrap_request_id" => resources::DEVICES_BOOTSTRAP_REQUEST_ID,
        "webhooks.user_id, webhooks.url" => resources::WEBHOOKS_USER_URL,
        "admin_webhooks.url" => resources::ADMIN_WEBHOOKS_URL,
        // Composite primary key on task_key_allocations.
        "task_key_allocations.user_id, task_key_allocations.prefix, task_key_allocations.n" => {
            resources::TASK_KEY_ALLOCATIONS_USER_PREFIX_N
        }
        "task_key_allocations.task_uuid" => resources::TASK_KEY_ALLOCATIONS_TASK_UUID,
        "task_key_allocations.task_scope_id, task_key_allocations.n" => {
            resources::TASK_KEY_ALLOCATIONS_TASK_SCOPE_N
        }
        // Unknown unique constraint — fall through to anyhow wrapping
        // rather than synthesising a label, so callers can't accidentally
        // match a stale name.
        _ => return None,
    })
}

/// Round-robin pool of read-only `tokio_rusqlite::Connection`s.
///
/// Each `tokio_rusqlite::Connection` owns ONE background thread + ONE
/// rusqlite connection, so it serializes its own calls. The single shared
/// writer connection (`SqliteConfigStore::conn`) is therefore a process-wide
/// serialization point: config reads queue behind writes (see #146 — the
/// fsync-free `get_user_prefix` read grew 368× from 1→50 users). WAL allows
/// many concurrent readers against the same file, so this pool of N separate
/// connections lets hot reads bypass the writer queue and run in parallel.
///
/// Read connections set `query_only=ON` so they can never write — the
/// `MAX(n)+1`+INSERT allocation path and every other mutation stay on the
/// writer lane (`conn`). See #147.
struct ReadPool {
    conns: Vec<Connection>,
    next: AtomicUsize,
}

impl ReadPool {
    /// Pick the next read connection, round-robin. `Relaxed` is fine —
    /// we only need even-ish distribution, not a strict sequence.
    fn pick(&self) -> &Connection {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.conns.len();
        &self.conns[i]
    }
}

/// `:memory:` (and the empty-string anonymous temp DB) are PER-CONNECTION —
/// separate connections each get their own independent database. A read pool
/// would see empty databases, so it must be disabled and reads fall back to
/// the writer connection (used by the `:memory:` store unit tests).
fn is_per_connection_db(db_path: &str) -> bool {
    db_path == ":memory:" || db_path.is_empty()
}

/// Read-pool size. Defaults to available parallelism clamped to [2, 8]
/// (the reference runner is 8 vCPU); overridable via
/// `CMDOCK_CONFIG_READ_POOL_SIZE` (clamped to [1, 64]).
fn read_pool_size() -> usize {
    if let Ok(v) = std::env::var("CMDOCK_CONFIG_READ_POOL_SIZE") {
        if let Ok(n) = v.parse::<usize>() {
            if n >= 1 {
                return n.min(64);
            }
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 8)
}

async fn build_read_pool(db_path: &str) -> anyhow::Result<Option<Arc<ReadPool>>> {
    if is_per_connection_db(db_path) {
        return Ok(None);
    }
    let size = read_pool_size();
    let mut conns = Vec::with_capacity(size);
    for _ in 0..size {
        let conn = Connection::open(db_path).await?;
        conn.call(|conn| {
            // Do NOT set `journal_mode` here: the file is already WAL (the
            // writer set it first), and a `journal_mode` pragma issued from a
            // `query_only` connection can error as an attempted header write.
            // `query_only` is set LAST so the `foreign_keys` pragma runs first.
            conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA query_only=ON;")?;
            Ok::<_, BoxErr>(())
        })
        .await
        .map_err(map_err)?;
        conns.push(conn);
    }
    Ok(Some(Arc::new(ReadPool {
        conns,
        next: AtomicUsize::new(0),
    })))
}

/// SQLite-backed implementation of ConfigStore.
///
/// Uses tokio-rusqlite for async access. Designed to be swappable
/// with a Postgres implementation when scaling commercially.
///
/// `conn` is the single WRITER lane (all writes + the `MAX(n)+1`+INSERT
/// allocation path under `BEGIN IMMEDIATE`). `read_pool` serves hot reads
/// concurrently so they don't queue behind writes (#147); it is `None` for
/// per-connection (`:memory:`) databases, where reads fall back to `conn`.
#[derive(Clone)]
pub struct SqliteConfigStore {
    conn: Connection,
    read_pool: Option<Arc<ReadPool>>,
}

impl SqliteConfigStore {
    pub async fn new(db_path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(db_path).await?;

        conn.call(|conn| {
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
            Ok::<_, BoxErr>(())
        })
        .await
        .map_err(map_err)?;

        let read_pool = build_read_pool(db_path).await?;

        Ok(Self { conn, read_pool })
    }

    /// Dispatch a READ-ONLY closure to the read pool (round-robin), or to the
    /// writer connection when no pool exists (`:memory:`). Mirrors
    /// `tokio_rusqlite::Connection::call`. Use ONLY for pure reads — the
    /// pooled connections are `query_only`, so any write will error. Writes
    /// and allocation transactions MUST stay on `self.conn`.
    pub(super) async fn read_call<F, R, E>(
        &self,
        function: F,
    ) -> std::result::Result<R, tokio_rusqlite::Error<E>>
    where
        F: FnOnce(&mut rusqlite::Connection) -> std::result::Result<R, E> + Send + 'static,
        R: Send + 'static,
        E: Send + 'static,
    {
        match &self.read_pool {
            Some(pool) => pool.pick().call(function).await,
            None => self.conn.call(function).await,
        }
    }
}

#[cfg(test)]
impl SqliteConfigStore {
    /// Create all tables inline (for unit tests that don't have the migrations directory).
    async fn run_migrations_inline(&self) -> anyhow::Result<()> {
        self.conn
            .call(|conn| {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS users (
                        id TEXT PRIMARY KEY,
                        username TEXT UNIQUE NOT NULL,
                        password_hash TEXT NOT NULL,
                        created_at TEXT NOT NULL DEFAULT (datetime('now')),
                        prefix TEXT,
                        task_keys_migrated_at TEXT
                    );
                    CREATE UNIQUE INDEX IF NOT EXISTS idx_users_prefix
                        ON users(prefix) WHERE prefix IS NOT NULL;
                    -- Keep in sync with migrations/031_create_task_scopes.sql;
                    -- inline migrations are only for store unit tests.
                    CREATE TABLE IF NOT EXISTS task_scopes (
                        id TEXT PRIMARY KEY,
                        kind TEXT NOT NULL CHECK (kind IN ('personal')),
                        owner_runtime_user_id TEXT,
                        owner_team_id TEXT,
                        key_prefix TEXT NOT NULL,
                        status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','disabled','deleted')),
                        storage_path TEXT,
                        created_at TEXT NOT NULL DEFAULT (datetime('now')),
                        updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                        -- TODO(Task Scope teams): relax both the kind CHECK
                        -- and this ownership CHECK together when team scopes
                        -- are enabled.
                        CHECK (kind = 'personal' AND owner_runtime_user_id IS NOT NULL AND owner_team_id IS NULL),
                        FOREIGN KEY(owner_runtime_user_id) REFERENCES users(id) ON DELETE CASCADE
                    );
                    CREATE UNIQUE INDEX IF NOT EXISTS idx_task_scopes_personal_owner
                        ON task_scopes(owner_runtime_user_id)
                        WHERE kind = 'personal' AND owner_runtime_user_id IS NOT NULL AND status != 'deleted';
                    CREATE UNIQUE INDEX IF NOT EXISTS idx_task_scopes_key_prefix_active
                        ON task_scopes(key_prefix) WHERE status != 'deleted';
                    CREATE TABLE IF NOT EXISTS task_key_allocations (
                        user_id      TEXT    NOT NULL,
                        prefix       TEXT    NOT NULL,
                        n            INTEGER NOT NULL,
                        task_scope_id TEXT REFERENCES task_scopes(id),
                        task_uuid    TEXT,
                        state        TEXT    NOT NULL CHECK (state IN ('pending','committed','burned')),
                        attempt_id   TEXT    NOT NULL,
                        created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
                        committed_at TEXT,
                        PRIMARY KEY (user_id, prefix, n)
                    );
                    CREATE INDEX IF NOT EXISTS idx_task_key_allocations_state
                        ON task_key_allocations(user_id, prefix, state);
                    CREATE UNIQUE INDEX IF NOT EXISTS idx_task_key_allocations_uuid
                        ON task_key_allocations(task_uuid) WHERE task_uuid IS NOT NULL;
                    CREATE UNIQUE INDEX IF NOT EXISTS idx_task_key_allocations_task_scope_n
                        ON task_key_allocations(task_scope_id, n)
                        WHERE task_scope_id IS NOT NULL;
                    CREATE INDEX IF NOT EXISTS idx_task_key_allocations_task_scope_state
                        ON task_key_allocations(task_scope_id, state);
                    CREATE TABLE IF NOT EXISTS api_tokens (
                        token_hash TEXT PRIMARY KEY,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        label TEXT,
                        token_id TEXT UNIQUE,
                        expires_at TEXT,
                        created_at TEXT NOT NULL DEFAULT (datetime('now')),
                        first_used_at TEXT,
                        last_used_at TEXT,
                        last_used_ip TEXT
                    );
                    CREATE TABLE IF NOT EXISTS user_runtime_policies (
                        user_id TEXT PRIMARY KEY REFERENCES users(id),
                        desired_version TEXT NOT NULL,
                        desired_policy_json TEXT NOT NULL,
                        applied_version TEXT,
                        applied_policy_json TEXT,
                        applied_at TEXT,
                        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                    );
                    CREATE TABLE IF NOT EXISTS views (
                        id TEXT NOT NULL,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        label TEXT NOT NULL,
                        icon TEXT NOT NULL DEFAULT '',
                        filter TEXT NOT NULL DEFAULT '',
                        group_by TEXT,
                        context_filtered INTEGER NOT NULL DEFAULT 0,
                        display_mode TEXT NOT NULL DEFAULT 'list',
                        sort_order INTEGER NOT NULL DEFAULT 0,
                        origin TEXT NOT NULL DEFAULT 'user' CHECK(origin IN ('builtin', 'user')),
                        user_modified INTEGER NOT NULL DEFAULT 0,
                        hidden INTEGER NOT NULL DEFAULT 0,
                        template_version INTEGER NOT NULL DEFAULT 0,
                        context_id TEXT,
                        PRIMARY KEY (user_id, id)
                    );
                    CREATE TABLE IF NOT EXISTS contexts (
                        id TEXT NOT NULL,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        label TEXT NOT NULL,
                        project_prefixes TEXT NOT NULL DEFAULT '[]',
                        color TEXT,
                        icon TEXT,
                        sort_order INTEGER NOT NULL DEFAULT 0,
                        origin TEXT NOT NULL DEFAULT 'user' CHECK(origin IN ('builtin', 'user')),
                        user_modified INTEGER NOT NULL DEFAULT 0,
                        hidden INTEGER NOT NULL DEFAULT 0,
                        template_version INTEGER NOT NULL DEFAULT 0,
                        PRIMARY KEY (user_id, id)
                    );
                    CREATE TABLE IF NOT EXISTS presets (
                        id TEXT NOT NULL,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        label TEXT NOT NULL,
                        raw_suffix TEXT NOT NULL DEFAULT '',
                        sort_order INTEGER NOT NULL DEFAULT 0,
                        PRIMARY KEY (user_id, id)
                    );
                    CREATE TABLE IF NOT EXISTS stores (
                        id TEXT NOT NULL,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        label TEXT NOT NULL,
                        tag TEXT NOT NULL DEFAULT '',
                        sort_order INTEGER NOT NULL DEFAULT 0,
                        PRIMARY KEY (user_id, id)
                    );
                    CREATE TABLE IF NOT EXISTS shopping_config (
                        user_id TEXT PRIMARY KEY REFERENCES users(id),
                        project TEXT NOT NULL DEFAULT '',
                        default_tags TEXT NOT NULL DEFAULT '[]'
                    );
                    CREATE TABLE IF NOT EXISTS geofences (
                        id TEXT NOT NULL,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        label TEXT NOT NULL,
                        latitude REAL NOT NULL,
                        longitude REAL NOT NULL,
                        radius REAL NOT NULL DEFAULT 200,
                        type TEXT NOT NULL DEFAULT 'home',
                        context_id TEXT,
                        view_id TEXT,
                        store_tag TEXT,
                        PRIMARY KEY (user_id, id)
                    );
                    CREATE TABLE IF NOT EXISTS config (
                        config_type TEXT NOT NULL,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        version TEXT,
                        items TEXT NOT NULL DEFAULT '[]',
                        PRIMARY KEY (user_id, config_type)
                    );
                    CREATE TABLE IF NOT EXISTS replicas (
                        id TEXT PRIMARY KEY,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        encryption_secret_enc TEXT NOT NULL,
                        label TEXT NOT NULL DEFAULT 'Personal',
                        created_at TEXT NOT NULL DEFAULT (datetime('now')),
                        UNIQUE(user_id)
                    );
                    CREATE TABLE IF NOT EXISTS devices (
                        client_id TEXT PRIMARY KEY,
                        user_id TEXT NOT NULL REFERENCES users(id),
                        name TEXT NOT NULL,
                        encryption_secret_enc TEXT,
                        registered_at TEXT NOT NULL DEFAULT (datetime('now')),
                        last_sync_at TEXT,
                        last_sync_ip TEXT,
                        status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'revoked')),
                        bootstrap_request_id TEXT UNIQUE,
                        bootstrap_status TEXT,
                        bootstrap_requested_username TEXT,
                        bootstrap_create_user_if_missing INTEGER,
                        bootstrap_expires_at TEXT
                    );
                    CREATE TABLE IF NOT EXISTS webhooks (
                        id TEXT PRIMARY KEY,
                        user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                        url TEXT NOT NULL,
                        events_json TEXT NOT NULL,
                        modified_fields_json TEXT,
                        name TEXT,
                        enabled INTEGER NOT NULL DEFAULT 1,
                        consecutive_failures INTEGER NOT NULL DEFAULT 0,
                        secret_enc TEXT NOT NULL,
                        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                        updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                        UNIQUE(user_id, url)
                    );
                    CREATE INDEX IF NOT EXISTS idx_webhooks_user_id
                    ON webhooks(user_id);
                    CREATE TABLE IF NOT EXISTS webhook_deliveries (
                        delivery_id TEXT PRIMARY KEY,
                        webhook_id TEXT NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
                        event_id TEXT NOT NULL,
                        event TEXT NOT NULL,
                        timestamp TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                        status TEXT NOT NULL,
                        response_status INTEGER,
                        attempt INTEGER NOT NULL,
                        failure_reason TEXT
                    );
                    CREATE TABLE IF NOT EXISTS admin_webhooks (
                        id TEXT PRIMARY KEY,
                        url TEXT NOT NULL UNIQUE,
                        events_json TEXT NOT NULL,
                        modified_fields_json TEXT,
                        name TEXT,
                        enabled INTEGER NOT NULL DEFAULT 1,
                        consecutive_failures INTEGER NOT NULL DEFAULT 0,
                        secret_enc TEXT NOT NULL,
                        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                        updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
                    );
                    CREATE TABLE IF NOT EXISTS admin_webhook_deliveries (
                        delivery_id TEXT PRIMARY KEY,
                        webhook_id TEXT NOT NULL REFERENCES admin_webhooks(id) ON DELETE CASCADE,
                        event_id TEXT NOT NULL,
                        event TEXT NOT NULL,
                        timestamp TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                        status TEXT NOT NULL,
                        response_status INTEGER,
                        attempt INTEGER NOT NULL,
                        failure_reason TEXT
                    );
                    CREATE TABLE IF NOT EXISTS webhook_event_history (
                        user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                        task_uuid TEXT NOT NULL,
                        event_type TEXT NOT NULL,
                        due_at TEXT NOT NULL,
                        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                        PRIMARY KEY (user_id, task_uuid, event_type, due_at)
                    );
                    CREATE INDEX IF NOT EXISTS idx_webhook_event_history_task
                    ON webhook_event_history(user_id, task_uuid);
                    CREATE INDEX IF NOT EXISTS idx_webhook_event_history_created_at
                    ON webhook_event_history(created_at);
                    CREATE TABLE IF NOT EXISTS merged_sync_journal (
                        journal_id TEXT PRIMARY KEY,
                        user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                        client_id TEXT NOT NULL,
                        attempt_id TEXT NOT NULL,
                        parent_version_id TEXT NOT NULL,
                        inbound_history_segment BLOB NOT NULL DEFAULT X'',
                        merged_version_id TEXT,
                        state TEXT NOT NULL CHECK (state IN ('received','merged_version_accepted','source_plan_applied','projection_appended','finalized','failed','quarantined')),
                        recovery_status TEXT NOT NULL CHECK (recovery_status IN ('not_required','recoverable','recovered','failed','quarantined')),
                        diagnostic_code TEXT,
                        diagnostic_message TEXT,
                        state_version INTEGER NOT NULL DEFAULT 0,
                        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                        updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                        finalized_at TEXT,
                        CHECK ((state IN ('finalized', 'failed', 'quarantined')) OR finalized_at IS NULL)
                    );
                    CREATE INDEX IF NOT EXISTS idx_merged_sync_journal_user_state
                        ON merged_sync_journal(user_id, state, updated_at);
                    CREATE INDEX IF NOT EXISTS idx_merged_sync_journal_recovery_status
                        ON merged_sync_journal(recovery_status, updated_at);
                    CREATE UNIQUE INDEX IF NOT EXISTS idx_merged_sync_journal_attempt
                        ON merged_sync_journal(journal_id, attempt_id);",
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

#[async_trait]
impl ConfigStore for SqliteConfigStore {
    // --- Users & Auth ---

    async fn get_user_by_token(&self, token: &str) -> anyhow::Result<Option<UserRecord>> {
        self.get_user_by_token_impl(token).await
    }

    async fn get_user_by_id(&self, user_id: &str) -> anyhow::Result<Option<UserRecord>> {
        self.get_user_by_id_impl(user_id).await
    }

    async fn get_user_by_username(&self, username: &str) -> anyhow::Result<Option<UserRecord>> {
        self.get_user_by_username_impl(username).await
    }

    async fn list_users(&self) -> anyhow::Result<Vec<UserRecord>> {
        self.list_users_impl().await
    }

    async fn create_user(&self, user: &NewUser) -> Result<UserRecord, crate::store::StoreError> {
        self.create_user_impl(user)
            .await
            .map_err(store_err_from_anyhow)
    }

    async fn create_api_token(&self, user_id: &str, label: Option<&str>) -> anyhow::Result<String> {
        self.create_api_token_impl(user_id, label).await
    }

    async fn create_api_token_with_expiry(
        &self,
        user_id: &str,
        label: Option<&str>,
        expires_at: Option<&str>,
        token_bytes: usize,
    ) -> anyhow::Result<String> {
        self.create_api_token_with_expiry_impl(user_id, label, expires_at, token_bytes)
            .await
    }

    async fn create_labeled_api_token(
        &self,
        user_id: &str,
        label: &str,
        token_id: &str,
        expires_at: &str,
        token_bytes: usize,
    ) -> anyhow::Result<IssuedApiToken> {
        self.create_labeled_api_token_impl(user_id, label, token_id, expires_at, token_bytes)
            .await
    }

    async fn lookup_token_correlation(
        &self,
        token: &str,
        expected_label: &str,
    ) -> anyhow::Result<Option<LabeledTokenCorrelation>> {
        self.lookup_token_correlation_impl(token, expected_label)
            .await
    }

    async fn delete_user(&self, user_id: &str) -> anyhow::Result<bool> {
        self.delete_user_impl(user_id).await
    }

    async fn get_runtime_policy(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Option<RuntimePolicyRecord>> {
        self.get_runtime_policy_impl(user_id).await
    }

    async fn upsert_runtime_policy(
        &self,
        user_id: &str,
        desired_version: &str,
        desired_policy: &crate::runtime_policy::RuntimePolicy,
        applied_version: Option<&str>,
        applied_policy: Option<&crate::runtime_policy::RuntimePolicy>,
        applied_at: Option<&str>,
    ) -> anyhow::Result<RuntimePolicyRecord> {
        self.upsert_runtime_policy_impl(
            user_id,
            desired_version,
            desired_policy,
            applied_version,
            applied_policy,
            applied_at,
        )
        .await
    }

    async fn list_api_tokens(&self, user_id: &str) -> anyhow::Result<Vec<ApiTokenRecord>> {
        self.list_api_tokens_impl(user_id).await
    }

    async fn mark_token_used(
        &self,
        token: &str,
        client_ip: &str,
        expected_label: &str,
    ) -> anyhow::Result<Option<TokenUseRecord>> {
        self.mark_token_used_impl(token, client_ip, expected_label)
            .await
    }

    async fn revoke_api_token(&self, token_hash: &str) -> anyhow::Result<bool> {
        self.revoke_api_token_impl(token_hash).await
    }

    // --- Views ---

    async fn list_views(&self, user_id: &str) -> anyhow::Result<Vec<ViewRecord>> {
        self.list_views_impl(user_id).await
    }

    async fn list_views_all(&self, user_id: &str) -> anyhow::Result<Vec<ViewRecord>> {
        self.list_views_all_impl(user_id).await
    }

    async fn upsert_view(&self, user_id: &str, view: &ViewRecord) -> anyhow::Result<()> {
        self.upsert_view_impl(user_id, view).await
    }

    async fn delete_view(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        self.delete_view_impl(user_id, id).await
    }

    // --- Contexts ---

    async fn list_contexts(&self, user_id: &str) -> anyhow::Result<Vec<ContextRecord>> {
        self.list_contexts_impl(user_id).await
    }

    async fn list_contexts_all(&self, user_id: &str) -> anyhow::Result<Vec<ContextRecord>> {
        self.list_contexts_all_impl(user_id).await
    }

    async fn upsert_context(&self, user_id: &str, ctx: &ContextRecord) -> anyhow::Result<()> {
        self.upsert_context_impl(user_id, ctx).await
    }

    async fn delete_context(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        self.delete_context_impl(user_id, id).await
    }

    // --- Presets ---

    async fn list_presets(&self, user_id: &str) -> anyhow::Result<Vec<PresetRecord>> {
        self.list_presets_impl(user_id).await
    }

    async fn upsert_preset(&self, user_id: &str, preset: &PresetRecord) -> anyhow::Result<()> {
        self.upsert_preset_impl(user_id, preset).await
    }

    async fn delete_preset(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        self.delete_preset_impl(user_id, id).await
    }

    // --- Stores ---

    async fn list_stores(&self, user_id: &str) -> anyhow::Result<Vec<StoreRecord>> {
        self.list_stores_impl(user_id).await
    }

    async fn upsert_store(&self, user_id: &str, store: &StoreRecord) -> anyhow::Result<()> {
        self.upsert_store_impl(user_id, store).await
    }

    async fn delete_store(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        self.delete_store_impl(user_id, id).await
    }

    // --- Shopping Config ---

    async fn get_shopping_config(&self, user_id: &str) -> anyhow::Result<Option<ShoppingRecord>> {
        self.get_shopping_config_impl(user_id).await
    }

    async fn upsert_shopping_config(
        &self,
        user_id: &str,
        config: &ShoppingRecord,
    ) -> anyhow::Result<()> {
        self.upsert_shopping_config_impl(user_id, config).await
    }

    async fn delete_shopping_config(&self, user_id: &str) -> anyhow::Result<bool> {
        self.delete_shopping_config_impl(user_id).await
    }

    // --- Geofences ---

    async fn list_geofences(&self, user_id: &str) -> anyhow::Result<Vec<GeofenceRecord>> {
        self.list_geofences_impl(user_id).await
    }

    async fn upsert_geofence(
        &self,
        user_id: &str,
        geofence: &GeofenceRecord,
    ) -> anyhow::Result<()> {
        self.upsert_geofence_impl(user_id, geofence).await
    }

    async fn delete_geofence(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        self.delete_geofence_impl(user_id, id).await
    }

    // --- Generic Config ---

    async fn get_config(
        &self,
        user_id: &str,
        config_type: &str,
    ) -> anyhow::Result<Option<GenericConfigRecord>> {
        self.get_config_impl(user_id, config_type).await
    }

    async fn upsert_config(
        &self,
        user_id: &str,
        config_type: &str,
        record: &GenericConfigRecord,
    ) -> anyhow::Result<()> {
        self.upsert_config_impl(user_id, config_type, record).await
    }

    async fn delete_config_item(
        &self,
        user_id: &str,
        config_type: &str,
        item_id: &str,
    ) -> anyhow::Result<bool> {
        self.delete_config_item_impl(user_id, config_type, item_id)
            .await
    }

    // --- Replicas (ADR-0001) ---

    async fn create_replica(
        &self,
        user_id: &str,
        client_id: &str,
        encryption_secret_enc: &str,
    ) -> Result<(), crate::store::StoreError> {
        self.create_replica_impl(user_id, client_id, encryption_secret_enc)
            .await
            .map_err(store_err_from_anyhow)
    }

    async fn get_replica_by_user(&self, user_id: &str) -> anyhow::Result<Option<ReplicaRecord>> {
        self.get_replica_by_user_impl(user_id).await
    }

    async fn get_replica_by_client_id(
        &self,
        client_id: &str,
    ) -> anyhow::Result<Option<ReplicaRecord>> {
        self.get_replica_by_client_id_impl(client_id).await
    }

    async fn get_user_by_client_id(&self, client_id: &str) -> anyhow::Result<Option<UserRecord>> {
        self.get_user_by_client_id_impl(client_id).await
    }

    async fn delete_replica(&self, user_id: &str) -> anyhow::Result<bool> {
        self.delete_replica_impl(user_id).await
    }

    // --- Devices ---

    async fn list_devices(&self, user_id: &str) -> anyhow::Result<Vec<DeviceRecord>> {
        self.list_devices_impl(user_id).await
    }

    async fn get_device(&self, client_id: &str) -> anyhow::Result<Option<DeviceRecord>> {
        self.get_device_impl(client_id).await
    }

    async fn get_device_by_bootstrap_request(
        &self,
        bootstrap_request_id: &str,
    ) -> anyhow::Result<Option<DeviceRecord>> {
        self.get_device_by_bootstrap_request_impl(bootstrap_request_id)
            .await
    }

    async fn create_device(
        &self,
        user_id: &str,
        client_id: &str,
        name: &str,
        encryption_secret_enc: Option<&str>,
    ) -> anyhow::Result<()> {
        self.create_device_impl(user_id, client_id, name, encryption_secret_enc)
            .await
    }

    // ADR-0002 HC-1 exception: trait dispatch mirrors trait signature
    // exactly; bundling into a struct here adds an indirection without
    // simplifying anything. Trait + impl already annotated.
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
    ) -> Result<(), crate::store::StoreError> {
        self.create_bootstrap_device_impl(
            user_id,
            client_id,
            name,
            encryption_secret_enc,
            bootstrap_request_id,
            bootstrap_requested_username,
            bootstrap_create_user_if_missing,
            bootstrap_expires_at,
        )
        .await
        .map_err(store_err_from_anyhow)
    }

    async fn update_device_name(
        &self,
        user_id: &str,
        client_id: &str,
        name: &str,
    ) -> anyhow::Result<bool> {
        self.update_device_name_impl(user_id, client_id, name).await
    }

    async fn revoke_device(&self, user_id: &str, client_id: &str) -> anyhow::Result<bool> {
        self.revoke_device_impl(user_id, client_id).await
    }

    async fn unrevoke_device(&self, user_id: &str, client_id: &str) -> anyhow::Result<bool> {
        self.unrevoke_device_impl(user_id, client_id).await
    }

    async fn delete_device(&self, user_id: &str, client_id: &str) -> anyhow::Result<bool> {
        self.delete_device_impl(user_id, client_id).await
    }

    async fn acknowledge_bootstrap_device(
        &self,
        bootstrap_request_id: &str,
    ) -> anyhow::Result<bool> {
        self.acknowledge_bootstrap_device_impl(bootstrap_request_id)
            .await
    }

    async fn touch_device(&self, client_id: &str, ip: &str) -> anyhow::Result<()> {
        self.touch_device_impl(client_id, ip).await
    }

    // --- Webhooks ---

    async fn list_webhooks(&self, user_id: &str) -> anyhow::Result<Vec<WebhookRecord>> {
        self.list_webhooks_impl(user_id).await
    }

    async fn get_webhook(
        &self,
        user_id: &str,
        webhook_id: &str,
    ) -> anyhow::Result<Option<WebhookRecord>> {
        self.get_webhook_impl(user_id, webhook_id).await
    }

    async fn create_webhook(
        &self,
        webhook: &NewWebhookRecord,
        limit: usize,
    ) -> Result<WebhookRecord, crate::store::StoreError> {
        self.create_webhook_impl(webhook, limit)
            .await
            .map_err(store_err_from_anyhow)
    }

    async fn update_webhook(
        &self,
        webhook: &UpdateWebhookRecord,
    ) -> Result<Option<WebhookRecord>, crate::store::StoreError> {
        self.update_webhook_impl(webhook)
            .await
            .map_err(store_err_from_anyhow)
    }

    async fn delete_webhook(&self, user_id: &str, webhook_id: &str) -> anyhow::Result<bool> {
        self.delete_webhook_impl(user_id, webhook_id).await
    }

    async fn list_admin_webhooks(&self) -> anyhow::Result<Vec<AdminWebhookRecord>> {
        self.list_admin_webhooks_impl().await
    }

    async fn get_admin_webhook(
        &self,
        webhook_id: &str,
    ) -> anyhow::Result<Option<AdminWebhookRecord>> {
        self.get_admin_webhook_impl(webhook_id).await
    }

    async fn create_admin_webhook(
        &self,
        webhook: &NewAdminWebhookRecord,
        limit: usize,
    ) -> Result<AdminWebhookRecord, crate::store::StoreError> {
        self.create_admin_webhook_impl(webhook, limit)
            .await
            .map_err(store_err_from_anyhow)
    }

    async fn update_admin_webhook(
        &self,
        webhook: &UpdateAdminWebhookRecord,
    ) -> Result<Option<AdminWebhookRecord>, crate::store::StoreError> {
        self.update_admin_webhook_impl(webhook)
            .await
            .map_err(store_err_from_anyhow)
    }

    async fn delete_admin_webhook(&self, webhook_id: &str) -> anyhow::Result<bool> {
        self.delete_admin_webhook_impl(webhook_id).await
    }

    async fn list_webhook_deliveries(
        &self,
        user_id: &str,
        webhook_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<WebhookDeliveryRecord>> {
        self.list_webhook_deliveries_impl(user_id, webhook_id, limit)
            .await
    }

    async fn list_admin_webhook_deliveries(
        &self,
        webhook_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<WebhookDeliveryRecord>> {
        self.list_admin_webhook_deliveries_impl(webhook_id, limit)
            .await
    }

    async fn record_webhook_delivery(
        &self,
        delivery: &WebhookDeliveryRecord,
    ) -> anyhow::Result<()> {
        self.record_webhook_delivery_impl(delivery).await
    }

    async fn record_admin_webhook_delivery(
        &self,
        delivery: &WebhookDeliveryRecord,
    ) -> anyhow::Result<()> {
        self.record_admin_webhook_delivery_impl(delivery).await
    }

    async fn purge_webhook_deliveries_older_than(
        &self,
        retention_days: u32,
    ) -> anyhow::Result<usize> {
        self.purge_webhook_deliveries_older_than_impl(retention_days)
            .await
    }

    async fn mark_webhook_delivery_succeeded(&self, webhook_id: &str) -> anyhow::Result<()> {
        self.mark_webhook_delivery_succeeded_impl(webhook_id).await
    }

    async fn mark_admin_webhook_delivery_succeeded(&self, webhook_id: &str) -> anyhow::Result<()> {
        self.mark_admin_webhook_delivery_succeeded_impl(webhook_id)
            .await
    }

    async fn mark_webhook_delivery_failed(
        &self,
        webhook_id: &str,
        disable_after: u32,
    ) -> anyhow::Result<Option<WebhookFailureState>> {
        self.mark_webhook_delivery_failed_impl(webhook_id, disable_after)
            .await
    }

    async fn mark_admin_webhook_delivery_failed(
        &self,
        webhook_id: &str,
        disable_after: u32,
    ) -> anyhow::Result<Option<WebhookFailureState>> {
        self.mark_admin_webhook_delivery_failed_impl(webhook_id, disable_after)
            .await
    }

    async fn record_webhook_event_history(
        &self,
        user_id: &str,
        task_uuid: &str,
        event_type: &str,
        due_at: &str,
    ) -> anyhow::Result<bool> {
        self.record_webhook_event_history_impl(user_id, task_uuid, event_type, due_at)
            .await
    }

    async fn clear_webhook_event_history(
        &self,
        user_id: &str,
        task_uuid: &str,
    ) -> anyhow::Result<()> {
        self.clear_webhook_event_history_impl(user_id, task_uuid)
            .await
    }

    // --- Idempotency-Key dedup records ---

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
    ) -> anyhow::Result<crate::store::models::IdempotencyLookupOutcome> {
        self.lookup_or_insert_idempotency_pending_impl(
            user_id,
            request_path,
            idempotency_key,
            body_fingerprint,
            pending_timeout_seconds,
            completed_retention_hours,
            now_unix_seconds,
        )
        .await
    }

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
    ) -> anyhow::Result<bool> {
        self.finalize_idempotency_completed_impl(
            user_id,
            request_path,
            idempotency_key,
            attempt_id,
            status_code,
            response_body,
            content_type,
        )
        .await
    }

    async fn rollback_idempotency_pending(
        &self,
        user_id: &str,
        request_path: &str,
        idempotency_key: &str,
        attempt_id: &str,
    ) -> anyhow::Result<bool> {
        self.rollback_idempotency_pending_impl(user_id, request_path, idempotency_key, attempt_id)
            .await
    }

    async fn prune_idempotency_completed(
        &self,
        retention_hours: u32,
        now_unix_seconds: i64,
    ) -> anyhow::Result<usize> {
        self.prune_idempotency_completed_impl(retention_hours, now_unix_seconds)
            .await
    }

    async fn prune_idempotency_stranded_pending(
        &self,
        pending_timeout_seconds: u32,
        now_unix_seconds: i64,
    ) -> anyhow::Result<usize> {
        self.prune_idempotency_stranded_pending_impl(pending_timeout_seconds, now_unix_seconds)
            .await
    }

    // --- Task Keys ---

    async fn reserve_task_key_pending(
        &self,
        user_id: &str,
        prefix: &str,
    ) -> Result<(i64, String), crate::store::StoreError> {
        self.reserve_task_key_pending_impl(user_id, prefix).await
    }

    async fn reserve_task_key_pending_for_uuid(
        &self,
        user_id: &str,
        prefix: &str,
        task_uuid: &str,
    ) -> Result<(i64, String), crate::store::StoreError> {
        self.reserve_task_key_pending_for_uuid_impl(user_id, prefix, task_uuid)
            .await
    }

    async fn attach_task_uuid_to_pending(
        &self,
        user_id: &str,
        prefix: &str,
        n: i64,
        attempt_id: &str,
        task_uuid: &str,
    ) -> Result<(), crate::store::StoreError> {
        self.attach_task_uuid_to_pending_impl(user_id, prefix, n, attempt_id, task_uuid)
            .await
    }

    async fn commit_task_key(
        &self,
        user_id: &str,
        prefix: &str,
        n: i64,
        attempt_id: &str,
    ) -> Result<(), crate::store::StoreError> {
        self.commit_task_key_impl(user_id, prefix, n, attempt_id)
            .await
    }

    async fn burn_task_key(
        &self,
        user_id: &str,
        prefix: &str,
        n: i64,
        attempt_id: &str,
    ) -> Result<(), crate::store::StoreError> {
        self.burn_task_key_impl(user_id, prefix, n, attempt_id)
            .await
    }

    async fn select_stale_pending_task_keys(
        &self,
        now_unix_seconds: i64,
        pending_timeout_seconds: u32,
        batch_limit: usize,
    ) -> Result<Vec<crate::store::models::StalePendingCandidate>, crate::store::StoreError> {
        self.select_stale_pending_task_keys_impl(
            now_unix_seconds,
            pending_timeout_seconds,
            batch_limit,
        )
        .await
    }

    async fn get_user_prefix(
        &self,
        user_id: &str,
    ) -> Result<Option<String>, crate::store::StoreError> {
        self.get_user_prefix_impl(user_id).await
    }

    async fn set_user_prefix(
        &self,
        user_id: &str,
        prefix: &str,
    ) -> Result<(), crate::store::StoreError> {
        self.set_user_prefix_impl(user_id, prefix).await
    }

    async fn users_without_prefix(
        &self,
    ) -> Result<Vec<crate::store::models::UserWithoutPrefix>, crate::store::StoreError> {
        self.users_without_prefix_impl().await
    }

    async fn get_personal_task_scope_for_user(
        &self,
        user_id: &str,
    ) -> Result<Option<TaskScopeRecord>, crate::store::StoreError> {
        self.get_personal_task_scope_for_user_impl(user_id).await
    }

    async fn ensure_personal_task_scope_for_user(
        &self,
        user_id: &str,
    ) -> Result<PersonalTaskScopeEnsure, crate::store::StoreError> {
        self.ensure_personal_task_scope_for_user_impl(user_id).await
    }

    async fn lookup_task_scope_by_prefix_for_user(
        &self,
        user_id: &str,
        prefix: &str,
    ) -> Result<Option<TaskScopeRecord>, crate::store::StoreError> {
        self.lookup_task_scope_by_prefix_for_user_impl(user_id, prefix)
            .await
    }

    async fn list_users_pending_personal_task_scope(
        &self,
    ) -> Result<Vec<UserMissingPersonalTaskScope>, crate::store::StoreError> {
        self.list_users_pending_personal_task_scope_impl().await
    }

    async fn backfill_task_key_allocation_task_scope_ids(
        &self,
    ) -> Result<usize, crate::store::StoreError> {
        self.backfill_task_key_allocation_task_scope_ids_impl()
            .await
    }

    async fn count_task_key_allocations_missing_task_scope_id(
        &self,
    ) -> Result<usize, crate::store::StoreError> {
        self.count_task_key_allocations_missing_task_scope_id_impl()
            .await
    }

    async fn lookup_task_uuid_by_task_scope_key(
        &self,
        task_scope_id: &str,
        n: i64,
    ) -> Result<Option<String>, crate::store::StoreError> {
        self.lookup_task_uuid_by_task_scope_key_impl(task_scope_id, n)
            .await
    }

    async fn lookup_task_uuid_by_key(
        &self,
        user_id: &str,
        prefix: &str,
        n: i64,
    ) -> Result<Option<String>, crate::store::StoreError> {
        self.lookup_task_uuid_by_key_impl(user_id, prefix, n).await
    }

    async fn lookup_task_key_by_uuid(
        &self,
        user_id: &str,
        task_uuid: &str,
    ) -> Result<Option<(String, crate::store::models::KeyState)>, crate::store::StoreError> {
        self.lookup_task_key_by_uuid_impl(user_id, task_uuid).await
    }

    async fn lookup_task_keys_by_uuids(
        &self,
        user_id: &str,
        task_uuids: &[String],
    ) -> Result<std::collections::HashMap<String, String>, crate::store::StoreError> {
        self.lookup_task_keys_by_uuids_impl(user_id, task_uuids)
            .await
    }

    async fn lookup_task_keys_for_projection_by_task_scope(
        &self,
        task_scope_id: &str,
        task_uuids: &[String],
        now_unix_seconds: i64,
        pending_timeout_seconds: u32,
    ) -> Result<std::collections::HashMap<String, String>, crate::store::StoreError> {
        self.lookup_task_keys_for_projection_by_task_scope_impl(
            task_scope_id,
            task_uuids,
            now_unix_seconds,
            pending_timeout_seconds,
        )
        .await
    }

    async fn lookup_task_keys_for_projection(
        &self,
        user_id: &str,
        task_uuids: &[String],
        now_unix_seconds: i64,
        pending_timeout_seconds: u32,
    ) -> Result<std::collections::HashMap<String, String>, crate::store::StoreError> {
        self.lookup_task_keys_for_projection_impl(
            user_id,
            task_uuids,
            now_unix_seconds,
            pending_timeout_seconds,
        )
        .await
    }

    async fn lookup_task_keys_for_drift(
        &self,
        user_id: &str,
        task_uuids: &[String],
    ) -> Result<Vec<crate::store::models::DriftAllocationRow>, crate::store::StoreError> {
        self.lookup_task_keys_for_drift_impl(user_id, task_uuids)
            .await
    }

    async fn get_user_task_keys_migrated_at(
        &self,
        user_id: &str,
    ) -> Result<Option<String>, crate::store::StoreError> {
        self.get_user_task_keys_migrated_at_impl(user_id).await
    }

    async fn mark_user_task_keys_migrated(
        &self,
        user_id: &str,
    ) -> Result<(), crate::store::StoreError> {
        self.mark_user_task_keys_migrated_impl(user_id).await
    }

    async fn max_n_for_user_prefix(
        &self,
        user_id: &str,
        prefix: &str,
    ) -> Result<i64, crate::store::StoreError> {
        self.max_n_for_user_prefix_impl(user_id, prefix).await
    }

    async fn commit_backfill_allocations_for_user(
        &self,
        user_id: &str,
        prefix: &str,
        expected_max_n: i64,
        task_uuids_in_order: &[String],
    ) -> Result<Vec<(String, i64)>, crate::store::StoreError> {
        self.commit_backfill_allocations_for_user_impl(
            user_id,
            prefix,
            expected_max_n,
            task_uuids_in_order,
        )
        .await
    }

    async fn list_pending_attached_task_keys_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<crate::store::models::PendingAttachedKey>, crate::store::StoreError> {
        self.list_pending_attached_task_keys_for_user_impl(user_id)
            .await
    }

    async fn create_merged_sync_journal_attempt(
        &self,
        attempt: &crate::store::models::NewMergedSyncJournalAttempt,
    ) -> anyhow::Result<crate::store::models::MergedSyncJournalRecord> {
        self.create_merged_sync_journal_attempt_impl(attempt).await
    }

    async fn transition_merged_sync_journal(
        &self,
        transition: crate::store::models::MergedSyncJournalTransition<'_>,
    ) -> anyhow::Result<Option<crate::store::models::MergedSyncJournalRecord>> {
        self.transition_merged_sync_journal_impl(transition).await
    }

    async fn get_merged_sync_journal(
        &self,
        journal_id: &str,
    ) -> anyhow::Result<Option<crate::store::models::MergedSyncJournalRecord>> {
        self.get_merged_sync_journal_impl(journal_id).await
    }

    async fn list_merged_sync_journal_for_user(
        &self,
        user_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::store::models::MergedSyncJournalRecord>> {
        self.list_merged_sync_journal_for_user_impl(user_id, limit)
            .await
    }

    async fn list_nonterminal_merged_sync_journal_for_user(
        &self,
        user_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::store::models::MergedSyncJournalRecord>> {
        self.list_nonterminal_merged_sync_journal_for_user_impl(user_id, limit)
            .await
    }

    async fn count_merged_sync_journal_states_for_user(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Vec<crate::store::models::MergedSyncJournalStateCount>> {
        self.count_merged_sync_journal_states_for_user_impl(user_id)
            .await
    }

    // --- Migrations ---

    async fn run_migrations(&self) -> anyhow::Result<()> {
        self.run_migrations_impl().await
    }
}

#[async_trait]
impl crate::store::OperatorMaintenanceBackend for SqliteConfigStore {
    async fn checkpoint(&self) -> anyhow::Result<()> {
        self.checkpoint_database_impl().await
    }

    async fn backup_to_path(&self, dst: &Path) -> anyhow::Result<()> {
        self.backup_to_path_impl(dst).await
    }

    async fn restore_from_path(&self, src: &Path) -> anyhow::Result<()> {
        self.restore_from_path_impl(src).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;
    use uuid::Uuid;

    /// Create an in-memory SqliteConfigStore with all tables.
    async fn test_store() -> SqliteConfigStore {
        let store = SqliteConfigStore::new(":memory:").await.unwrap();
        store.run_migrations_inline().await.unwrap();
        store
    }

    #[tokio::test]
    async fn inline_task_key_allocation_scope_schema_matches_migration_032_contract() {
        let store = test_store().await;
        let sql: Vec<String> = store
            .conn
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT sql FROM sqlite_master
                     WHERE type IN ('table', 'index')
                       AND name IN (
                           'task_key_allocations',
                           'idx_task_key_allocations_task_scope_n',
                           'idx_task_key_allocations_task_scope_state'
                       )
                     ORDER BY name",
                )?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .unwrap();
        let joined = sql.join("\n");
        let migration =
            include_str!("../../migrations/032_task_key_allocations_add_task_scope_id.sql");
        for fragment in [
            "task_scope_id TEXT REFERENCES task_scopes(id)",
            "idx_task_key_allocations_task_scope_n",
            "ON task_key_allocations(task_scope_id, n)",
            "WHERE task_scope_id IS NOT NULL",
            "idx_task_key_allocations_task_scope_state",
            "ON task_key_allocations(task_scope_id, state)",
        ] {
            assert!(
                migration.contains(fragment),
                "canonical migration 032 missing fragment: {fragment}"
            );
            assert!(
                joined.contains(fragment),
                "inline task_key_allocations schema missing migration 032 fragment: {fragment}"
            );
        }
    }

    #[tokio::test]
    async fn inline_task_scope_schema_matches_migration_contract() {
        let store = test_store().await;
        let sql: Vec<String> = store
            .conn
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT sql FROM sqlite_master
                     WHERE type IN ('table', 'index')
                       AND name IN (
                           'task_scopes',
                           'idx_task_scopes_personal_owner',
                           'idx_task_scopes_key_prefix_active'
                       )
                     ORDER BY name",
                )?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .unwrap();
        let joined = sql.join("\n");
        let migration = include_str!("../../migrations/031_create_task_scopes.sql");
        for fragment in [
            "task_scopes (",
            "kind TEXT NOT NULL CHECK (kind IN ('personal'))",
            "status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','disabled','deleted'))",
            "CHECK (kind = 'personal' AND owner_runtime_user_id IS NOT NULL AND owner_team_id IS NULL)",
            "FOREIGN KEY(owner_runtime_user_id) REFERENCES users(id) ON DELETE CASCADE",
            "idx_task_scopes_personal_owner",
            "idx_task_scopes_key_prefix_active",
            "WHERE status != 'deleted'",
        ] {
            assert!(
                migration.contains(fragment),
                "canonical migration 031 missing fragment: {fragment}"
            );
            assert!(
                joined.contains(fragment),
                "inline task_scopes schema missing migration fragment: {fragment}"
            );
        }
    }

    /// Create a test user and return (user_id, raw_token).
    async fn create_test_user(store: &SqliteConfigStore) -> (String, String) {
        let user = store
            .create_user(&NewUser {
                username: format!("test_{}", Uuid::new_v4()),
                password_hash: "hash".to_string(),
            })
            .await
            .unwrap();
        let token = store
            .create_api_token(&user.id, Some("test"))
            .await
            .unwrap();
        (user.id, token)
    }

    async fn create_task_key_user(store: &SqliteConfigStore) -> String {
        let (user_id, _) = create_test_user(store).await;
        store.set_user_prefix(&user_id, "WORK").await.unwrap();
        store
            .ensure_personal_task_scope_for_user(&user_id)
            .await
            .unwrap();
        user_id
    }

    #[tokio::test]
    async fn test_expired_token_not_returned() {
        let store = test_store().await;
        let user = store
            .create_user(&NewUser {
                username: "expiry_test".to_string(),
                password_hash: "hash".to_string(),
            })
            .await
            .unwrap();

        // Insert a token with past expiry directly via SQL
        let token = "expired-token-value";
        let token_hash = hash_token(token);
        let user_id = user.id.clone();
        store
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO api_tokens (token_hash, user_id, label, expires_at)
                     VALUES (?1, ?2, 'expired', datetime('now', '-1 hour'))",
                    rusqlite::params![token_hash, user_id],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .unwrap();

        // Lookup should return None because the token is expired
        let result = store.get_user_by_token(token).await.unwrap();
        assert!(result.is_none(), "expired token should not return a user");
    }

    #[tokio::test]
    async fn test_revoke_token() {
        let store = test_store().await;
        let (user_id, token) = create_test_user(&store).await;

        // Token should work before revocation
        let before = store.get_user_by_token(&token).await.unwrap();
        assert!(before.is_some(), "token should be valid before revocation");
        assert_eq!(before.unwrap().id, user_id);

        // Revoke by hash
        let token_hash = hash_token(&token);
        let revoked = store.revoke_api_token(&token_hash).await.unwrap();
        assert!(revoked, "revoke should return true");

        // Token should no longer work
        let after = store.get_user_by_token(&token).await.unwrap();
        assert!(after.is_none(), "revoked token should not return a user");
    }

    #[tokio::test]
    async fn test_create_api_token_with_expiry_supports_compact_tokens() {
        let store = test_store().await;
        let user = store
            .create_user(&NewUser {
                username: "compact-token-test".to_string(),
                password_hash: "hash".to_string(),
            })
            .await
            .unwrap();

        let issued = store
            .create_labeled_api_token(
                &user.id,
                "connect-config",
                "cc_test0000000000",
                "2099-01-01 00:00:00",
                18,
            )
            .await
            .unwrap();
        let token = issued.token.clone();

        assert!(token.len() <= 24, "compact token should fit QR budget");
        assert!(
            !token.contains('='),
            "compact token should be URL-safe without padding"
        );

        let looked_up = store.get_user_by_token(&token).await.unwrap();
        assert_eq!(looked_up.unwrap().id, user.id);

        let first = store
            .mark_token_used(&token, "203.0.113.10", "connect-config")
            .await
            .unwrap()
            .expect("mark_token_used should return a record for an existing connect-config token");
        assert_eq!(first.user_id, user.id);
        assert_eq!(first.token_id, issued.token_id);
        assert_eq!(first.credential_hash_prefix, issued.credential_hash_prefix);
        assert_eq!(first.label.as_deref(), Some("connect-config"));
        assert!(
            first.was_first_use,
            "first call should report was_first_use"
        );

        let second = store
            .mark_token_used(&token, "203.0.113.11", "connect-config")
            .await
            .unwrap()
            .expect("mark_token_used should return a record for an existing connect-config token");
        assert_eq!(second.token_id, issued.token_id);
        assert!(
            !second.was_first_use,
            "second call should report was_first_use=false"
        );

        // Label mismatch returns None and does NOT touch the row —
        // preserves the pre-refactor "regular tokens are read-only on
        // this path" behaviour.
        let mismatch = store
            .mark_token_used(&token, "203.0.113.12", "not-connect-config")
            .await
            .unwrap();
        assert!(
            mismatch.is_none(),
            "label mismatch should return None: {mismatch:?}"
        );

        let token_row = store.list_api_tokens(&user.id).await.unwrap();
        let token_row = token_row
            .into_iter()
            .find(|row| row.label.as_deref() == Some("connect-config"))
            .unwrap();
        assert_eq!(
            token_row.token_id.as_deref(),
            Some(issued.token_id.as_str())
        );
        assert!(token_row.first_used_at.is_some());
        assert!(token_row.last_used_at.is_some());
        assert_eq!(token_row.last_used_ip.as_deref(), Some("203.0.113.11"));
    }

    #[tokio::test]
    async fn test_delete_user_removes_tokens() {
        let store = test_store().await;
        let (user_id, token) = create_test_user(&store).await;

        // Token works before deletion
        assert!(store.get_user_by_token(&token).await.unwrap().is_some());

        // Delete user
        let deleted = store.delete_user(&user_id).await.unwrap();
        assert!(deleted, "delete_user should return true");

        // Token lookup should fail (user and tokens deleted)
        let after = store.get_user_by_token(&token).await.unwrap();
        assert!(after.is_none(), "token should be gone after user deletion");

        // User should be gone too
        let user = store.get_user_by_id(&user_id).await.unwrap();
        assert!(user.is_none(), "user should be gone after deletion");
    }

    #[tokio::test]
    async fn test_create_and_query_replicas() {
        let store = test_store().await;
        let (user_id, _token) = create_test_user(&store).await;

        let client_id = Uuid::new_v4().to_string();
        let enc_secret = "encrypted-secret-base64";

        // Create replica
        store
            .create_replica(&user_id, &client_id, enc_secret)
            .await
            .unwrap();

        // get_replica_by_user should return it
        let replica = store.get_replica_by_user(&user_id).await.unwrap();
        assert!(replica.is_some());
        let replica = replica.unwrap();
        assert_eq!(replica.id, client_id);
        assert_eq!(replica.user_id, user_id);
        assert_eq!(replica.encryption_secret_enc, enc_secret);
        assert_eq!(replica.label, "Personal");

        // get_replica_by_client_id should return it
        let replica = store.get_replica_by_client_id(&client_id).await.unwrap();
        assert!(replica.is_some());
        assert_eq!(replica.unwrap().user_id, user_id);

        // get_user_by_client_id should resolve to the user
        let user = store.get_user_by_client_id(&client_id).await.unwrap();
        assert!(user.is_some());
        assert_eq!(user.unwrap().id, user_id);

        // Delete replica
        let deleted = store.delete_replica(&user_id).await.unwrap();
        assert!(deleted);

        // Should be gone now
        let replica = store.get_replica_by_user(&user_id).await.unwrap();
        assert!(
            replica.is_none(),
            "replica should be removed after deletion"
        );

        // Lookup by client_id should return None
        let user = store.get_user_by_client_id(&client_id).await.unwrap();
        assert!(user.is_none());
    }

    #[tokio::test]
    async fn test_one_replica_per_user() {
        let store = test_store().await;
        let (user_id, _token) = create_test_user(&store).await;

        let client_id1 = Uuid::new_v4().to_string();
        let client_id2 = Uuid::new_v4().to_string();

        store
            .create_replica(&user_id, &client_id1, "enc1")
            .await
            .unwrap();

        // Second create for same user should fail (UNIQUE constraint on user_id)
        let result = store.create_replica(&user_id, &client_id2, "enc2").await;
        assert!(result.is_err(), "second replica for same user should fail");
    }

    #[tokio::test]
    async fn test_revoke_nonexistent_token_returns_false() {
        let store = test_store().await;
        let result = store.revoke_api_token("nonexistent-hash").await.unwrap();
        assert!(!result, "revoking nonexistent token should return false");
    }

    #[tokio::test]
    async fn test_delete_nonexistent_user_returns_false() {
        let store = test_store().await;
        let result = store.delete_user("nonexistent-id").await.unwrap();
        assert!(!result, "deleting nonexistent user should return false");
    }

    #[tokio::test]
    async fn test_delete_user_cascades_tokens_and_replicas() {
        let store = test_store().await;
        let (user_id, token) = create_test_user(&store).await;

        // Create a replica for this user
        let client_id = Uuid::new_v4().to_string();
        store
            .create_replica(&user_id, &client_id, "enc-secret")
            .await
            .unwrap();

        // Verify token and replica exist
        assert!(
            store.get_user_by_token(&token).await.unwrap().is_some(),
            "token should exist before deletion"
        );
        assert!(
            store
                .get_user_by_client_id(&client_id)
                .await
                .unwrap()
                .is_some(),
            "replica should exist before deletion"
        );
        assert!(store.get_replica_by_user(&user_id).await.unwrap().is_some());

        // Delete user — should cascade to tokens and replicas
        let deleted = store.delete_user(&user_id).await.unwrap();
        assert!(deleted, "delete_user should return true");

        // Verify everything is gone
        assert!(
            store.get_user_by_token(&token).await.unwrap().is_none(),
            "token should be gone after user deletion"
        );
        assert!(
            store
                .get_user_by_client_id(&client_id)
                .await
                .unwrap()
                .is_none(),
            "replica should be gone after user deletion"
        );
        assert!(
            store.list_api_tokens(&user_id).await.unwrap().is_empty(),
            "api_tokens should be empty after user deletion"
        );
        assert!(
            store.get_replica_by_user(&user_id).await.unwrap().is_none(),
            "replicas should be empty after user deletion"
        );
        assert!(
            store.get_user_by_id(&user_id).await.unwrap().is_none(),
            "user should be gone after deletion"
        );
    }

    /// DR scenario: delete_user on a restored backup from an older schema version
    /// where replicas table doesn't exist yet. The "no such table" branch in
    /// delete_user must handle this gracefully without crashing.
    #[tokio::test]
    async fn test_delete_user_missing_replicas_table() {
        // Create a store with only users + api_tokens (simulating old schema)
        let store = SqliteConfigStore::new(":memory:").await.unwrap();
        store
            .conn
            .call(|conn| {
                conn.execute_batch(
                    "CREATE TABLE users (
                    id TEXT PRIMARY KEY,
                    username TEXT UNIQUE NOT NULL,
                    password_hash TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE api_tokens (
                    token_hash TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL REFERENCES users(id),
                    label TEXT,
                    expires_at TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            })
            .await
            .unwrap();

        // Create a user directly
        store.conn.call(|conn| {
            conn.execute(
                "INSERT INTO users (id, username, password_hash) VALUES ('dr-user', 'drtest', 'hash')",
                [],
            )?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }).await.unwrap();

        // delete_user should NOT crash even though replicas table is missing
        let result = store.delete_user("dr-user").await;
        assert!(
            result.is_ok(),
            "delete_user should handle missing replicas table: {:?}",
            result.err()
        );
        assert!(result.unwrap(), "user should have been deleted");

        // Verify user is actually gone
        let user = store
            .conn
            .call(|conn| {
                let exists: bool = conn.query_row(
                    "SELECT COUNT(*) > 0 FROM users WHERE id = 'dr-user'",
                    [],
                    |row| row.get(0),
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(exists)
            })
            .await
            .unwrap();
        assert!(!user, "user should be removed from DB");
    }

    /// DR scenario: delete_user on a backup missing views/contexts/presets/stores tables.
    /// Simulates a very old backup where only users + api_tokens exist.
    #[tokio::test]
    async fn test_delete_user_missing_all_config_tables() {
        let store = SqliteConfigStore::new(":memory:").await.unwrap();
        store
            .conn
            .call(|conn| {
                conn.execute_batch(
                    "CREATE TABLE users (
                    id TEXT PRIMARY KEY,
                    username TEXT UNIQUE NOT NULL,
                    password_hash TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE api_tokens (
                    token_hash TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL REFERENCES users(id),
                    label TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            })
            .await
            .unwrap();

        store.conn.call(|conn| {
            conn.execute(
                "INSERT INTO users (id, username, password_hash) VALUES ('old-user', 'oldtest', 'hash')",
                [],
            )?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }).await.unwrap();

        // Should not crash even though views, contexts, presets, stores, replicas, sync_clients are all missing
        let result = store.delete_user("old-user").await;
        assert!(
            result.is_ok(),
            "delete_user should handle missing config tables: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_device_revoke_unrevoke_delete_lifecycle() {
        let store = test_store().await;
        let (user_id, _token) = create_test_user(&store).await;
        let client_id = Uuid::new_v4().to_string();

        store
            .create_device(
                &user_id,
                &client_id,
                "Test device",
                Some("enc-device-secret"),
            )
            .await
            .unwrap();

        let device = store.get_device(&client_id).await.unwrap().unwrap();
        assert_eq!(device.status, "active");
        assert_eq!(device.name, "Test device");

        let revoked = store.revoke_device(&user_id, &client_id).await.unwrap();
        assert!(revoked, "revoke_device should return true");
        let device = store.get_device(&client_id).await.unwrap().unwrap();
        assert_eq!(device.status, "revoked");

        let unrevoked = store.unrevoke_device(&user_id, &client_id).await.unwrap();
        assert!(unrevoked, "unrevoke_device should return true");
        let device = store.get_device(&client_id).await.unwrap().unwrap();
        assert_eq!(device.status, "active");

        let deleted = store.delete_device(&user_id, &client_id).await.unwrap();
        assert!(deleted, "delete_device should return true");
        assert!(
            store.get_device(&client_id).await.unwrap().is_none(),
            "device should be gone after delete"
        );
    }

    // --- StoreError resource-label mapping (ADR-0002 P4 sub-fix 3) ---

    use crate::store::error::resources;
    use crate::store::{ConstraintKind, StoreError};

    #[tokio::test]
    async fn store_err_users_username() {
        let store = test_store().await;
        store
            .create_user(&NewUser {
                username: "dup".into(),
                password_hash: String::new(),
            })
            .await
            .unwrap();
        let err = store
            .create_user(&NewUser {
                username: "dup".into(),
                password_hash: String::new(),
            })
            .await
            .expect_err("second insert must fail");
        assert!(
            matches!(
                err,
                StoreError::Constraint(ConstraintKind::Unique { resource })
                    if resource == resources::USERS_USERNAME
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn store_err_replicas_user_id() {
        let store = test_store().await;
        let (user_id, _) = create_test_user(&store).await;
        store
            .create_replica(&user_id, &Uuid::new_v4().to_string(), "enc1")
            .await
            .unwrap();
        let err = store
            .create_replica(&user_id, &Uuid::new_v4().to_string(), "enc2")
            .await
            .expect_err("second replica for same user must fail");
        assert!(
            matches!(
                err,
                StoreError::Constraint(ConstraintKind::Unique { resource })
                    if resource == resources::REPLICAS_USER_ID
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn store_err_devices_bootstrap_request_id() {
        let store = test_store().await;
        let (user_id, _) = create_test_user(&store).await;
        let req_id = Uuid::new_v4().to_string();
        store
            .create_bootstrap_device(
                &user_id,
                &Uuid::new_v4().to_string(),
                "first",
                "enc1",
                &req_id,
                None,
                false,
                "2099-01-01 00:00:00",
            )
            .await
            .unwrap();
        let err = store
            .create_bootstrap_device(
                &user_id,
                &Uuid::new_v4().to_string(),
                "second",
                "enc2",
                &req_id,
                None,
                false,
                "2099-01-01 00:00:00",
            )
            .await
            .expect_err("duplicate bootstrap_request_id must fail");
        assert!(
            matches!(
                err,
                StoreError::Constraint(ConstraintKind::Unique { resource })
                    if resource == resources::DEVICES_BOOTSTRAP_REQUEST_ID
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn store_err_webhooks_user_url() {
        let store = test_store().await;
        let (user_id, _) = create_test_user(&store).await;
        let url = "https://hooks.example.invalid/dup".to_string();
        store
            .create_webhook(
                &NewWebhookRecord {
                    id: Uuid::new_v4().to_string(),
                    user_id: user_id.clone(),
                    url: url.clone(),
                    events: vec!["task.created".into()],
                    modified_fields: None,
                    name: None,
                    enabled: true,
                    secret_enc: "enc".into(),
                },
                100,
            )
            .await
            .unwrap();
        let err = store
            .create_webhook(
                &NewWebhookRecord {
                    id: Uuid::new_v4().to_string(),
                    user_id,
                    url,
                    events: vec!["task.created".into()],
                    modified_fields: None,
                    name: None,
                    enabled: true,
                    secret_enc: "enc".into(),
                },
                100,
            )
            .await
            .expect_err("duplicate (user_id, url) must fail");
        assert!(
            matches!(
                err,
                StoreError::Constraint(ConstraintKind::Unique { resource })
                    if resource == resources::WEBHOOKS_USER_URL
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn store_err_admin_webhooks_url() {
        let store = test_store().await;
        let url = "https://hooks.example.invalid/admin-dup".to_string();
        store
            .create_admin_webhook(
                &NewAdminWebhookRecord {
                    id: Uuid::new_v4().to_string(),
                    url: url.clone(),
                    events: vec!["task.created".into()],
                    modified_fields: None,
                    name: None,
                    enabled: true,
                    secret_enc: "enc".into(),
                },
                100,
            )
            .await
            .unwrap();
        let err = store
            .create_admin_webhook(
                &NewAdminWebhookRecord {
                    id: Uuid::new_v4().to_string(),
                    url,
                    events: vec!["task.created".into()],
                    modified_fields: None,
                    name: None,
                    enabled: true,
                    secret_enc: "enc".into(),
                },
                100,
            )
            .await
            .expect_err("duplicate admin_webhooks.url must fail");
        assert!(
            matches!(
                err,
                StoreError::Constraint(ConstraintKind::Unique { resource })
                    if resource == resources::ADMIN_WEBHOOKS_URL
            ),
            "got {err:?}"
        );
    }

    /// Pin the constraint-message → resource mapping for `users.prefix` and
    /// `task_key_allocations.*` *before* C2 lands `set_user_prefix` /
    /// `reserve_task_key_pending`. SQLite's UNIQUE-constraint message format
    /// is fragile (subtle differences in column-list rendering between
    /// composite indexes vs primary keys); these tests catch typos in the
    /// `rusqlite_unique_resource` mapping that would otherwise only surface
    /// once the higher-level methods exist.
    #[tokio::test]
    async fn store_err_users_prefix() {
        let store = test_store().await;
        store
            .create_user(&NewUser {
                username: "alice".into(),
                password_hash: String::new(),
            })
            .await
            .unwrap();
        store
            .create_user(&NewUser {
                username: "bob".into(),
                password_hash: String::new(),
            })
            .await
            .unwrap();
        store
            .conn
            .call(|conn| {
                conn.execute(
                    "UPDATE users SET prefix = 'WORK' WHERE username = 'alice'",
                    [],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .unwrap();
        let result: Result<(), StoreError> = store
            .conn
            .call(|conn| {
                conn.execute(
                    "UPDATE users SET prefix = 'WORK' WHERE username = 'bob'",
                    [],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow);
        let err = result.expect_err("duplicate users.prefix must fail");
        assert!(
            matches!(
                err,
                StoreError::Constraint(ConstraintKind::Unique { resource })
                    if resource == resources::USERS_PREFIX
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn store_err_task_key_allocations_user_prefix_n() {
        let store = test_store().await;
        let (user_id, _) = create_test_user(&store).await;
        store
            .conn
            .call({
                let user_id = user_id.clone();
                move |conn| {
                    conn.execute(
                        "INSERT INTO task_key_allocations (user_id, prefix, n, state, attempt_id) VALUES (?1, 'WORK', 1, 'pending', 'a')",
                        [&user_id],
                    )?;
                    Ok::<_, BoxErr>(())
                }
            })
            .await
            .unwrap();
        let result: Result<(), StoreError> = store
            .conn
            .call({
                let user_id = user_id.clone();
                move |conn| {
                    conn.execute(
                        "INSERT INTO task_key_allocations (user_id, prefix, n, state, attempt_id) VALUES (?1, 'WORK', 1, 'pending', 'b')",
                        [&user_id],
                    )?;
                    Ok::<_, BoxErr>(())
                }
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow);
        let err = result.expect_err("duplicate (user_id,prefix,n) must fail");
        assert!(
            matches!(
                err,
                StoreError::Constraint(ConstraintKind::Unique { resource })
                    if resource == resources::TASK_KEY_ALLOCATIONS_USER_PREFIX_N
            ),
            "got {err:?}"
        );
    }

    // --- task-key allocation primitives (#130 C2) ---

    #[tokio::test]
    async fn migration_032_backfills_task_scope_id_for_all_allocation_states() {
        let store = SqliteConfigStore::new(":memory:").await.unwrap();
        store
            .conn
            .call(|conn| {
                conn.execute_batch(
                    "PRAGMA foreign_keys=ON;
                     CREATE TABLE users (
                        id TEXT PRIMARY KEY,
                        username TEXT UNIQUE NOT NULL,
                        password_hash TEXT NOT NULL,
                        created_at TEXT NOT NULL DEFAULT (datetime('now')),
                        prefix TEXT,
                        task_keys_migrated_at TEXT
                     );
                     CREATE TABLE task_scopes (
                        id TEXT PRIMARY KEY,
                        kind TEXT NOT NULL CHECK (kind IN ('personal')),
                        owner_runtime_user_id TEXT,
                        owner_team_id TEXT,
                        key_prefix TEXT NOT NULL,
                        status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','disabled','deleted')),
                        storage_path TEXT,
                        created_at TEXT NOT NULL DEFAULT (datetime('now')),
                        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                     );
                     CREATE TABLE task_key_allocations (
                        user_id TEXT NOT NULL,
                        prefix TEXT NOT NULL,
                        n INTEGER NOT NULL,
                        task_uuid TEXT,
                        state TEXT NOT NULL CHECK (state IN ('pending','committed','burned')),
                        attempt_id TEXT NOT NULL,
                        created_at TEXT NOT NULL DEFAULT (datetime('now')),
                        committed_at TEXT,
                        PRIMARY KEY (user_id, prefix, n)
                     );
                     INSERT INTO users (id, username, password_hash, prefix)
                     VALUES ('u1', 'user1', 'hash', 'WORK');
                     INSERT INTO task_scopes (id, kind, owner_runtime_user_id, key_prefix, status)
                     VALUES ('ts_personal', 'personal', 'u1', 'WORK', 'active');
                     INSERT INTO task_key_allocations (user_id, prefix, n, task_uuid, state, attempt_id)
                     VALUES
                        ('u1', 'WORK', 1, 'uuid-pending', 'pending', 'a1'),
                        ('u1', 'WORK', 2, 'uuid-committed', 'committed', 'a2'),
                        ('u1', 'WORK', 3, NULL, 'burned', 'a3');",
                )?;
                conn.execute_batch(include_str!("../../migrations/032_task_key_allocations_add_task_scope_id.sql"))?;
                Ok::<_, BoxErr>(())
            })
            .await
            .unwrap();

        let (missing, stamped, has_scope_n, has_scope_state): (i64, i64, bool, bool) = store
            .conn
            .call(|conn| {
                let missing: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM task_key_allocations WHERE task_scope_id IS NULL",
                    [],
                    |r| r.get(0),
                )?;
                let stamped: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM task_key_allocations WHERE task_scope_id = 'ts_personal'",
                    [],
                    |r| r.get(0),
                )?;
                let mut stmt = conn.prepare("PRAGMA index_list('task_key_allocations')")?;
                let names = stmt
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>((
                    missing,
                    stamped,
                    names
                        .iter()
                        .any(|n| n == "idx_task_key_allocations_task_scope_n"),
                    names
                        .iter()
                        .any(|n| n == "idx_task_key_allocations_task_scope_state"),
                ))
            })
            .await
            .unwrap();
        assert_eq!(missing, 0);
        assert_eq!(stamped, 3, "pending/committed/burned rows all backfilled");
        assert!(has_scope_n);
        assert!(has_scope_state);
    }

    #[tokio::test]
    async fn task_key_task_scope_backfill_is_idempotent() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (n1, attempt1) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let task_uuid = Uuid::new_v4().to_string();
        store
            .attach_task_uuid_to_pending(&user_id, "WORK", n1, &attempt1, &task_uuid)
            .await
            .unwrap();
        store
            .commit_task_key(&user_id, "WORK", n1, &attempt1)
            .await
            .unwrap();
        let (n2, attempt2) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        store
            .burn_task_key(&user_id, "WORK", n2, &attempt2)
            .await
            .unwrap();
        store
            .conn
            .call(|conn| {
                conn.execute("UPDATE task_key_allocations SET task_scope_id = NULL", [])?;
                Ok::<_, BoxErr>(())
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .count_task_key_allocations_missing_task_scope_id()
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .backfill_task_key_allocation_task_scope_ids()
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .count_task_key_allocations_missing_task_scope_id()
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .backfill_task_key_allocation_task_scope_ids()
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn task_keys_reserve_increments_n_within_user_prefix() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (n1, attempt1) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let (n2, attempt2) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        assert_eq!(n1, 1);
        assert_eq!(n2, 2);
        assert_ne!(attempt1, attempt2, "attempt_ids must be unique");
    }

    #[tokio::test]
    async fn task_keys_burned_rows_persist_so_max_n_does_not_reuse() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (_n1, attempt1) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        store
            .burn_task_key(&user_id, "WORK", 1, &attempt1)
            .await
            .unwrap();
        let (n2, _) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        assert_eq!(n2, 2, "next reservation must skip burned N=1");
    }

    // Pre-Task-Scope tests allowed multiple prefixes per user and asserted
    // independent counters. S2 narrows the compatibility wrapper to the user's
    // active Personal Task Scope prefix; future multi-scope support must route
    // via explicit Task Scope membership, not arbitrary per-user prefixes.
    #[tokio::test]
    async fn task_keys_rejects_prefix_without_active_personal_task_scope() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (n_work, _) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let err = store
            .reserve_task_key_pending(&user_id, "HOME")
            .await
            .expect_err("S2 allocation must not insert without resolved Task Scope");
        assert_eq!(n_work, 1);
        assert!(
            matches!(
                err,
                StoreError::AllocationTaskScopeMissing { user_id: ref err_user, prefix: ref err_prefix }
                    if err_user == &user_id && err_prefix == "HOME"
            ),
            "got {err:?}"
        );
        assert_eq!(
            store
                .count_task_key_allocations_missing_task_scope_id()
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn task_keys_new_reservations_write_task_scope_id() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let scope = store
            .get_personal_task_scope_for_user(&user_id)
            .await
            .unwrap()
            .expect("set-prefix must materialise Personal Task Scope");

        let (n, _) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let stamped: Option<String> = store
            .conn
            .call(move |conn| {
                let stamped = conn
                    .query_row(
                        "SELECT task_scope_id FROM task_key_allocations
                         WHERE user_id = ?1 AND prefix = 'WORK' AND n = ?2",
                        rusqlite::params![user_id, n],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok::<_, BoxErr>(stamped)
            })
            .await
            .unwrap();
        assert_eq!(stamped.as_deref(), Some(scope.id.as_str()));
    }

    #[tokio::test]
    async fn task_keys_attach_then_commit_round_trip() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (n, attempt) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let task_uuid = Uuid::new_v4().to_string();
        store
            .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid)
            .await
            .unwrap();
        store
            .commit_task_key(&user_id, "WORK", n, &attempt)
            .await
            .unwrap();

        // Lookup primitives now resolve.
        let resolved_uuid = store
            .lookup_task_uuid_by_key(&user_id, "WORK", n)
            .await
            .unwrap();
        assert_eq!(resolved_uuid.as_deref(), Some(task_uuid.as_str()));

        let (key, state) = store
            .lookup_task_key_by_uuid(&user_id, &task_uuid)
            .await
            .unwrap()
            .expect("committed row must resolve");
        assert_eq!(key, format!("WORK-{n}"));
        assert!(matches!(state, crate::store::models::KeyState::Committed));
    }

    #[tokio::test]
    async fn task_keys_lookup_uuid_routes_by_visible_task_scope() {
        let store = test_store().await;
        let user_a = create_task_key_user(&store).await;
        let (user_b, _) = create_test_user(&store).await;
        store.set_user_prefix(&user_b, "HOME").await.unwrap();
        let scope_b = store
            .ensure_personal_task_scope_for_user(&user_b)
            .await
            .unwrap()
            .scope;

        let (n, attempt) = store
            .reserve_task_key_pending(&user_a, "WORK")
            .await
            .unwrap();
        let task_uuid = Uuid::new_v4().to_string();
        store
            .attach_task_uuid_to_pending(&user_a, "WORK", n, &attempt, &task_uuid)
            .await
            .unwrap();
        store
            .commit_task_key(&user_a, "WORK", n, &attempt)
            .await
            .unwrap();

        // Simulate a future/global-prefix world or corruption where the
        // compatibility columns still point at user_a but the logical Task
        // Scope column does not. S3 key resolution must trust task_scope_id.
        store
            .conn
            .call({
                let user_a = user_a.clone();
                let scope_b_id = scope_b.id.clone();
                move |conn| {
                    conn.execute(
                        "UPDATE task_key_allocations
                            SET task_scope_id = ?1
                          WHERE user_id = ?2 AND prefix = 'WORK' AND n = ?3",
                        rusqlite::params![scope_b_id, user_a, n],
                    )?;
                    Ok::<_, BoxErr>(())
                }
            })
            .await
            .unwrap();

        assert!(store
            .lookup_task_uuid_by_key(&user_a, "WORK", n)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .lookup_task_uuid_by_key(&user_b, "WORK", n)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .lookup_task_uuid_by_task_scope_key(&scope_b.id, n)
                .await
                .unwrap()
                .as_deref(),
            Some(task_uuid.as_str())
        );
    }

    #[tokio::test]
    async fn task_keys_projection_routes_by_task_scope_id() {
        let store = test_store().await;
        let user_a = create_task_key_user(&store).await;
        let (user_b, _) = create_test_user(&store).await;
        store.set_user_prefix(&user_b, "HOME").await.unwrap();
        let scope_b = store
            .ensure_personal_task_scope_for_user(&user_b)
            .await
            .unwrap()
            .scope;

        let (n, attempt) = store
            .reserve_task_key_pending(&user_a, "WORK")
            .await
            .unwrap();
        let task_uuid = Uuid::new_v4().to_string();
        store
            .attach_task_uuid_to_pending(&user_a, "WORK", n, &attempt, &task_uuid)
            .await
            .unwrap();
        store
            .commit_task_key(&user_a, "WORK", n, &attempt)
            .await
            .unwrap();
        store
            .conn
            .call({
                let user_a = user_a.clone();
                let scope_b_id = scope_b.id.clone();
                move |conn| {
                    conn.execute(
                        "UPDATE task_key_allocations
                            SET task_scope_id = ?1
                          WHERE user_id = ?2 AND prefix = 'WORK' AND n = ?3",
                        rusqlite::params![scope_b_id, user_a, n],
                    )?;
                    Ok::<_, BoxErr>(())
                }
            })
            .await
            .unwrap();

        let uuids = vec![task_uuid.clone()];
        assert!(store
            .lookup_task_keys_for_projection(&user_a, &uuids, 1_900_000_000, 300)
            .await
            .unwrap()
            .is_empty());
        let projected = store
            .lookup_task_keys_for_projection_by_task_scope(&scope_b.id, &uuids, 1_900_000_000, 300)
            .await
            .unwrap();
        let expected_key = format!("WORK-{n}");
        assert_eq!(
            projected.get(&task_uuid).map(String::as_str),
            Some(expected_key.as_str())
        );
    }

    #[tokio::test]
    async fn task_keys_scope_routed_reads_gate_missing_task_scope_id() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (n, attempt) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let task_uuid = Uuid::new_v4().to_string();
        store
            .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid)
            .await
            .unwrap();
        store
            .commit_task_key(&user_id, "WORK", n, &attempt)
            .await
            .unwrap();
        store
            .conn
            .call({
                let user_id = user_id.clone();
                move |conn| {
                    conn.execute(
                        "UPDATE task_key_allocations
                            SET task_scope_id = NULL
                          WHERE user_id = ?1 AND prefix = 'WORK' AND n = ?2",
                        rusqlite::params![user_id, n],
                    )?;
                    Ok::<_, BoxErr>(())
                }
            })
            .await
            .unwrap();

        let err = store
            .lookup_task_uuid_by_key(&user_id, "WORK", n)
            .await
            .expect_err("S3 lookup must stop before Task Scope routing if S2 invariant is broken");
        assert!(
            matches!(err, StoreError::MissingTaskScopeId { user_id: ref err_user, count: 1 } if err_user == &user_id),
            "got {err:?}"
        );

        let err = store
            .lookup_task_keys_for_projection(&user_id, &[task_uuid], 1_900_000_000, 300)
            .await
            .expect_err(
                "S3 projection must stop before Task Scope routing if S2 invariant is broken",
            );
        assert!(
            matches!(err, StoreError::MissingTaskScopeId { user_id: ref err_user, count: 1 } if err_user == &user_id),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn task_keys_attach_stale_attempt_returns_stale_finalizer() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (n, _attempt) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let task_uuid = Uuid::new_v4().to_string();
        let err = store
            .attach_task_uuid_to_pending(&user_id, "WORK", n, "wrong-attempt", &task_uuid)
            .await
            .expect_err("stale attempt_id must reject");
        assert!(matches!(err, StoreError::AllocationStaleFinalizer));
    }

    #[tokio::test]
    async fn task_keys_commit_idempotent_on_already_committed_same_attempt() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (n, attempt) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let task_uuid = Uuid::new_v4().to_string();
        store
            .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid)
            .await
            .unwrap();
        store
            .commit_task_key(&user_id, "WORK", n, &attempt)
            .await
            .unwrap();
        // Second commit with same attempt_id is the reaper-race load-bearing
        // case — must succeed silently.
        store
            .commit_task_key(&user_id, "WORK", n, &attempt)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn task_keys_commit_with_wrong_attempt_returns_stale_finalizer() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (n, attempt) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let task_uuid = Uuid::new_v4().to_string();
        store
            .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid)
            .await
            .unwrap();
        let err = store
            .commit_task_key(&user_id, "WORK", n, "wrong-attempt")
            .await
            .expect_err("stale finaliser must reject");
        assert!(matches!(err, StoreError::AllocationStaleFinalizer));
    }

    #[tokio::test]
    async fn task_keys_burn_idempotent_on_already_burned_same_attempt() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (n, attempt) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        store
            .burn_task_key(&user_id, "WORK", n, &attempt)
            .await
            .unwrap();
        // Idempotent: same attempt_id can call burn twice.
        store
            .burn_task_key(&user_id, "WORK", n, &attempt)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn task_keys_set_user_prefix_locks_after_allocation() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        // Reserve → set-prefix should now fail with PrefixLocked.
        let _ = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let err = store
            .set_user_prefix(&user_id, "OTHER")
            .await
            .expect_err("must reject after allocation exists");
        assert!(matches!(err, StoreError::PrefixLocked), "got {err:?}");
    }

    #[tokio::test]
    async fn task_keys_set_user_prefix_collision_returns_unique_constraint() {
        let store = test_store().await;
        let (user_a, _) = create_test_user(&store).await;
        let (user_b, _) = create_test_user(&store).await;
        store.set_user_prefix(&user_a, "WORK").await.unwrap();
        let err = store
            .set_user_prefix(&user_b, "WORK")
            .await
            .expect_err("server-wide prefix uniqueness must enforce");
        assert!(
            matches!(
                err,
                StoreError::Constraint(ConstraintKind::Unique { resource })
                    if resource == resources::USERS_PREFIX
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn task_keys_get_user_prefix_round_trips() {
        let store = test_store().await;
        let (user_id, _) = create_test_user(&store).await;
        assert!(store.get_user_prefix(&user_id).await.unwrap().is_none());
        store.set_user_prefix(&user_id, "WORK").await.unwrap();
        assert_eq!(
            store.get_user_prefix(&user_id).await.unwrap().as_deref(),
            Some("WORK")
        );
    }

    #[tokio::test]
    async fn task_keys_users_without_prefix_lists_unmigrated() {
        let store = test_store().await;
        let (user_a, _) = create_test_user(&store).await;
        let (user_b, _) = create_test_user(&store).await;
        store.set_user_prefix(&user_a, "WORK").await.unwrap();
        let pending = store.users_without_prefix().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, user_b);
    }

    #[tokio::test]
    async fn task_keys_lookup_uuid_by_key_skips_pending_and_burned() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (n, attempt) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let task_uuid = Uuid::new_v4().to_string();
        store
            .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid)
            .await
            .unwrap();
        // Pending: lookup must return None.
        assert!(store
            .lookup_task_uuid_by_key(&user_id, "WORK", n)
            .await
            .unwrap()
            .is_none());
        // Burn (without committing) — still None.
        store
            .burn_task_key(&user_id, "WORK", n, &attempt)
            .await
            .unwrap();
        assert!(store
            .lookup_task_uuid_by_key(&user_id, "WORK", n)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn task_keys_lookup_keys_by_uuids_batch() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;

        let mut want: Vec<(String, String)> = Vec::new();
        for _ in 0..5 {
            let (n, attempt) = store
                .reserve_task_key_pending(&user_id, "WORK")
                .await
                .unwrap();
            let uuid = Uuid::new_v4().to_string();
            store
                .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &uuid)
                .await
                .unwrap();
            store
                .commit_task_key(&user_id, "WORK", n, &attempt)
                .await
                .unwrap();
            want.push((uuid, format!("WORK-{n}")));
        }

        let uuids: Vec<String> = want.iter().map(|(u, _)| u.clone()).collect();
        let map = store
            .lookup_task_keys_by_uuids(&user_id, &uuids)
            .await
            .unwrap();
        assert_eq!(map.len(), 5);
        for (uuid, key) in &want {
            assert_eq!(map.get(uuid).map(String::as_str), Some(key.as_str()));
        }

        // Empty-input contract — no DB call needed, returns empty map.
        let empty = store
            .lookup_task_keys_by_uuids(&user_id, &[])
            .await
            .unwrap();
        assert!(empty.is_empty());

        // Unknown UUID — silently skipped (caller treats as no key).
        let unknown = Uuid::new_v4().to_string();
        let mixed = store
            .lookup_task_keys_by_uuids(&user_id, &[unknown.clone()])
            .await
            .unwrap();
        assert!(mixed.is_empty());
    }

    #[tokio::test]
    async fn task_keys_select_stale_pending_filters_by_age() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (_n_old, _) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        // Forge `created_at` 600s in the past to trip a 300s timeout.
        store
            .conn
            .call(|conn| {
                conn.execute(
                    "UPDATE task_key_allocations SET created_at = datetime('now', '-600 seconds')",
                    [],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .unwrap();
        // Add a fresh pending row that should NOT be selected.
        let (_, _) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();

        let now: i64 = store
            .conn
            .call(|conn| {
                let v: i64 =
                    conn.query_row("SELECT CAST(strftime('%s', 'now') AS INTEGER)", [], |r| {
                        r.get(0)
                    })?;
                Ok::<_, BoxErr>(v)
            })
            .await
            .unwrap();
        let stale = store
            .select_stale_pending_task_keys(now, 300, 100)
            .await
            .unwrap();
        assert_eq!(stale.len(), 1, "only the forged-old row is stale");
        assert_eq!(stale[0].user_id, user_id);
        assert_eq!(stale[0].n, 1);
    }

    #[tokio::test]
    async fn task_keys_attach_double_uuid_collision_partial_index_fires() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let (n1, attempt1) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let (n2, attempt2) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();
        let task_uuid = Uuid::new_v4().to_string();
        store
            .attach_task_uuid_to_pending(&user_id, "WORK", n1, &attempt1, &task_uuid)
            .await
            .unwrap();
        let err = store
            .attach_task_uuid_to_pending(&user_id, "WORK", n2, &attempt2, &task_uuid)
            .await
            .expect_err("partial unique index on task_uuid must fire");
        assert!(
            matches!(
                err,
                StoreError::Constraint(ConstraintKind::Unique { resource })
                    if resource == resources::TASK_KEY_ALLOCATIONS_TASK_UUID
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn store_err_task_key_allocations_task_scope_n() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let scope = store
            .get_personal_task_scope_for_user(&user_id)
            .await
            .unwrap()
            .unwrap();
        let (n, _) = store
            .reserve_task_key_pending(&user_id, "WORK")
            .await
            .unwrap();

        let result: Result<(), StoreError> = store
            .conn
            .call({
                let scope_id = scope.id.clone();
                move |conn| {
                    conn.execute(
                        "INSERT INTO task_key_allocations
                            (user_id, prefix, n, task_scope_id, state, attempt_id)
                         VALUES ('other-user', 'OTHER', ?1, ?2, 'pending', 'b')",
                        rusqlite::params![n, scope_id],
                    )?;
                    Ok::<_, BoxErr>(())
                }
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow);
        let err = result.expect_err("duplicate (task_scope_id,n) must fail");
        assert!(
            matches!(
                err,
                StoreError::Constraint(ConstraintKind::Unique { resource })
                    if resource == resources::TASK_KEY_ALLOCATIONS_TASK_SCOPE_N
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn store_err_task_key_allocations_task_uuid() {
        let store = test_store().await;
        let user_id = create_task_key_user(&store).await;
        let task_uuid = Uuid::new_v4().to_string();
        store
            .conn
            .call({
                let user_id = user_id.clone();
                let task_uuid = task_uuid.clone();
                move |conn| {
                    conn.execute(
                        "INSERT INTO task_key_allocations (user_id, prefix, n, task_uuid, state, attempt_id) VALUES (?1, 'WORK', 1, ?2, 'pending', 'a')",
                        [&user_id, &task_uuid],
                    )?;
                    Ok::<_, BoxErr>(())
                }
            })
            .await
            .unwrap();
        let result: Result<(), StoreError> = store
            .conn
            .call({
                let user_id = user_id.clone();
                let task_uuid = task_uuid.clone();
                move |conn| {
                    conn.execute(
                        "INSERT INTO task_key_allocations (user_id, prefix, n, task_uuid, state, attempt_id) VALUES (?1, 'WORK', 2, ?2, 'pending', 'b')",
                        [&user_id, &task_uuid],
                    )?;
                    Ok::<_, BoxErr>(())
                }
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow);
        let err = result.expect_err("duplicate task_uuid must fail (partial unique index)");
        assert!(
            matches!(
                err,
                StoreError::Constraint(ConstraintKind::Unique { resource })
                    if resource == resources::TASK_KEY_ALLOCATIONS_TASK_UUID
            ),
            "got {err:?}"
        );
    }
}
