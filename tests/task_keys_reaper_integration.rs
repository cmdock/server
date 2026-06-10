//! Reaper integration tests (#130 C3 + C8) — covers:
//!
//! - stale-pending burn for `task_uuid IS NULL` rows (Phase 1)
//! - uuid-attached burn when TC has no matching task (Phase 2)
//! - uuid-attached finalise when TC has the matching task + UDA (Phase 2)
//! - fresh-row preservation (no premature action)
//! - lock-contention skip (don't block live mutations)
//! - per-user mutation lock eviction on user delete

mod common;

use std::sync::Arc;

use cmdock_server::app_state::AppState;
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::task_keys::run_reaper_pass;
use tempfile::TempDir;
use uuid::Uuid;

async fn build_state(data_dir: std::path::PathBuf) -> (AppState, Arc<dyn ConfigStore>) {
    let db_path = data_dir.join("config.sqlite");
    std::fs::create_dir_all(data_dir.join("users")).unwrap();
    let sqlite_store = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite_store.clone();
    store.run_migrations().await.unwrap();
    let config = common::test_server_config(data_dir);
    let state = AppState::new(store.clone(), sqlite_store, &config);
    (state, store)
}

async fn create_user(store: &Arc<dyn ConfigStore>, name: &str) -> String {
    let user_id = store
        .create_user(&NewUser {
            username: name.to_string(),
            password_hash: "hash".into(),
        })
        .await
        .unwrap()
        .id;
    store.set_user_prefix(&user_id, "WORK").await.unwrap();
    store
        .ensure_personal_task_scope_for_user(&user_id)
        .await
        .unwrap();
    user_id
}

