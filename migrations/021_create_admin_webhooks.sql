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
