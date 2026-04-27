CREATE TABLE geofences (
    id TEXT NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id),
    label TEXT NOT NULL,
    latitude REAL NOT NULL,
    longitude REAL NOT NULL,
    radius REAL NOT NULL DEFAULT 200,
    type TEXT NOT NULL DEFAULT 'home',
    context_id TEXT,
    view_id TEXT,
    store_tag TEXT,
    PRIMARY KEY (user_id, id)
);
