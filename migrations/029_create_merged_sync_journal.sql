CREATE TABLE IF NOT EXISTS merged_sync_journal (
    journal_id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    parent_version_id TEXT NOT NULL,
    merged_version_id TEXT,
    state TEXT NOT NULL CHECK (state IN (
        'received',
        'merged_version_accepted',
        'source_plan_applied',
        'projection_appended',
        'finalized',
        'failed',
        'quarantined'
    )),
    recovery_status TEXT NOT NULL CHECK (recovery_status IN (
        'not_required',
        'recoverable',
        'recovered',
        'failed',
        'quarantined'
    )),
    diagnostic_code TEXT,
    diagnostic_message TEXT,
    state_version INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    finalized_at TEXT,
    CHECK ((state IN ('finalized', 'failed', 'quarantined')) OR finalized_at IS NULL)
);

CREATE INDEX IF NOT EXISTS idx_merged_sync_journal_user_state
    ON merged_sync_journal(user_id, state, updated_at);

CREATE INDEX IF NOT EXISTS idx_merged_sync_journal_recovery_status
    ON merged_sync_journal(recovery_status, updated_at);

CREATE UNIQUE INDEX IF NOT EXISTS idx_merged_sync_journal_attempt
    ON merged_sync_journal(journal_id, attempt_id);
