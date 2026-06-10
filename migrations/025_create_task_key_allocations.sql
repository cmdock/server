-- Task key allocations per `task-write-contract.md` § Task Keys
-- (cmdock/architecture, contract commits 1a7af9e + a69c647 — `cmdock/architecture#34`).
--
-- Each row tracks one allocation of a `<PREFIX>-N` key for a (user, prefix)
-- counter. v1 maps account = user (per the contract's v1 scoping note);
-- (user_id, prefix) is the allocation counter scope until the account
-- abstraction lands.
--
-- Three-state row model (`pending|committed|burned`). Burned rows MUST
-- persist forever — the next allocation reads `MAX(n)` over ALL states so
-- pending rollback / reaper-burn cannot reuse N. Pending rows that expire
-- transition to `burned`, never deleted. Committed rows stay forever as
-- the canonical key→UUID lookup index.
--
-- The `attempt_id` mirrors the idempotency-record pattern (server#123-style
-- stale-finalizer guard). Phase 2 reserves under one attempt_id; if a
-- delayed finaliser arrives after the row is replaced by a fresh retry,
-- the attempt_id mismatch silently drops it.
--
-- Two-step finalisation: `attach_task_uuid_to_pending` sets `task_uuid`
-- (state stays pending) BEFORE TC commit; `commit_task_key` transitions
-- state only AFTER TC commit. This makes the row UUID-recoverable even if
-- the state-transition fails post-TC-commit (Phase 5 drift recovery and
-- the reaper both look up pending rows by `task_uuid`).
CREATE TABLE IF NOT EXISTS task_key_allocations (
    user_id      TEXT    NOT NULL,
    prefix       TEXT    NOT NULL,
    n            INTEGER NOT NULL,
    -- NULL between `reserve_task_key_pending` and `attach_task_uuid_to_pending`.
    -- After attach, NOT NULL for both `pending` (post-attach, pre-commit) and
    -- `committed`. May be NULL on `burned` rows whose pending phase never
    -- attached a UUID (caller rolled back before TC commit).
    task_uuid    TEXT,
    state        TEXT    NOT NULL CHECK (state IN ('pending', 'committed', 'burned')),
    attempt_id   TEXT    NOT NULL,
    created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
    committed_at TEXT,
    PRIMARY KEY (user_id, prefix, n)
);

-- Reaper sweeps `state = 'pending'` rows by age; the partial-by-state form
-- keeps the index small (committed + burned dominate at steady state).
CREATE INDEX IF NOT EXISTS idx_task_key_allocations_state
    ON task_key_allocations(user_id, prefix, state);

-- Phase 2 / Phase 5 lookup-by-UUID. Partial index on NOT NULL keeps it
-- small and serves as a defence-in-depth guard against double-attach (the
-- Rust path also asserts rows-affected == 1 in `attach_task_uuid_to_pending`).
-- task_uuid is globally unique across users — TC mints UUIDs and the same
-- UUID can't legitimately appear under two users.
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_key_allocations_uuid
    ON task_key_allocations(task_uuid) WHERE task_uuid IS NOT NULL;
