-- One-shot upgrade repair for burned `task_key_allocations` rows that
-- still carry a non-NULL `task_uuid`. The `burn_task_key` primitive
-- pre-iter2 left `task_uuid` set on transition to `burned`, which kept
-- the partial unique index `idx_task_key_allocations_uuid` (UNIQUE on
-- task_uuid WHERE task_uuid IS NOT NULL) holding the slot — blocking
-- a future Phase 4 backfill, recreate, or re-allocation for the same
-- task UUID even after the original reservation was rolled back.
--
-- Iter2 of #130 fixed the burn path to detach `task_uuid` going
-- forward; this migration brings already-burned rows up to the same
-- shape so deployments that ran the reaper or `commit_backfill_*`
-- before the fix shipped don't carry a permanent index hazard.
--
-- Burned rows are append-only — `n` stays burned forever (MAX(n) over
-- all states preserves rollback gaps), so detaching `task_uuid` here
-- does not relax any other invariant.
UPDATE task_key_allocations
   SET task_uuid = NULL
 WHERE state = 'burned'
   AND task_uuid IS NOT NULL;