/// Sqlite-typed helper: forge `created_at` on a pending row to make the
/// reaper see it as stale. Production code never manipulates timestamps;
/// tests use a separate `tokio_rusqlite::Connection` against the same DB
/// (SQLite serialises writers, so this is safe).
async fn forge_stale_created_at(db_path: &std::path::Path, age_seconds: i64) {
    let conn = tokio_rusqlite::Connection::open(db_path).await.unwrap();
    conn.call(move |conn| {
        conn.execute(
            &format!(
                "UPDATE task_key_allocations \
                 SET created_at = datetime('now', '-{age_seconds} seconds') \
                 WHERE state = 'pending'"
            ),
            [],
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reaper_burns_stale_pending_with_null_uuid() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let (state, store) = build_state(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "alice").await;

    let (n, _attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    assert_eq!(n, 1);

    // Forge stale: 10 minutes (default pending_timeout is 5 minutes).
    forge_stale_created_at(&db_path, 600).await;

    let outcome = run_reaper_pass(&state).await;
    assert_eq!(outcome.burned, 1);
    assert_eq!(outcome.skipped_uuid_attached, 0);

    // Next reservation must skip burned N (no reuse).
    let (next_n, _) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    assert_eq!(next_n, 2);
}

#[tokio::test]
async fn reaper_burns_uuid_attached_row_when_tc_lacks_matching_task() {
    // Phase 2 contract (server#130 C8): uuid-attached pending rows whose
    // task does NOT exist in TC are burned by the reaper. As of iter3 of
    // Phase 4 (server#130 C13), uuid-attached rows whose TC task exists
    // *with a mismatched `cmdock_key` UDA* are NOT burned — they're
    // SkipUdaMismatch'd and audited for operator review (see
    // `tests/task_keys_backfill_integration.rs::reaper_skips_uuid_attached_row_with_uda_mismatch_for_operator_review`).
    // The companion finalisation path (TC has task with matching UDA →
    // reaper finalises pending → committed) is exercised by C9's
    // reaper-race regression test.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let (state, store) = build_state(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "bob").await;

    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    let task_uuid = Uuid::new_v4().to_string();
    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid)
        .await
        .unwrap();

    forge_stale_created_at(&db_path, 600).await;

    let outcome = run_reaper_pass(&state).await;
    // No TC task exists for this UUID, so the reaper burns the row.
    assert_eq!(outcome.burned, 1);
    assert_eq!(outcome.finalised, 0);

    // Row is now `burned` — no longer surfaces via lookup_task_key_by_uuid
    // (which only returns pending+committed states).
    let entry = store
        .lookup_task_key_by_uuid(&user_id, &task_uuid)
        .await
        .unwrap();
    assert!(entry.is_none());
}

/// Reaper-race regression lock: a uuid-attached pending row whose TC task
/// exists with a matching `cmdock_key` UDA must be FINALISED by the
/// reaper (`pending → committed`), NOT burned. This is the recovery path
/// for the situation where `commit_operations` succeeded but
/// `commit_task_key` failed (or was interrupted) — the row is stuck
/// pending, but the TC side is correct, so the reaper finalises.
///
/// Per `task-write-contract.md` § Task Keys; CLAUDE.md § Task key
/// allocation; server#130 C8.
#[tokio::test]
async fn reaper_finalises_uuid_attached_row_when_tc_has_matching_uda() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let (state, store) = build_state(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "erin").await;

    // Reserve a slot and synthesise the task in TC with a matching UDA.
    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    let canonical_key = format!("WORK-{n}");
    let task_uuid = Uuid::new_v4();

    // Write the TC task directly via the replica handle so we don't
    // depend on the HTTP create-path (this test simulates the scenario
    // where the create-path's commit_operations succeeded but
    // commit_task_key failed, leaving the row pending+attached).
    let rep_arc = cmdock_server::user_runtime::open_user_replica(&state, &user_id, "test")
        .await
        .unwrap();
    {
        let mut rep = rep_arc.lock().await;
        let mut ops = taskchampion::Operations::new();
        let mut task = rep.create_task(task_uuid, &mut ops).await.unwrap();
        task.set_status(taskchampion::Status::Pending, &mut ops)
            .unwrap();
        task.set_value("cmdock_key", Some(canonical_key.clone()), &mut ops)
            .unwrap();
        rep.commit_operations(ops).await.unwrap();
    }

    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid.to_string())
        .await
        .unwrap();

    forge_stale_created_at(&db_path, 600).await;

    let outcome = run_reaper_pass(&state).await;
    assert_eq!(
        outcome.finalised, 1,
        "reaper must finalise on UUID+UDA match"
    );
    assert_eq!(
        outcome.burned, 0,
        "reaper must NOT burn a row whose TC task is correct"
    );

    // Row is now committed.
    let entry = store
        .lookup_task_key_by_uuid(&user_id, &task_uuid.to_string())
        .await
        .unwrap();
    assert!(matches!(
        entry,
        Some((ref k, cmdock_server::store::models::KeyState::Committed)) if k == &canonical_key
    ));
}

#[tokio::test]
async fn reaper_does_not_touch_fresh_pending() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = build_state(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "carol").await;

    let (n, _attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();

    // No stale forge — created_at is now.
    let outcome = run_reaper_pass(&state).await;
    assert_eq!(outcome.burned, 0);
    assert_eq!(outcome.skipped_uuid_attached, 0);
    assert_eq!(outcome.skipped_lock_busy, 0);

    // Row still pending — next reservation gets N+1 not N.
    let (next_n, _) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    assert_eq!(next_n, n + 1);
}

#[tokio::test]
async fn reaper_skips_when_per_user_lock_contended() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let (state, store) = build_state(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "dave").await;

    let (_, _) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    forge_stale_created_at(&db_path, 600).await;

    // Hold the per-user mutation lock from outside the reaper.
    let lock = state.recovery_runtime.task_mutation_lock(&user_id);
    let _guard = lock.lock().await;

    let outcome = run_reaper_pass(&state).await;
    assert_eq!(outcome.burned, 0);
    assert_eq!(outcome.skipped_uuid_attached, 0);
    assert_eq!(outcome.skipped_lock_busy, 1);

    drop(_guard);
    // Subsequent pass with lock free should burn the row.
    let outcome2 = run_reaper_pass(&state).await;
    assert_eq!(outcome2.burned, 1);
}

