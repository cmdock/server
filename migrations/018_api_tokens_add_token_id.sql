CREATE TABLE IF NOT EXISTS api_tokens (
    token_hash TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    label TEXT,
    expires_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    first_used_at TEXT,
    last_used_at TEXT,
    last_used_ip TEXT
);

ALTER TABLE api_tokens ADD COLUMN token_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_api_tokens_token_id
ON api_tokens(token_id)
WHERE token_id IS NOT NULL;
