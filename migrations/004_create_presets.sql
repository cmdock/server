CREATE TABLE presets (
    id TEXT NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id),
    label TEXT NOT NULL,
    raw_suffix TEXT NOT NULL,
    sort_order INTEGER DEFAULT 0,
    PRIMARY KEY (user_id, id)
);
