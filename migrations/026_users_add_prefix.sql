-- Per-user task-key prefix per `task-write-contract.md` § Task Keys
-- (cmdock/architecture, commit a69c647 — `cmdock/architecture#34`).
--
-- v1 maps account = user. The prefix is the leading component of every
-- `<PREFIX>-N` key allocated under (user_id, prefix) in
-- task_key_allocations. Format is `^[A-Z][A-Z0-9]{0,9}$` — first character
-- a letter, rest letters or digits, total length ≤ 10.
--
-- Server-wide UNIQUE: keys collide trivially across users if two users
-- share `WORK`, so prefixes are globally unique. Operators can override
-- a derived collision via `cmdock-server admin user set-prefix`.
--
-- Nullable for existing rows. Backfill happens via a Rust startup routine
-- (post-migration hook in `src/main.rs`) calling
-- `admin::prefix::backfill_missing_user_prefixes`. SQL can't run the
-- collision-resolving derive algorithm; that's why this is two phases.
--
-- Immutability rule: once any allocation row exists for a user (any state)
-- OR `users.task_keys_migrated_at IS NOT NULL`, `set-prefix` rejects with
-- `PREFIX_LOCKED`. See `task-write-contract.md` § Set-prefix immutability.
ALTER TABLE users ADD COLUMN prefix TEXT;

-- Server-wide uniqueness; SQLite UNIQUE on a nullable column allows
-- multiple NULLs (existing pre-backfill rows) and rejects duplicate
-- non-NULL values, which is exactly what we want during the backfill
-- transition window.
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_prefix
    ON users(prefix) WHERE prefix IS NOT NULL;
