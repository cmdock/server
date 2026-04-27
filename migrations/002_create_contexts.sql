CREATE TABLE contexts (
    id TEXT NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id),
    label TEXT NOT NULL,
    project_prefixes TEXT NOT NULL,  -- JSON array: ["PERSONAL"]
    color TEXT,
    icon TEXT,
    sort_order INTEGER DEFAULT 0,
    PRIMARY KEY (user_id, id)
);