/// Install a temporary trigger that causes any state→'committed'
/// UPDATE on `task_key_allocations` to fail with `RAISE(ABORT, ...)`.
/// Used to simulate the rare "Phase 3 retry irrecoverable" path
/// (commit_task_key fails after UUID+UDA match) per Phase 5c.
async fn install_commit_fail_trigger(db_path: &std::path::Path) {
    let conn = tokio_rusqlite::Connection::open(db_path).await.unwrap();
    conn.call(|conn| {
        conn.execute(
            "CREATE TRIGGER IF NOT EXISTS test_force_commit_fail
             BEFORE UPDATE OF state ON task_key_allocations
             WHEN NEW.state = 'committed'
             BEGIN
               SELECT RAISE(ABORT, 'forced commit fail for test');
             END",
            [],
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .unwrap();
}

/// Install a temporary trigger that causes any state→'burned' UPDATE
/// on `task_key_allocations` to fail. Used to simulate the rare
/// "burn fails after UDA clear" path (Phase 5c) where the row stays
/// pending, TC UDA is already cleared, and the next reaper pass must
/// self-heal by burning via the plain Burn path.
async fn install_burn_fail_trigger(db_path: &std::path::Path) {
    let conn = tokio_rusqlite::Connection::open(db_path).await.unwrap();
    conn.call(|conn| {
        conn.execute(
            "CREATE TRIGGER IF NOT EXISTS test_force_burn_fail
             BEFORE UPDATE OF state ON task_key_allocations
             WHEN NEW.state = 'burned'
             BEGIN
               SELECT RAISE(ABORT, 'forced burn fail for test');
             END",
            [],
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .unwrap();
}

/// Drop the burn-fail trigger so the next reaper pass can self-heal.
async fn drop_burn_fail_trigger(db_path: &std::path::Path) {
    let conn = tokio_rusqlite::Connection::open(db_path).await.unwrap();
    conn.call(|conn| {
        conn.execute("DROP TRIGGER IF EXISTS test_force_burn_fail", [])?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .unwrap();
}

/// Phase 5c regression lock: when `commit_task_key` fails on a
/// Finalise-classified row (TC has the task with matching `cmdock_key`
/// UDA), the reaper escalates to burn-with-UDA-clear: emit reverse
/// `cmdock_key` UDA op, commit under the held replica lock, then
/// `burn_task_key`. End state: row is burned, TC's `cmdock_key` UDA is
/// gone, no half-applied "row burned, UDA still set" state can be
/// observed.
#[tokio::test]
async fn reaper_phase3_retry_failure_burns_with_uda_clear() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let (state, store) = build_state(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "fran").await;

    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    let canonical_key = format!("WORK-{n}");
    let task_uuid = Uuid::new_v4();

    // Synthesise the TC task with a matching cmdock_key UDA (reaper
    // would otherwise classify as Burn or SkipUdaMismatch).
    let rep_arc = cmdock_server::user_runtime::open_user_replica(&state, &user_id, "test")
        .await
        .unwrap();
    {
        let mut rep = rep_arc.lock().await;
        let mut ops = taskchampion::Operations::new();
        let mut task = rep.create_task(task_uuid, &mut ops).await.unwrap();
        task.set_status(taskchampion::Status::Pending, &mut ops)
            .unwrap();
        task.set_value("cmdock_key", Some(canonical_key.clone()), &mut ops)
            .unwrap();
        rep.commit_operations(ops).await.unwrap();
    }

    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid.to_string())
        .await
        .unwrap();

    forge_stale_created_at(&db_path, 600).await;

    // Install the trigger AFTER setup so reservation/attach succeeds;
    // the trigger only fires on state→'committed' transitions.
    install_commit_fail_trigger(&db_path).await;

    let outcome = run_reaper_pass(&state).await;

    // Phase 3 retry attempt failed (commit_task_key blocked by trigger).
    assert_eq!(outcome.phase3_retry_failed, 1);
    // Reverse-UDA op was emitted and committed.
    assert_eq!(outcome.uda_cleared, 1);
    // Row transitioned to burned (state→'burned' is not blocked by the
    // trigger, only state→'committed' is).
    assert_eq!(outcome.burned, 1);
    assert_eq!(outcome.finalised, 0);

    // Final state: TC task no longer carries the cmdock_key UDA.
    let rep_arc2 = cmdock_server::user_runtime::open_user_replica(&state, &user_id, "test")
        .await
        .unwrap();
    let mut rep = rep_arc2.lock().await;
    let task = rep.get_task(task_uuid).await.unwrap().unwrap();
    assert!(
        task.get_value("cmdock_key").is_none(),
        "reverse-UDA op did not clear cmdock_key on canonical replica"
    );
    drop(rep);

    // Allocation row is burned (lookup excludes burned).
    let entry = store
        .lookup_task_key_by_uuid(&user_id, &task_uuid.to_string())
        .await
        .unwrap();
    assert!(
        entry.is_none(),
        "allocation row should be burned (lookup_task_key_by_uuid returns None)",
    );
}

/// Regression lock for server#141: burn-with-UDA-clear removes only the
/// allocation key. `cmdock_account` remains on the TC task so account
/// ownership is preserved during the no-key window.
#[tokio::test]
async fn reaper_burn_preserves_cmdock_account() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let (state, store) = build_state(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "account-burn").await;

    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    let canonical_key = format!("WORK-{n}");
    let task_uuid = Uuid::new_v4();

    let rep_arc = cmdock_server::user_runtime::open_user_replica(&state, &user_id, "test")
        .await
        .unwrap();
    {
        let mut rep = rep_arc.lock().await;
        let mut ops = taskchampion::Operations::new();
        let mut task = rep.create_task(task_uuid, &mut ops).await.unwrap();
        task.set_status(taskchampion::Status::Pending, &mut ops)
            .unwrap();
        task.set_value("cmdock_key", Some(canonical_key.clone()), &mut ops)
            .unwrap();
        task.set_value("cmdock_account", Some("WORK".to_string()), &mut ops)
            .unwrap();
        rep.commit_operations(ops).await.unwrap();
    }

    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid.to_string())
        .await
        .unwrap();
    forge_stale_created_at(&db_path, 600).await;
    install_commit_fail_trigger(&db_path).await;

    let outcome = run_reaper_pass(&state).await;
    assert_eq!(outcome.phase3_retry_failed, 1);
    assert_eq!(outcome.uda_cleared, 1);
    assert_eq!(outcome.burned, 1);

    let rep_arc2 = cmdock_server::user_runtime::open_user_replica(&state, &user_id, "test")
        .await
        .unwrap();
    let mut rep = rep_arc2.lock().await;
    let task = rep.get_task(task_uuid).await.unwrap().unwrap();
    assert!(
        task.get_value("cmdock_key").is_none(),
        "reaper burn must clear cmdock_key in the same observation"
    );
    assert_eq!(
        task.get_value("cmdock_account").map(|v| v.to_string()),
        Some("WORK".to_string()),
        "reaper burn must preserve cmdock_account; a cmdock_account=NULL op must not be emitted"
    );
    drop(rep);

    let entry = store
        .lookup_task_key_by_uuid(&user_id, &task_uuid.to_string())
        .await
        .unwrap();
    assert!(
        entry.is_none(),
        "allocation row should be burned while cmdock_account remains on the TC task",
    );
}

/// Burned-no-UDA regression lock: when the TC task is missing entirely,
/// the reaper burns the row WITHOUT emitting a reverse-UDA op (no UDA
/// to clear). `outcome.uda_cleared` stays at 0. Pinned independently so
/// future changes to the burn path can't accidentally start emitting
/// no-op reverse-UDA commits on every burn.
#[tokio::test]
async fn reaper_burned_no_uda_does_not_emit_reverse_op() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let (state, store) = build_state(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "gail").await;

    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    let task_uuid = Uuid::new_v4().to_string();
    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid)
        .await
        .unwrap();
    forge_stale_created_at(&db_path, 600).await;

    let outcome = run_reaper_pass(&state).await;
    assert_eq!(outcome.burned, 1);
    assert_eq!(
        outcome.uda_cleared, 0,
        "no UDA to clear; no reverse op emitted"
    );
    assert_eq!(outcome.phase3_retry_failed, 0);
}

/// Lock-discipline regression lock for Phase 5c: while the reaper runs
/// burn-with-UDA-clear, no observer that takes the per-user replica
/// lock can see "DB row burned but TC's cmdock_key UDA still set".
///
/// The test spawns a hammer-loop reader that, under the replica lock,
/// reads BOTH the TC UDA value AND the allocation-row state (via
/// `lookup_task_key_by_uuid`). Each pair of observations is taken
/// inside one lock acquisition; the reaper's same-lock burn-with-UDA-
/// clear means each observation is consistent (UDA-set + row-pending
/// OR UDA-cleared + row-burned, never UDA-set + row-burned).
#[tokio::test]
async fn reaper_burn_with_uda_clear_lock_discipline_consistency() {
    use cmdock_server::store::models::KeyState;

    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let (state, store) = build_state(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "hank").await;

    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    let canonical_key = format!("WORK-{n}");
    let task_uuid = Uuid::new_v4();

    let rep_arc = cmdock_server::user_runtime::open_user_replica(&state, &user_id, "test")
        .await
        .unwrap();
    {
        let mut rep = rep_arc.lock().await;
        let mut ops = taskchampion::Operations::new();
        let mut task = rep.create_task(task_uuid, &mut ops).await.unwrap();
        task.set_status(taskchampion::Status::Pending, &mut ops)
            .unwrap();
        task.set_value("cmdock_key", Some(canonical_key.clone()), &mut ops)
            .unwrap();
        rep.commit_operations(ops).await.unwrap();
    }

    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid.to_string())
        .await
        .unwrap();
    forge_stale_created_at(&db_path, 600).await;
    install_commit_fail_trigger(&db_path).await;

    let task_uuid_str = task_uuid.to_string();
    let reader_state = state.clone();
    let reader_user_id = user_id.clone();
    let reader_task_uuid_str = task_uuid_str.clone();
    let reader_canonical = canonical_key.clone();
    let reader = tokio::spawn(async move {
        // Hammer the canonical state under the replica lock. Take both
        // the TC UDA AND the DB row state inside one lock acquisition
        // so reaper writes can't interleave.
        let mut bad: Vec<&'static str> = Vec::new();
        for _ in 0..40 {
            let rep_arc = cmdock_server::user_runtime::open_user_replica(
                &reader_state,
                &reader_user_id,
                "test",
            )
            .await
            .unwrap();
            let mut rep = rep_arc.lock().await;
            let task = rep.get_task(task_uuid).await.unwrap();
            let tc_uda = task.and_then(|t| t.get_value("cmdock_key").map(|v| v.to_string()));
            // DB read while still holding replica lock.
            let row = reader_state
                .store
                .lookup_task_key_by_uuid(&reader_user_id, &reader_task_uuid_str)
                .await
                .unwrap();
            drop(rep);

            match (tc_uda.as_deref(), row.as_ref()) {
                (Some(v), Some((row_key, KeyState::Pending))) => {
                    if v != reader_canonical || row_key != &reader_canonical {
                        bad.push("pending-state-mismatch");
                    }
                }
                (Some(_), None) => {
                    bad.push("uda-set-but-row-burned-or-missing");
                }
                (None, None) => {
                    // Post-burn-with-UDA-clear consistent state.
                }
                (None, Some((_, KeyState::Pending))) => {
                    bad.push("uda-cleared-but-row-still-pending");
                }
                _ => {}
            }
            tokio::time::sleep(std::time::Duration::from_micros(200)).await;
        }
        bad
    });

    let outcome = run_reaper_pass(&state).await;
    let bad_observations = reader.await.unwrap();

    assert_eq!(outcome.burned, 1);
    assert_eq!(outcome.uda_cleared, 1);
    assert_eq!(outcome.phase3_retry_failed, 1);
    assert!(
        bad_observations.is_empty(),
        "reader observed inconsistent state during reaper burn-with-UDA-clear: {bad_observations:?}",
    );

    // Final assertion: post-reaper, both sides agree (no UDA, no row).
    let rep_arc2 = cmdock_server::user_runtime::open_user_replica(&state, &user_id, "test")
        .await
        .unwrap();
    let mut rep = rep_arc2.lock().await;
    let task = rep.get_task(task_uuid).await.unwrap().unwrap();
    assert!(task.get_value("cmdock_key").is_none());
    drop(rep);
    let entry = store
        .lookup_task_key_by_uuid(&user_id, &task_uuid_str)
        .await
        .unwrap();
    assert!(entry.is_none());
}

/// Phase 5c regression lock: when the canonical-replica lock is held
/// by an external party (e.g. an in-flight bridge sync) the reaper
/// must NOT block indefinitely while holding the mutation lock —
/// instead it skips the user this pass with `replica_busy` reason.
/// Without the timeout at the replica-lock acquire, a long-running
/// `do_sync` would stall every live mutation for that user behind the
/// mutation lock the reaper still holds.
#[tokio::test]
async fn reaper_skips_when_replica_lock_contended() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let (state, store) = build_state(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "iris").await;

    // Reserve a uuid-attached pending row so the reaper takes the
    // replica-lock branch (null-uuid candidates skip the replica
    // lock entirely).
    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    let task_uuid = Uuid::new_v4().to_string();
    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid)
        .await
        .unwrap();
    forge_stale_created_at(&db_path, 600).await;

    // Hold the replica lock from outside the reaper.
    let rep_arc = cmdock_server::user_runtime::open_user_replica(&state, &user_id, "test")
        .await
        .unwrap();
    let _rep_guard = rep_arc.lock().await;

    let outcome = run_reaper_pass(&state).await;
    assert_eq!(outcome.burned, 0);
    assert_eq!(outcome.finalised, 0);
    assert_eq!(
        outcome.skipped_lock_busy, 1,
        "reaper must skip the user when replica lock is contended (records on the same metric as mutation-lock contention)",
    );

    // Release the lock; subsequent pass observes the row and burns it
    // via the normal "TC missing the task" path (we never created the
    // task on TC in this fixture — so the classify step yields Burn).
    drop(_rep_guard);
    let outcome2 = run_reaper_pass(&state).await;
    assert_eq!(outcome2.burned, 1);
}

