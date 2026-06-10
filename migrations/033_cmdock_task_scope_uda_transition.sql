-- Forward-only Task Scope UDA transition marker.
--
-- `cmdock_task_scope` is TaskChampion task metadata, not config-DB state, so
-- there is no SQLite column to add or backfill here. The durable namespace was
-- already materialised in `task_scopes` and `task_key_allocations.task_scope_id`
-- by migrations 031/032. This marker documents the forward-only rollout: code
-- paths that create, backfill, reconcile, drift-repair, or gateway-project a
-- task now stamp `cmdock_task_scope` as the canonical Task Scope UDA.
-- `cmdock_account` writes are suppressed server-side (TSKEY-007/SG-011);
-- existing `cmdock_account` TC values are left in place but filtered from the
-- REST TaskItem projection.
--
-- Rollback: forward-only. Once sync clients observe `cmdock_task_scope`, a DB
-- down-migration cannot remove already-synchronised TC UDA history. A rollback
-- must deploy compatibility code that ignores `cmdock_task_scope` while leaving
-- existing TC properties harmlessly in place.

SELECT 1;
