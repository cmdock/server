CREATE TABLE stores (
    id TEXT NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id),
    label TEXT NOT NULL,
    tag TEXT NOT NULL,
    sort_order INTEGER DEFAULT 0,
    PRIMARY KEY (user_id, id)
);

CREATE TABLE shopping_config (
    user_id TEXT PRIMARY KEY REFERENCES users(id),
    project TEXT NOT NULL,
    default_tags TEXT NOT NULL  -- JSON array: ["shopping"]
);
