CREATE TABLE views (
    id TEXT NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id),
    label TEXT NOT NULL,
    icon TEXT NOT NULL,
    filter TEXT NOT NULL,
    group_by TEXT,
    context_filtered INTEGER DEFAULT 0,
    sort_order INTEGER DEFAULT 0,
    PRIMARY KEY (user_id, id)
);
