-- Device registry: maps physical devices to users.
-- Each device has its own client_id (used in TC sync X-Client-Id header).
-- A user can have many devices; revoking one does not affect the others.

CREATE TABLE IF NOT EXISTS devices (
    client_id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    registered_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_sync_at TEXT,
    last_sync_ip TEXT,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'revoked'))
);

CREATE INDEX IF NOT EXISTS idx_devices_user_id ON devices(user_id);

-- Backfill: auto-register any existing replicas as devices.
-- This avoids breaking existing beta users who already have a client_id.
INSERT OR IGNORE INTO devices (client_id, user_id, name, registered_at, status)
SELECT id, user_id, 'Migrated device', datetime('now'), 'active'
FROM replicas;
