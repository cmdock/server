-- Maps TaskChampion sync client IDs to users.
-- The `task` CLI sends X-Client-Id on every sync request.
-- This table maps that to a user_id for data isolation.
CREATE TABLE IF NOT EXISTS sync_clients (
    client_id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    label TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_sync_clients_user ON sync_clients(user_id);
