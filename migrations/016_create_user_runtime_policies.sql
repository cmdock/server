CREATE TABLE IF NOT EXISTS user_runtime_policies (
    user_id TEXT PRIMARY KEY REFERENCES users(id),
    desired_version TEXT NOT NULL,
    desired_policy_json TEXT NOT NULL,
    applied_version TEXT,
    applied_policy_json TEXT,
    applied_at TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
