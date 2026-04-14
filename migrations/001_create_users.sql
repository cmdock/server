CREATE TABLE users (
    id TEXT PRIMARY KEY,
    username TEXT UNIQUE NOT NULL,
    password_hash TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE api_tokens (
    token_hash TEXT PRIMARY KEY,      -- SHA-256 hash of the bearer token
    user_id TEXT NOT NULL REFERENCES users(id),
    label TEXT,
    expires_at TEXT,                    -- NULL = no expiry
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
