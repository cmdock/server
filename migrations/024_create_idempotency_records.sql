-- Idempotency-Key dedup records per `task-write-contract.md` § Idempotency
-- (cmdock/architecture commit a3f242a).
--
-- Implements the three-phase write-ahead pattern. Phase 1 inserts a row
-- with state='pending' + body fingerprint + attempt_id. Phase 3 updates
-- the row to state='completed' with the response payload, guarded by the
-- attempt_id to defeat stale-finalizer races (a delayed Phase 3 from an
-- expired attempt finds no matching row and is silently discarded).
--
-- The tuple (user_id, request_path, idempotency_key) is the unique key.
-- Two retries with the same tuple either find an existing row and replay,
-- or race on insert and the loser falls through to lookup (UNIQUE
-- constraint serialises them).
--
-- Lives in config.sqlite alongside the other observability tables
-- (webhook_event_history, audit log targets) — NOT in the per-user
-- TaskChampion replica, which is owned by the TC library and does not
-- accept arbitrary table writes. The contract acknowledges this in
-- § Storage reality: true single-transaction atomicity across the TC
-- mutation and the dedup record is structurally infeasible without
-- forking TaskChampion. The two-phase write-ahead pattern bounds the
-- residual window via lookup-time expiry on `pending` rows.
CREATE TABLE IF NOT EXISTS idempotency_records (
    user_id          TEXT    NOT NULL,
    request_path     TEXT    NOT NULL,
    idempotency_key  TEXT    NOT NULL,
    -- Server-generated UUID per attempt. Phase 3's UPDATE is conditioned
    -- on this so a stale Phase 3 from an expired attempt cannot overwrite
    -- a fresh retry's pending row. Spec § Server behaviour Phase 3.
    attempt_id       TEXT    NOT NULL,
    -- 'pending' (Phase 1) or 'completed' (Phase 3). CHECK enforced.
    state            TEXT    NOT NULL CHECK (state IN ('pending', 'completed')),
    -- SHA-256 of the HTTP request body bytes after content-encoding
    -- decoding (handler-visible bytes). 32 bytes; constant-size.
    body_fingerprint BLOB    NOT NULL,
    -- Response payload — NULL while pending, populated by Phase 3.
    -- Replayed verbatim on `completed` lookups; per-attempt headers
    -- (X-Request-ID, Date, tracing) are regenerated separately.
    status_code      INTEGER,
    response_body    BLOB,
    content_type     TEXT,
    content_length   INTEGER,
    -- Unix epoch seconds. Used by lookup-time expiry on `pending` rows
    -- (deterministic residual-window bound, independent of the reaper)
    -- and by retention pruning for both states.
    created_at       INTEGER NOT NULL,
    PRIMARY KEY (user_id, request_path, idempotency_key)
) WITHOUT ROWID;

-- Retention pruner index — both `completed` (24h default) and stranded
-- `pending` (5min default) prune by created_at.
CREATE INDEX IF NOT EXISTS idx_idempotency_records_created_at
    ON idempotency_records(created_at);

-- Stranded-pending reaper index — fast scan of `pending` rows by age.
-- Lookup-time expiry can use the primary key, but the reaper sweeps by
-- (state, created_at) to avoid full-table scans.
CREATE INDEX IF NOT EXISTS idx_idempotency_records_state_created_at
    ON idempotency_records(state, created_at);
