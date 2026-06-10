-- Explicit Task Scope identity for task ownership/key namespace.
-- S1 creates the Personal Task Scope read model only. Allocation rows gain
-- task_scope_id in the follow-up dual-write migration so this slice remains
-- additive and rollback-safe.

CREATE TABLE IF NOT EXISTS task_scopes (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('personal')),
    owner_runtime_user_id TEXT,
    owner_team_id TEXT,
    key_prefix TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','disabled','deleted')),
    storage_path TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    -- TODO(Task Scope teams): relax both the kind CHECK and this ownership
    -- CHECK together when team scopes are enabled.
    CHECK (kind = 'personal' AND owner_runtime_user_id IS NOT NULL AND owner_team_id IS NULL),
    FOREIGN KEY(owner_runtime_user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_task_scopes_personal_owner
    ON task_scopes(owner_runtime_user_id)
    WHERE kind = 'personal' AND owner_runtime_user_id IS NOT NULL AND status != 'deleted';

CREATE UNIQUE INDEX IF NOT EXISTS idx_task_scopes_key_prefix_active
    ON task_scopes(key_prefix)
    WHERE status != 'deleted';

INSERT OR IGNORE INTO task_scopes (id, kind, owner_runtime_user_id, key_prefix, status)
SELECT 'ts_' || lower(hex(randomblob(16))), 'personal', users.id, users.prefix, 'active'
FROM users
WHERE users.prefix IS NOT NULL
  AND NOT EXISTS (
      SELECT 1 FROM task_scopes
      WHERE kind = 'personal'
        AND owner_runtime_user_id = users.id
        AND status != 'deleted'
  );
