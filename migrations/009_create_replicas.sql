-- Per-user replica + sync identity (ADR-0001).
-- Replaces sync_clients table with encrypted key escrow.
CREATE TABLE IF NOT EXISTS replicas (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    encryption_secret_enc TEXT NOT NULL,
    label TEXT NOT NULL DEFAULT 'Personal',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(user_id)
);

-- Migrate: sync_clients had no encryption_secret, so data cannot be migrated.
-- Existing sync.sqlite files remain valid (Replica is source of truth).
DROP TABLE IF EXISTS sync_clients;
