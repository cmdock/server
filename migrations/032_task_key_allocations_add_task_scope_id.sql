-- Dual-write Task Scope identity onto task-key allocations.
--
-- S2 of the Task Scope transition keeps the legacy `(user_id, prefix, n)`
-- primary key for compatibility, but stamps every existing and new allocation
-- with the stable `task_scopes.id` for the user's active Personal Task Scope.
-- Burned rows are backfilled too: they are part of the no-reuse counter
-- history and must move with the namespace.

ALTER TABLE task_key_allocations
    ADD COLUMN task_scope_id TEXT REFERENCES task_scopes(id);

UPDATE task_key_allocations
SET task_scope_id = (
    SELECT task_scopes.id
    FROM task_scopes
    WHERE task_scopes.kind = 'personal'
      AND task_scopes.status = 'active'
      AND task_scopes.owner_runtime_user_id = task_key_allocations.user_id
      AND task_scopes.key_prefix = task_key_allocations.prefix
)
WHERE task_scope_id IS NULL;

-- Future lookup/counter paths use Task Scope as the namespace. SQLite permits
-- multiple NULLs in UNIQUE indexes, which keeps this rollback-safe while legacy
-- rows are being repaired by startup backfill.
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_key_allocations_task_scope_n
    ON task_key_allocations(task_scope_id, n)
    WHERE task_scope_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_task_key_allocations_task_scope_state
    ON task_key_allocations(task_scope_id, state);
