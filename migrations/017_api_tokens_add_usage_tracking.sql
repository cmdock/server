CREATE TABLE IF NOT EXISTS api_tokens (
    token_hash TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    label TEXT,
    expires_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

ALTER TABLE api_tokens ADD COLUMN first_used_at TEXT;
ALTER TABLE api_tokens ADD COLUMN last_used_at TEXT;
ALTER TABLE api_tokens ADD COLUMN last_used_ip TEXT;