/// Phase 5c regression lock: when `burn_task_key` fails AFTER the
/// reverse `cmdock_key` UDA op has committed under the held replica
/// lock, the row stays pending with the canonical UDA cleared. The
/// half-applied state self-heals on the next reaper pass — the TC
/// task no longer carries `cmdock_key`, so classification yields Burn
/// and `burn_plain` transitions the row to burned. This is the
/// inverse of the contract-forbidden `row=burned, UDA still set`
/// state and is documented as a known transient with operator
/// observability via `task.key.reaper_burn_after_uda_clear_failed`.
#[tokio::test]
async fn reaper_burn_after_uda_clear_failure_self_heals_on_next_pass() {
    use cmdock_server::store::models::KeyState;

    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let (state, store) = build_state(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "jack").await;

    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    let canonical_key = format!("WORK-{n}");
    let task_uuid = Uuid::new_v4();

    let rep_arc = cmdock_server::user_runtime::open_user_replica(&state, &user_id, "test")
        .await
        .unwrap();
    {
        let mut rep = rep_arc.lock().await;
        let mut ops = taskchampion::Operations::new();
        let mut task = rep.create_task(task_uuid, &mut ops).await.unwrap();
        task.set_status(taskchampion::Status::Pending, &mut ops)
            .unwrap();
        task.set_value("cmdock_key", Some(canonical_key.clone()), &mut ops)
            .unwrap();
        rep.commit_operations(ops).await.unwrap();
    }

    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid.to_string())
        .await
        .unwrap();
    forge_stale_created_at(&db_path, 600).await;

    // Block BOTH state→'committed' (forces escalation to burn-with-
    // UDA-clear) AND state→'burned' (forces the burn to fail after
    // the UDA was already cleared under the held replica lock).
    install_commit_fail_trigger(&db_path).await;
    install_burn_fail_trigger(&db_path).await;

    let outcome = run_reaper_pass(&state).await;
    // Phase 3 retry counter incremented (commit blocked by trigger).
    assert_eq!(outcome.phase3_retry_failed, 1);
    // Reverse-UDA op committed successfully under the replica lock.
    assert_eq!(outcome.uda_cleared, 1);
    // Burn was blocked → row stays pending; outcome.burned NOT
    // incremented for this row.
    assert_eq!(outcome.burned, 0);

    // Canonical TC state: UDA was cleared.
    let task_uuid_str = task_uuid.to_string();
    let rep_arc2 = cmdock_server::user_runtime::open_user_replica(&state, &user_id, "test")
        .await
        .unwrap();
    let mut rep = rep_arc2.lock().await;
    let task = rep.get_task(task_uuid).await.unwrap().unwrap();
    assert!(
        task.get_value("cmdock_key").is_none(),
        "reverse-UDA op should have cleared cmdock_key under the same lock as the (failed) burn",
    );
    drop(rep);

    // Allocation row state: still pending (burn was blocked).
    let entry = store
        .lookup_task_key_by_uuid(&user_id, &task_uuid_str)
        .await
        .unwrap();
    assert!(
        matches!(entry, Some((_, KeyState::Pending))),
        "row stays pending when burn fails after UDA clear; lookup_task_key_by_uuid should reflect this",
    );

    // Self-heal: drop the burn trigger and run another pass. The TC
    // task no longer carries `cmdock_key`, so classify→Burn→burn_plain
    // transitions the row to burned (commit-fail trigger remains, but
    // burn_plain doesn't go through commit_task_key).
    drop_burn_fail_trigger(&db_path).await;
    let outcome2 = run_reaper_pass(&state).await;
    assert_eq!(
        outcome2.burned, 1,
        "next pass must self-heal: TC task missing UDA → classify Burn → row burns",
    );

    // Final state: row burned (lookup excludes burned), UDA absent.
    let entry2 = store
        .lookup_task_key_by_uuid(&user_id, &task_uuid_str)
        .await
        .unwrap();
    assert!(entry2.is_none());
}

#[tokio::test]
async fn task_mutation_lock_evicted_on_evict_user() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = build_state(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "ed").await;

    let lock_a = state.recovery_runtime.task_mutation_lock(&user_id);
    state.recovery_runtime.evict_user(&user_id);
    let lock_b = state.recovery_runtime.task_mutation_lock(&user_id);

    // After eviction the map entry is fresh; pre-eviction handle and
    // post-eviction handle point to different mutexes (Arc identity).
    assert!(!Arc::ptr_eq(&lock_a, &lock_b));
}
