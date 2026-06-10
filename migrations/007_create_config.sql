CREATE TABLE config (
    config_type TEXT NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id),
    version TEXT,
    items TEXT NOT NULL,  -- JSON blob
    PRIMARY KEY (user_id, config_type)
);
