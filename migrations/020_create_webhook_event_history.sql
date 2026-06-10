CREATE TABLE IF NOT EXISTS webhook_event_history (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    task_uuid TEXT NOT NULL,
    event_type TEXT NOT NULL,
    due_at TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    PRIMARY KEY (user_id, task_uuid, event_type, due_at)
);

CREATE INDEX IF NOT EXISTS idx_webhook_event_history_task
ON webhook_event_history(user_id, task_uuid);

CREATE INDEX IF NOT EXISTS idx_webhook_event_history_created_at
ON webhook_event_history(created_at);
