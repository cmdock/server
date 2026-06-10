//! Backfill + delete-cascade integration tests (#130 C4 + C13).
//!
//! Covers:
//! - C4 startup-routine prefix backfill (`backfill_missing_user_prefixes`)
//! - C4 `delete_user` cascade through `task_key_allocations`
//! - C13 Phase 4 per-account-lazy task-keys backfill
//!   (`task_keys::backfill::ensure_user_task_keys_migrated`):
//!     - fresh-user no-op fast-path
//!     - pre-feature tasks → all get keys + UDA, ordering deterministic
//!     - cache hit second run, no DB writes
//!     - concurrent first-access exactly-once
//!     - isolation: user A's first access does not migrate user B
//!     - recovery: pre-existing UDA matches → recovery audit fires

mod common;

use std::sync::Arc;

use cmdock_server::admin::prefix::backfill_missing_user_prefixes;
use cmdock_server::app_state::AppState;
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::task_keys::backfill::ensure_user_task_keys_migrated;
use taskchampion::{Operations, Replica, SqliteStorage, Status};
use tempfile::TempDir;
use uuid::Uuid;

async fn open_store_at(data_dir: &std::path::Path) -> Arc<dyn ConfigStore> {
    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite;
    store.run_migrations().await.unwrap();
    store
}

#[tokio::test]
async fn backfill_assigns_unique_prefixes_to_unmigrated_users() {
    let tmp = TempDir::new().unwrap();
    let store = open_store_at(tmp.path()).await;

    // Two users with the same prefix candidate ("ALICE") to force a
    // collision on the second derive.
    for name in ["alice", "alice42"] {
        store
            .create_user(&NewUser {
                username: name.to_string(),
                password_hash: "hash".into(),
            })
            .await
            .unwrap();
    }

    let count = backfill_missing_user_prefixes(store.as_ref())
        .await
        .unwrap();
    assert_eq!(count, 2);

    // Second pass: no work to do.
    let count2 = backfill_missing_user_prefixes(store.as_ref())
        .await
        .unwrap();
    assert_eq!(count2, 0);

    let users = store.list_users().await.unwrap();
    let mut prefixes: Vec<String> = Vec::new();
    for u in users {
        let p = store.get_user_prefix(&u.id).await.unwrap();
        prefixes.push(p.expect("prefix must be assigned"));
    }
    // Both prefixes present, both unique.
    assert_eq!(prefixes.len(), 2);
    assert_ne!(prefixes[0], prefixes[1]);
    // Both follow the canonical format (we don't pin exact values
    // because users.created_at ordering may differ by sub-second).
    for p in &prefixes {
        assert!(!p.is_empty() && p.len() <= 10);
        let mut chars = p.chars();
        assert!(chars.next().unwrap().is_ascii_uppercase());
        assert!(chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }
}

#[tokio::test]
async fn backfill_skips_users_already_assigned() {
    let tmp = TempDir::new().unwrap();
    let store = open_store_at(tmp.path()).await;

    let alice = store
        .create_user(&NewUser {
            username: "alice".to_string(),
            password_hash: "hash".into(),
        })
        .await
        .unwrap();
    store.set_user_prefix(&alice.id, "WORK").await.unwrap();

    let bob = store
        .create_user(&NewUser {
            username: "bob".to_string(),
            password_hash: "hash".into(),
        })
        .await
        .unwrap();

    let count = backfill_missing_user_prefixes(store.as_ref())
        .await
        .unwrap();
    assert_eq!(count, 1);
    // Alice's prefix is unchanged.
    assert_eq!(
        store.get_user_prefix(&alice.id).await.unwrap().as_deref(),
        Some("WORK")
    );
    assert!(store.get_user_prefix(&bob.id).await.unwrap().is_some());
}

#[tokio::test]
async fn delete_user_cascades_through_task_key_allocations() {
    let tmp = TempDir::new().unwrap();
    let store = open_store_at(tmp.path()).await;

    let alice = store
        .create_user(&NewUser {
            username: "alice".into(),
            password_hash: "hash".into(),
        })
        .await
        .unwrap();
    store.set_user_prefix(&alice.id, "WORK").await.unwrap();
    store
        .ensure_personal_task_scope_for_user(&alice.id)
        .await
        .unwrap();

    // Reserve, attach, commit a few rows; reserve a pending row; reserve
    // and burn one. Mix of states to ensure cascade catches all.
    let task_uuid_1 = Uuid::new_v4().to_string();
    let (n1, attempt1) = store
        .reserve_task_key_pending(&alice.id, "WORK")
        .await
        .unwrap();
    store
        .attach_task_uuid_to_pending(&alice.id, "WORK", n1, &attempt1, &task_uuid_1)
        .await
        .unwrap();
    store
        .commit_task_key(&alice.id, "WORK", n1, &attempt1)
        .await
        .unwrap();

    let (_n2, _attempt2) = store
        .reserve_task_key_pending(&alice.id, "WORK")
        .await
        .unwrap();

    let (n3, attempt3) = store
        .reserve_task_key_pending(&alice.id, "WORK")
        .await
        .unwrap();
    store
        .burn_task_key(&alice.id, "WORK", n3, &attempt3)
        .await
        .unwrap();

    let deleted = store.delete_user(&alice.id).await.unwrap();
    assert!(deleted);

    // Allocations under the deleted user must be gone — verifiable via
    // the unique-uuid index: re-inserting the same task_uuid for a
    // different user must succeed.
    let bob = store
        .create_user(&NewUser {
            username: "bob".into(),
            password_hash: "hash".into(),
        })
        .await
        .unwrap();
    store.set_user_prefix(&bob.id, "WORK").await.unwrap();
    store
        .ensure_personal_task_scope_for_user(&bob.id)
        .await
        .unwrap();
    let (n_bob, attempt_bob) = store
        .reserve_task_key_pending(&bob.id, "WORK")
        .await
        .unwrap();
    // task_uuid_1 was attached to alice's allocation; must be reusable
    // post-delete (the partial unique index is global, so leftover rows
    // would cause this to fail).
    store
        .attach_task_uuid_to_pending(&bob.id, "WORK", n_bob, &attempt_bob, &task_uuid_1)
        .await
        .unwrap();
}

// === Phase 4 task-keys backfill tests (#130 C13) =====================

/// Build an `AppState` rooted at `data_dir` with the given store. Mirrors
/// the production wiring used by `tests/task_keys_create_integration.rs`
/// but skipped the HTTP layer — the backfill function is library-level.
fn make_state(
    data_dir: &std::path::Path,
    store: Arc<dyn ConfigStore>,
    sqlite: Arc<SqliteConfigStore>,
) -> AppState {
    let config = common::test_server_config(data_dir.to_path_buf());
    AppState::new(store, sqlite, &config)
}

/// Seed `n` tasks directly via TaskChampion at `data_dir/users/<user_id>`.
/// Returns the inserted UUIDs in TC's internal order; tests sort/lookup
/// against this list as needed. Each task gets `entry = baseline + i*1s`
/// so the canonical sort is deterministic.
async fn seed_tc_tasks(data_dir: &std::path::Path, user_id: &str, n: usize) -> Vec<Uuid> {
    let user_dir = data_dir.join("users").join(user_id);
    std::fs::create_dir_all(&user_dir).unwrap();
    let storage = SqliteStorage::new(
        &user_dir,
        taskchampion::storage::AccessMode::ReadWrite,
        true,
    )
    .await
    .unwrap();
    let mut rep = Replica::new(storage);
    let baseline = chrono::Utc::now();
    let mut uuids = Vec::with_capacity(n);

    for i in 0..n {
        let mut ops = Operations::new();
        let uuid = Uuid::new_v4();
        let mut task = rep.create_task(uuid, &mut ops).await.unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        task.set_entry(
            Some(baseline + chrono::Duration::seconds(i as i64)),
            &mut ops,
        )
        .unwrap();
        task.set_description(format!("seed-{i}"), &mut ops).unwrap();
        rep.commit_operations(ops).await.unwrap();
        uuids.push(uuid);
    }
    uuids
}

async fn write_cmdock_key_uda(
    data_dir: &std::path::Path,
    user_id: &str,
    task_uuid: Uuid,
    value: &str,
) {
    write_uda(data_dir, user_id, task_uuid, "cmdock_key", value).await;
}

async fn write_uda(
    data_dir: &std::path::Path,
    user_id: &str,
    task_uuid: Uuid,
    key: &str,
    value: &str,
) {
    set_uda(data_dir, user_id, task_uuid, key, Some(value.to_string())).await;
}

async fn clear_uda(data_dir: &std::path::Path, user_id: &str, task_uuid: Uuid, key: &str) {
    set_uda(data_dir, user_id, task_uuid, key, None).await;
}

async fn set_uda(
    data_dir: &std::path::Path,
    user_id: &str,
    task_uuid: Uuid,
    key: &str,
    value: Option<String>,
) {
    let user_dir = data_dir.join("users").join(user_id);
    let storage = SqliteStorage::new(
        &user_dir,
        taskchampion::storage::AccessMode::ReadWrite,
        true,
    )
    .await
    .unwrap();
    let mut rep = Replica::new(storage);
    let mut ops = Operations::new();
    let mut task = rep.get_task(task_uuid).await.unwrap().unwrap();
    task.set_value(key, value, &mut ops).unwrap();
    rep.commit_operations(ops).await.unwrap();
}

async fn read_task_uda(
    data_dir: &std::path::Path,
    user_id: &str,
    task_uuid: Uuid,
    key: &str,
) -> Option<String> {
    let user_dir = data_dir.join("users").join(user_id);
    let storage = SqliteStorage::new(
        &user_dir,
        taskchampion::storage::AccessMode::ReadWrite,
        true,
    )
    .await
    .unwrap();
    let mut rep = Replica::new(storage);
    let task = rep.get_task(task_uuid).await.unwrap()?;
    task.get_value(key).map(|s| s.to_string())
}

async fn read_cmdock_key_uda(
    data_dir: &std::path::Path,
    user_id: &str,
    task_uuid: Uuid,
) -> Option<String> {
    read_task_uda(data_dir, user_id, task_uuid, "cmdock_key").await
}

async fn read_cmdock_account_uda(
    data_dir: &std::path::Path,
    user_id: &str,
    task_uuid: Uuid,
) -> Option<String> {
    read_task_uda(data_dir, user_id, task_uuid, "cmdock_account").await
}

async fn read_cmdock_task_scope_uda(
    data_dir: &std::path::Path,
    user_id: &str,
    task_uuid: Uuid,
) -> Option<String> {
    read_task_uda(data_dir, user_id, task_uuid, "cmdock_task_scope").await
}

async fn create_user_with_prefix(store: &dyn ConfigStore, username: &str, prefix: &str) -> String {
    let user = store
        .create_user(&NewUser {
            username: username.to_string(),
            password_hash: "hash".into(),
        })
        .await
        .unwrap();
    store.set_user_prefix(&user.id, prefix).await.unwrap();
    store
        .ensure_personal_task_scope_for_user(&user.id)
        .await
        .unwrap();
    user.id
}

#[tokio::test]
async fn ensure_migrated_is_noop_for_user_with_no_tasks() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "noop", "NOOP").await;
    std::fs::create_dir_all(data_dir.join("users").join(&user_id)).unwrap();

    let state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();

    // Column populated; no allocation rows.
    let migrated = store
        .get_user_task_keys_migrated_at(&user_id)
        .await
        .unwrap();
    assert!(migrated.is_some());
    assert!(state.recovery_runtime.task_keys_migration_marked(&user_id));
    assert_eq!(
        store
            .lookup_task_keys_by_uuids(&user_id, &[])
            .await
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn ensure_migrated_allocates_keys_in_entry_asc_order() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "alloc", "WORK").await;
    let uuids = seed_tc_tasks(data_dir, &user_id, 5).await;

    let state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();

    // All five tasks have a committed allocation row + UDA.
    let uuid_strs: Vec<String> = uuids.iter().map(|u| u.to_string()).collect();
    let map = store
        .lookup_task_keys_by_uuids(&user_id, &uuid_strs)
        .await
        .unwrap();
    assert_eq!(map.len(), 5, "all five tasks must have a key");

    // Wire keys are entry-asc: uuids[0] → WORK-1, uuids[4] → WORK-5.
    for (i, u) in uuids.iter().enumerate() {
        let expected = format!("WORK-{}", i + 1);
        assert_eq!(map.get(&u.to_string()), Some(&expected));
        assert_eq!(
            read_cmdock_key_uda(data_dir, &user_id, *u).await,
            Some(expected),
            "cmdock_key UDA must match canonical key on TC side",
        );
        assert_eq!(
            read_cmdock_task_scope_uda(data_dir, &user_id, *u).await,
            Some("WORK".to_string()),
            "cmdock_task_scope UDA must match user prefix on TC side",
        );
        assert_eq!(
            read_cmdock_account_uda(data_dir, &user_id, *u).await,
            None,
            "backfill must not stamp deprecated cmdock_account",
        );
    }
}

#[tokio::test]
async fn ensure_migrated_is_idempotent_on_second_call() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "idem", "IDEM").await;
    let uuids = seed_tc_tasks(data_dir, &user_id, 3).await;

    let state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();

    let first = store
        .lookup_task_keys_by_uuids(
            &user_id,
            &uuids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();

    // Second call hits the cache; no new rows.
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();
    let second = store
        .lookup_task_keys_by_uuids(
            &user_id,
            &uuids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    assert_eq!(first, second, "second call must not change allocations");
    for uuid in &uuids {
        assert_eq!(
            read_cmdock_task_scope_uda(data_dir, &user_id, *uuid).await,
            Some("IDEM".to_string()),
            "idempotent rerun must leave cmdock_task_scope populated"
        );
    }
}

#[tokio::test]
async fn ensure_migrated_upgrade_stamps_missing_cmdock_task_scope_for_already_migrated_user() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "upgrade-scope", "UPG").await;
    let uuids = seed_tc_tasks(data_dir, &user_id, 2).await;

    let first_state = make_state(data_dir, store.clone(), sqlite.clone());
    ensure_user_task_keys_migrated(&first_state, &user_id)
        .await
        .unwrap();

    // Simulate a pre-slice upgraded user: allocation rows + cmdock_key +
    // deprecated cmdock_account exist, but canonical cmdock_task_scope is
    // missing on TC tasks. A fresh runtime must repair this even though
    // users.task_keys_migrated_at is already populated.
    for uuid in &uuids {
        clear_uda(data_dir, &user_id, *uuid, "cmdock_task_scope").await;
        assert_eq!(
            read_cmdock_account_uda(data_dir, &user_id, *uuid).await,
            None,
            "backfill must not stamp deprecated cmdock_account"
        );
        assert_eq!(
            read_cmdock_task_scope_uda(data_dir, &user_id, *uuid).await,
            None
        );
    }

    let restarted_state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&restarted_state, &user_id)
        .await
        .unwrap();

    for uuid in &uuids {
        assert_eq!(
            read_cmdock_task_scope_uda(data_dir, &user_id, *uuid).await,
            Some("UPG".to_string()),
            "already-migrated upgrade pass must backfill cmdock_task_scope"
        );
    }
}

#[tokio::test]
async fn ensure_migrated_does_not_run_for_other_users() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let alice = create_user_with_prefix(store.as_ref(), "alice", "ALICE").await;
    let bob = create_user_with_prefix(store.as_ref(), "bob", "BOB").await;
    let _alice_uuids = seed_tc_tasks(data_dir, &alice, 2).await;
    let bob_uuids = seed_tc_tasks(data_dir, &bob, 2).await;

    let state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&state, &alice)
        .await
        .unwrap();

    // Bob remains unmigrated.
    assert!(store
        .get_user_task_keys_migrated_at(&bob)
        .await
        .unwrap()
        .is_none());
    let bob_map = store
        .lookup_task_keys_by_uuids(
            &bob,
            &bob_uuids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    assert_eq!(bob_map.len(), 0, "bob's tasks must not have keys yet");
}

#[tokio::test]
async fn concurrent_first_access_runs_backfill_exactly_once() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "racy", "RACE").await;
    let uuids = seed_tc_tasks(&data_dir, &user_id, 4).await;

    let state = make_state(&data_dir, store.clone(), sqlite);
    let state = Arc::new(state);

    // Spawn N concurrent first-access calls; only one should perform the
    // backfill. The mutation lock + double-checked DB read enforce this.
    let mut handles = Vec::new();
    for _ in 0..10 {
        let st = state.clone();
        let uid = user_id.clone();
        handles.push(tokio::spawn(async move {
            ensure_user_task_keys_migrated(&st, &uid).await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }

    // Exactly four committed rows exist — no double-allocation.
    let map = store
        .lookup_task_keys_by_uuids(
            &user_id,
            &uuids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    assert_eq!(map.len(), 4);
    let n_values: Vec<i64> = map
        .values()
        .map(|s| s.split_once('-').unwrap().1.parse::<i64>().unwrap())
        .collect();
    let mut sorted = n_values.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        vec![1, 2, 3, 4],
        "n values must be 1..4 with no gaps"
    );
}

#[tokio::test]
async fn evict_user_clears_migration_cache_so_second_run_re_reads_db() {
    // Exercises the CLAUDE.md § Runtime cache eviction invariant: the
    // migration_status_cache is cleared via RuntimeRecoveryCoordinator
    // ::evict_user, the single owner of the per-user runtime-cache
    // eviction recipe.
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "evict", "EVCT").await;
    let _uuids = seed_tc_tasks(data_dir, &user_id, 1).await;

    let state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();
    assert!(state.recovery_runtime.task_keys_migration_marked(&user_id));

    state.recovery_runtime.evict_user(&user_id);
    assert!(
        !state.recovery_runtime.task_keys_migration_marked(&user_id),
        "evict_user must clear the migration_status_cache entry",
    );

    // Second call hits the DB double-check (column is populated) and
    // re-marks the cache without doing any allocation work.
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();
    assert!(state.recovery_runtime.task_keys_migration_marked(&user_id));
}

/// Regression lock for codex iter2 important #3 — reconcile policy
/// for pending+attached rows whose TC task no longer exists. The TC
/// commit must have been rolled back (the regular create flow commits
/// TC + sets cmdock_key in one Operations batch), so the allocation
/// row is orphaned. Auto-burn in reconcile, freeing the task_uuid slot.
#[tokio::test]
async fn reconcile_burns_orphan_pending_when_tc_task_missing() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "orph", "ORPH").await;
    let real_uuids = seed_tc_tasks(data_dir, &user_id, 2).await;

    // Reserve+attach a pending row for a TASK UUID THAT WAS NEVER
    // CREATED IN TC. Mirrors a crashed add_task whose TC commit failed
    // mid-batch, leaving the allocation row orphaned.
    let phantom = Uuid::new_v4().to_string();
    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "ORPH")
        .await
        .unwrap();
    store
        .attach_task_uuid_to_pending(&user_id, "ORPH", n, &attempt, &phantom)
        .await
        .unwrap();

    let state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();

    // Real tasks got fresh keys at n=2 and n=3 (the orphan burned n=1
    // but still consumes that slot — the burn detaches task_uuid so
    // there's no UNIQUE collision).
    let map = store
        .lookup_task_keys_by_uuids(
            &user_id,
            &real_uuids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    assert_eq!(map.len(), 2);
    let n_values: Vec<i64> = map
        .values()
        .map(|s| s.split_once('-').unwrap().1.parse::<i64>().unwrap())
        .collect();
    let mut sorted = n_values.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        vec![2, 3],
        "orphan burned n=1 (slot persists); real tasks got n=2,3",
    );
}

/// Regression lock for codex iter2 important #3 — reconcile bails on
/// UDA mismatch (auto-overwriting could mask data loss). The mutation
/// lock keeps the row pending until operator review.
#[tokio::test]
async fn reconcile_bails_on_uda_mismatch() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "mism", "MISM").await;
    let uuids = seed_tc_tasks(data_dir, &user_id, 1).await;

    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "MISM")
        .await
        .unwrap();
    let target_uuid = uuids[0];
    store
        .attach_task_uuid_to_pending(&user_id, "MISM", n, &attempt, &target_uuid.to_string())
        .await
        .unwrap();
    // Set a DIFFERENT cmdock_key UDA than the canonical row would use.
    {
        let user_dir = data_dir.join("users").join(&user_id);
        let storage = SqliteStorage::new(
            &user_dir,
            taskchampion::storage::AccessMode::ReadWrite,
            true,
        )
        .await
        .unwrap();
        let mut rep = Replica::new(storage);
        let mut ops = Operations::new();
        let mut t = rep.get_task(target_uuid).await.unwrap().unwrap();
        t.set_value("cmdock_key", Some("OTHER-99".to_string()), &mut ops)
            .unwrap();
        rep.commit_operations(ops).await.unwrap();
    }

    let state = make_state(data_dir, store.clone(), sqlite);
    let err = ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("mismatch"),
        "error must indicate UDA mismatch; got: {msg}",
    );

    // The user is NOT marked migrated; the pending row stays pending
    // until operator review.
    assert!(store
        .get_user_task_keys_migrated_at(&user_id)
        .await
        .unwrap()
        .is_none());
    assert!(!state.recovery_runtime.task_keys_migration_marked(&user_id));
}

/// Regression lock for codex iter4 important — a concurrent admin
/// `set-prefix` that lands between the backfill's prefix read (Phase B
/// precompute) and the atomic Phase A+C commit must abort the commit
/// rather than silently writing rows under one prefix while
/// `users.prefix` already names another. Drive the store primitive
/// directly with a mismatched expected_max_n_OK + prefix-shift to
/// confirm the explicit `BackfillPrefixChanged` failure shape.
#[tokio::test]
async fn commit_backfill_rejects_when_users_prefix_shifted() {
    use cmdock_server::store::StoreError;
    let tmp = TempDir::new().unwrap();
    let store = open_store_at(tmp.path()).await;

    let alice = store
        .create_user(&NewUser {
            username: "shft".into(),
            password_hash: "hash".into(),
        })
        .await
        .unwrap();
    store.set_user_prefix(&alice.id, "OLD").await.unwrap();
    // No allocation rows yet; prefix is mutable. Simulate the admin
    // racing the backfill: change prefix between Phase B read and
    // Phase A+C commit. (No allocations exist + migrated_at is NULL,
    // so PrefixLocked does NOT fire here — that's the gap.)
    store.set_user_prefix(&alice.id, "NEW").await.unwrap();

    let task_uuid = Uuid::new_v4().to_string();
    let err = store
        .commit_backfill_allocations_for_user(&alice.id, "OLD", 0, &[task_uuid])
        .await
        .unwrap_err();
    match err {
        StoreError::BackfillPrefixChanged { expected, actual } => {
            assert_eq!(expected, "OLD");
            assert_eq!(actual.as_deref(), Some("NEW"));
        }
        other => panic!("expected BackfillPrefixChanged, got {other:?}"),
    }

    // No allocation rows landed; users.prefix is unchanged.
    assert!(store
        .get_user_task_keys_migrated_at(&alice.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store.get_user_prefix(&alice.id).await.unwrap().as_deref(),
        Some("NEW")
    );
}

/// Regression lock for codex iter4 nice-to-have — TC tasks with an
/// **empty-string** `cmdock_key` UDA must classify as SkipUdaMismatch
/// (not Burn). TaskChampion preserves `Some(\"\")`, the index records
/// it, and the reaper's three-way decision treats it as a present-but-
/// non-matching UDA. Pre-iter3 the reaper would have burned this row.
#[tokio::test]
async fn reaper_skips_uuid_attached_row_with_empty_string_uda() {
    use cmdock_server::config::TaskWriteSection;
    use cmdock_server::task_keys::run_reaper_pass;

    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();
    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "emp", "EMP").await;
    let uuids = seed_tc_tasks(data_dir, &user_id, 1).await;
    let target = uuids[0];

    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "EMP")
        .await
        .unwrap();
    store
        .attach_task_uuid_to_pending(&user_id, "EMP", n, &attempt, &target.to_string())
        .await
        .unwrap();
    {
        let user_dir = data_dir.join("users").join(&user_id);
        let storage = SqliteStorage::new(
            &user_dir,
            taskchampion::storage::AccessMode::ReadWrite,
            true,
        )
        .await
        .unwrap();
        let mut rep = Replica::new(storage);
        let mut ops = Operations::new();
        let mut t = rep.get_task(target).await.unwrap().unwrap();
        t.set_value("cmdock_key", Some(String::new()), &mut ops)
            .unwrap();
        rep.commit_operations(ops).await.unwrap();
    }
    // Make the row stale.
    let raw = tokio_rusqlite::Connection::open(&db_path).await.unwrap();
    let user_id_for_sql = user_id.clone();
    raw.call(move |c| {
        c.execute(
            "UPDATE task_key_allocations
                SET created_at = datetime('now', '-1 hour')
              WHERE user_id = ?1 AND prefix = 'EMP' AND n = ?2",
            rusqlite::params![user_id_for_sql, n],
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .unwrap();

    let mut config = common::test_server_config(data_dir.to_path_buf());
    config.task_write = TaskWriteSection {
        idempotency_pending_timeout_seconds: 1,
        ..config.task_write
    };
    let state = AppState::new(store, sqlite, &config);
    let outcome = run_reaper_pass(&state).await;
    assert_eq!(
        outcome.skipped_uda_mismatch, 1,
        "empty-string cmdock_key must skip-for-review, not burn",
    );
    assert_eq!(outcome.burned, 0);
}

/// Regression lock for codex iter3 critical — `set_user_prefix` must
/// reject a fresh user who has been marked migrated but has no
/// allocation rows yet (empty-account backfill landed). Otherwise an
/// operator could re-prefix a user who is already serving wire keys
/// to clients on the old prefix.
#[tokio::test]
async fn set_prefix_rejects_after_empty_account_backfill_marks_migrated() {
    use cmdock_server::store::StoreError;
    let tmp = TempDir::new().unwrap();
    let store = open_store_at(tmp.path()).await;

    let alice = store
        .create_user(&NewUser {
            username: "empty".into(),
            password_hash: "hash".into(),
        })
        .await
        .unwrap();
    store.set_user_prefix(&alice.id, "EMPTY").await.unwrap();
    // No tasks, no allocation rows — but mark migrated (the
    // empty-candidate branch in the backfill flow).
    store.mark_user_task_keys_migrated(&alice.id).await.unwrap();

    let err = store
        .set_user_prefix(&alice.id, "RENAME")
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::PrefixLocked),
        "expected PrefixLocked once task_keys_migrated_at IS NOT NULL; got {err:?}",
    );
}

/// Regression lock for codex iter3 critical — the reaper's
/// uuid-attached decision branch must distinguish UDA-mismatch from
/// genuinely-orphan rows. Pre-iter3 the reaper burned both, defeating
/// `reconcile_pending_attached_rows`'s mismatch-bail policy: an
/// operator review window opened by Phase 4 could close silently when
/// the reaper later swept through.
///
/// Setup: pending+attached row whose TC task exists with a DIFFERENT
/// `cmdock_key` UDA than canonical. A reaper pass with a 1s timeout
/// must classify as `SkipUdaMismatch` and leave the row pending.
#[tokio::test]
async fn reaper_skips_uuid_attached_row_with_uda_mismatch_for_operator_review() {
    use cmdock_server::config::TaskWriteSection;
    use cmdock_server::task_keys::run_reaper_pass;

    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "rmm", "RMM").await;
    let uuids = seed_tc_tasks(data_dir, &user_id, 1).await;
    let target = uuids[0];

    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "RMM")
        .await
        .unwrap();
    store
        .attach_task_uuid_to_pending(&user_id, "RMM", n, &attempt, &target.to_string())
        .await
        .unwrap();
    {
        let user_dir = data_dir.join("users").join(&user_id);
        let storage = SqliteStorage::new(
            &user_dir,
            taskchampion::storage::AccessMode::ReadWrite,
            true,
        )
        .await
        .unwrap();
        let mut rep = Replica::new(storage);
        let mut ops = Operations::new();
        let mut t = rep.get_task(target).await.unwrap().unwrap();
        t.set_value("cmdock_key", Some("OTHER-99".to_string()), &mut ops)
            .unwrap();
        rep.commit_operations(ops).await.unwrap();
    }
    // Roll the row's created_at back so the reaper considers it stale.
    let raw = tokio_rusqlite::Connection::open(&db_path).await.unwrap();
    let user_id_for_sql = user_id.clone();
    raw.call(move |c| {
        c.execute(
            "UPDATE task_key_allocations
                SET created_at = datetime('now', '-1 hour')
              WHERE user_id = ?1 AND prefix = 'RMM' AND n = ?2",
            rusqlite::params![user_id_for_sql, n],
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .unwrap();

    let mut config = common::test_server_config(data_dir.to_path_buf());
    config.task_write = TaskWriteSection {
        idempotency_pending_timeout_seconds: 1,
        ..config.task_write
    };
    let state = AppState::new(store.clone(), sqlite, &config);
    let outcome = run_reaper_pass(&state).await;

    assert_eq!(
        outcome.skipped_uda_mismatch, 1,
        "reaper must classify the mismatched row as skipped, not burned",
    );
    assert_eq!(outcome.burned, 0);
    assert_eq!(outcome.finalised, 0);

    // Row stays pending+attached.
    let after = store
        .lookup_task_key_by_uuid(&user_id, &target.to_string())
        .await
        .unwrap();
    assert!(matches!(after, Some((_, ks)) if format!("{ks:?}").contains("Pending")));
}

/// Regression lock for codex iter3 important #2 — migration 028 must
/// detach `task_uuid` from any pre-existing burned rows on upgrade so
/// a fresh Phase 4 backfill for the same task UUID does not collide
/// on `idx_task_key_allocations_uuid`. We seed a burned row with a
/// non-NULL `task_uuid` directly via SQL (simulating pre-iter2 state),
/// then re-run migrations and confirm the column was nulled.
#[tokio::test]
async fn migration_028_detaches_task_uuid_from_pre_iter2_burned_rows() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();
    let db_path = data_dir.join("config.sqlite");

    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "leg".into(),
            password_hash: "hash".into(),
        })
        .await
        .unwrap();
    store.set_user_prefix(&user.id, "LEG").await.unwrap();

    // Insert a burned row with non-NULL task_uuid via raw SQL (the
    // current burn primitive nulls task_uuid, so we cannot construct
    // this state through the typed API — but pre-iter2 deployments
    // accumulated rows like this and the migration must clean them up).
    let stale_uuid = Uuid::new_v4().to_string();
    let raw = tokio_rusqlite::Connection::open(&db_path).await.unwrap();
    let user_id = user.id.clone();
    let stale_uuid_clone = stale_uuid.clone();
    raw.call(move |c| {
        c.execute(
            "INSERT INTO task_key_allocations
                (user_id, prefix, n, task_uuid, state, attempt_id)
             VALUES (?1, 'LEG', 1, ?2, 'burned', 'legacy-attempt')",
            rusqlite::params![user_id, stale_uuid_clone],
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .unwrap();

    // The migration runner tracks applied migrations in the
    // `_migrations` table, so a second `run_migrations` call is a
    // no-op for already-applied entries. Replay 028 by deleting its
    // entry and re-running. This is the only honest way to assert the
    // SQL's cleanup semantics without forking the runner.
    let user_id_clone = user.id.clone();
    raw.call(move |c| {
        c.execute(
            "DELETE FROM _migrations WHERE name = ?1",
            ["028_burned_task_keys_detach_uuid.sql"],
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .unwrap();
    store.run_migrations().await.unwrap();

    let detached: Option<String> = raw
        .call(move |c| {
            let row: Option<Option<String>> = c
                .query_row(
                    "SELECT task_uuid FROM task_key_allocations
                     WHERE user_id = ?1 AND prefix = 'LEG' AND n = 1",
                    [&user_id_clone],
                    |r| r.get(0),
                )
                .ok();
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(row.flatten())
        })
        .await
        .unwrap();
    assert!(
        detached.is_none(),
        "migration 028 must NULL task_uuid on pre-iter2 burned rows; got {detached:?}",
    );
    let _ = stale_uuid; // kept for documentation purposes
}

/// Regression lock for codex iter2 important #2 — burned rows must
/// detach `task_uuid` so the partial unique index does not block a
/// future allocation for the same task UUID. Construct a burned row
/// referencing a TC task that still exists, then run the backfill —
/// it must allocate a fresh row for that task, not 500 on the index
/// collision.
#[tokio::test]
async fn ensure_migrated_succeeds_when_burned_row_held_same_task_uuid() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "brn", "BRN").await;
    let uuids = seed_tc_tasks(data_dir, &user_id, 1).await;

    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "BRN")
        .await
        .unwrap();
    store
        .attach_task_uuid_to_pending(&user_id, "BRN", n, &attempt, &uuids[0].to_string())
        .await
        .unwrap();
    store
        .burn_task_key(&user_id, "BRN", n, &attempt)
        .await
        .unwrap();

    let state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();

    // Backfill allocated a fresh row at n=2 (n=1 stays burned, slot
    // permanently consumed). The task_uuid attached to the burned row
    // was detached on burn, so the partial unique index allowed the
    // fresh insert.
    let map = store
        .lookup_task_keys_by_uuids(&user_id, &[uuids[0].to_string()])
        .await
        .unwrap();
    assert_eq!(map.get(&uuids[0].to_string()), Some(&"BRN-2".to_string()));
}

/// Regression lock for codex iter1 important #3 —
/// `commit_backfill_allocations_for_user` must reject when MAX(n) has
/// shifted between the Phase B precompute and the Phase A+C commit.
/// Single-process deployments under the per-user mutation lock cannot
/// trip this organically, so we drive the store primitive directly to
/// confirm the explicit `BackfillMaxChanged` failure shape.
#[tokio::test]
async fn commit_backfill_rejects_when_max_n_shifts() {
    use cmdock_server::store::StoreError;
    let tmp = TempDir::new().unwrap();
    let store = open_store_at(tmp.path()).await;

    let user = store
        .create_user(&NewUser {
            username: "max_shift".into(),
            password_hash: "hash".into(),
        })
        .await
        .unwrap();
    store.set_user_prefix(&user.id, "MAX").await.unwrap();
    store
        .ensure_personal_task_scope_for_user(&user.id)
        .await
        .unwrap();

    // Simulate a stale Phase B precompute: read MAX(n)=0, then a racing
    // allocation lands an n=1 row, then we try to commit with the stale
    // expected_max_n=0.
    let stale_expected = 0;
    let (n1, attempt1) = store
        .reserve_task_key_pending(&user.id, "MAX")
        .await
        .unwrap();
    let racing_uuid = Uuid::new_v4().to_string();
    store
        .attach_task_uuid_to_pending(&user.id, "MAX", n1, &attempt1, &racing_uuid)
        .await
        .unwrap();
    store
        .commit_task_key(&user.id, "MAX", n1, &attempt1)
        .await
        .unwrap();

    let target_uuid = Uuid::new_v4().to_string();
    let err = store
        .commit_backfill_allocations_for_user(&user.id, "MAX", stale_expected, &[target_uuid])
        .await
        .unwrap_err();
    match err {
        StoreError::BackfillMaxChanged { expected, actual } => {
            assert_eq!(expected, 0);
            assert_eq!(actual, 1);
        }
        other => panic!("expected BackfillMaxChanged, got {other:?}"),
    }

    // The user is NOT marked migrated.
    assert!(store
        .get_user_task_keys_migrated_at(&user.id)
        .await
        .unwrap()
        .is_none());
}

/// Regression lock for codex iter1 important #4 — concurrent
/// `delete_user` between lock acquire and commit must be detected
/// inside the commit transaction. We exercise the explicit failure
/// shape via the store primitive.
#[tokio::test]
async fn commit_backfill_rejects_when_user_was_deleted() {
    use cmdock_server::store::StoreError;
    let tmp = TempDir::new().unwrap();
    let store = open_store_at(tmp.path()).await;

    let target_uuid = Uuid::new_v4().to_string();
    let err = store
        .commit_backfill_allocations_for_user("does-not-exist", "GONE", 0, &[target_uuid])
        .await
        .unwrap_err();
    match err {
        StoreError::BackfillUserMissing => {}
        other => panic!("expected BackfillUserMissing, got {other:?}"),
    }
}

/// Regression lock for codex iter1 critical #2 — pending+attached
/// allocation rows must be reconciled BEFORE the candidate filter so
/// the atomic Phase A+C insert can't collide on
/// `idx_task_key_allocations_uuid`. We seed three TC tasks, then
/// manually reserve+attach a pending row for the first task (with the
/// matching `cmdock_key` UDA) — simulating a previously crashed
/// `service::add_task` that wrote the UDA but never reached
/// `commit_task_key`. The backfill run must finalise that pending row
/// (transition to `committed` with the original `n` preserved), then
/// allocate fresh rows for the remaining two tasks.
#[tokio::test]
async fn ensure_migrated_reconciles_pending_attached_rows() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "pend", "PEND").await;
    let uuids = seed_tc_tasks(data_dir, &user_id, 3).await;

    // Simulate a crashed add_task that reached step 8 of service.rs
    // (TC commit) but never finalised step 9 (commit_task_key): a
    // pending+attached row exists with the matching cmdock_key UDA on
    // the TC task.
    let (n_pending, attempt_id) = store
        .reserve_task_key_pending(&user_id, "PEND")
        .await
        .unwrap();
    assert_eq!(n_pending, 1);
    let target_uuid = uuids[0];
    store
        .attach_task_uuid_to_pending(
            &user_id,
            "PEND",
            n_pending,
            &attempt_id,
            &target_uuid.to_string(),
        )
        .await
        .unwrap();
    {
        let user_dir = data_dir.join("users").join(&user_id);
        let storage = SqliteStorage::new(
            &user_dir,
            taskchampion::storage::AccessMode::ReadWrite,
            true,
        )
        .await
        .unwrap();
        let mut rep = Replica::new(storage);
        let mut ops = Operations::new();
        let mut t = rep.get_task(target_uuid).await.unwrap().unwrap();
        t.set_value("cmdock_key", Some("PEND-1".to_string()), &mut ops)
            .unwrap();
        rep.commit_operations(ops).await.unwrap();
    }

    let state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();

    // The pending row was finalised in place — original n preserved.
    // The remaining two tasks got fresh rows at n=2 and n=3.
    let map = store
        .lookup_task_keys_by_uuids(
            &user_id,
            &uuids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    assert_eq!(map.len(), 3);
    assert_eq!(map.get(&uuids[0].to_string()), Some(&"PEND-1".to_string()));
    let n2: i64 = map[&uuids[1].to_string()]
        .strip_prefix("PEND-")
        .unwrap()
        .parse()
        .unwrap();
    let n3: i64 = map[&uuids[2].to_string()]
        .strip_prefix("PEND-")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(n2, 2);
    assert_eq!(n3, 3);
    for uuid in &uuids {
        assert_eq!(
            read_cmdock_account_uda(data_dir, &user_id, *uuid).await,
            None,
            "backfill must not stamp deprecated cmdock_account",
        );
    }
}

/// Regression lock for the recovery branch: a previous crashed backfill
/// wrote `cmdock_key` UDAs but never committed Phase A+C (zero
/// allocation rows). The retry must observe the matching UDA values,
/// re-stamp the allocation rows in the same canonical order, and end
/// with both wire `key` populated and the migrated_at column set. The
/// `task.key.migration_recovery` audit branch fires when at least one
/// pre-existing UDA matches what the backfill would write.
#[tokio::test]
async fn ensure_migrated_recovers_when_phase_b_partially_completed() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user_id = create_user_with_prefix(store.as_ref(), "rec", "REC").await;
    let uuids = seed_tc_tasks(data_dir, &user_id, 5).await;

    // Simulate a crashed Phase B that wrote the `cmdock_key` UDA on the
    // first three tasks but never reached the atomic Phase A+C commit.
    // We pre-stamp the canonical key the backfill would compute under
    // the lock — `MAX(n)` is 0 (no rows), so n_i = i+1.
    {
        let user_dir = data_dir.join("users").join(&user_id);
        let storage = SqliteStorage::new(
            &user_dir,
            taskchampion::storage::AccessMode::ReadWrite,
            true,
        )
        .await
        .unwrap();
        let mut rep = Replica::new(storage);
        let mut ops = Operations::new();
        for (i, uuid) in uuids.iter().take(3).enumerate() {
            let mut t = rep.get_task(*uuid).await.unwrap().unwrap();
            t.set_value("cmdock_key", Some(format!("REC-{}", i + 1)), &mut ops)
                .unwrap();
        }
        rep.commit_operations(ops).await.unwrap();
    }

    // Pre-conditions: no allocation rows yet.
    let migrated = store
        .get_user_task_keys_migrated_at(&user_id)
        .await
        .unwrap();
    assert!(migrated.is_none(), "no migration row before recovery");

    // Run the backfill. The first three UDAs are already correct; the
    // last two need to be written. Phase A+C inserts all five rows.
    let state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();

    // All five tasks now have committed allocation rows mapping to
    // REC-1..REC-5 in entry-asc order.
    let map = store
        .lookup_task_keys_by_uuids(
            &user_id,
            &uuids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    assert_eq!(map.len(), 5);
    for (i, u) in uuids.iter().enumerate() {
        assert_eq!(map.get(&u.to_string()), Some(&format!("REC-{}", i + 1)));
        assert_eq!(
            read_cmdock_key_uda(data_dir, &user_id, *u).await,
            Some(format!("REC-{}", i + 1)),
            "all UDAs end up canonical (pre-existing kept, missing written)",
        );
        assert_eq!(
            read_cmdock_account_uda(data_dir, &user_id, *u).await,
            None,
            "backfill must not stamp deprecated cmdock_account",
        );
    }
    assert!(state.recovery_runtime.task_keys_migration_marked(&user_id));
}

/// Regression for the staging E2E "Deleted user token → unexpected 500"
/// failure (#137 follow-up). When a user is deleted but their bearer
/// token is still in the auth cache (documented stale-cache window),
/// the read handler may invoke `ensure_user_task_keys_migrated` for
/// a user whose record no longer exists. Pre-fix: the helper bailed
/// with "user has no prefix", which the handler mapped to 500. Post-
/// fix: the helper distinguishes "user gone" (silent Ok — let auth
/// cache TTL handle eviction) from "user exists, no prefix" (real
/// regression — keep the error so operators see Phase 1 gaps).
#[tokio::test]
async fn ensure_migrated_silently_succeeds_when_user_was_deleted() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let store = open_store_at(data_dir).await;
    let sqlite = Arc::new(
        SqliteConfigStore::new(&data_dir.join("config.sqlite").to_string_lossy())
            .await
            .unwrap(),
    );

    let user_id = create_user_with_prefix(store.as_ref(), "alice", "ALICE").await;
    // Delete the user. Their auth cache entry would still resolve to
    // this user_id for the documented stale-cache window.
    assert!(store.delete_user(&user_id).await.unwrap());

    let state = make_state(data_dir, store.clone(), sqlite);
    // Must NOT bail — silent Ok lets the read handler proceed;
    // the auth cache TTL closes the window without surfacing 500.
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .expect("deleted-user path must not error");
}

// === Phase 5e: orphan reconciliation tests (#130) ====================
//
// "Foreign UDA" = a `cmdock_key` value on the canonical replica that does
// not match the canonical key the backfill would assign for that task.
// Source populations include pre-feature TC sync segments carrying
// imported UDA values (`task-write-contract.md` § Orphan reconciliation).
// Backfill must allocate fresh next-N (NEVER adopt the encoded N, even if
// the encoded N happens to be unallocated) and overwrite the UDA.

#[tokio::test]
async fn orphan_foreign_uda_replaced_with_fresh_n() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let store = open_store_at(data_dir).await;
    let sqlite = Arc::new(
        SqliteConfigStore::new(&data_dir.join("config.sqlite").to_string_lossy())
            .await
            .unwrap(),
    );

    let user_id = create_user_with_prefix(store.as_ref(), "alice", "ALICE").await;
    let uuids = seed_tc_tasks(data_dir, &user_id, 1).await;
    let task_uuid = uuids[0];

    // Foreign UDA — totally unrelated value (different prefix, different N).
    write_cmdock_key_uda(data_dir, &user_id, task_uuid, "WHATEVER-42").await;
    write_uda(data_dir, &user_id, task_uuid, "cmdock_account", "FOREIGN").await;

    let state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();

    // Backfill picks ALICE-1 (max+1=1 since no rows existed); the foreign
    // value is overwritten on the canonical replica.
    let map = store
        .lookup_task_keys_by_uuids(&user_id, &[task_uuid.to_string()])
        .await
        .unwrap();
    assert_eq!(
        map.get(&task_uuid.to_string()),
        Some(&"ALICE-1".to_string()),
        "orphan must receive fresh-N (ALICE-1), not adopt foreign value"
    );
    assert_eq!(
        read_cmdock_key_uda(data_dir, &user_id, task_uuid).await,
        Some("ALICE-1".to_string()),
        "canonical UDA overwritten with fresh-N"
    );
    assert_eq!(
        read_cmdock_account_uda(data_dir, &user_id, task_uuid).await,
        Some("FOREIGN".to_string()),
        "backfill must not overwrite pre-existing cmdock_account (deprecated field)"
    );
}

#[tokio::test]
async fn orphan_foreign_uda_does_not_adopt_encoded_n_even_when_unallocated() {
    // The fresh-N rule preserves "burned numbers never re-allocate". Even
    // when the foreign UDA encodes the SAME prefix, backfill MUST pick
    // max(n)+1 — never the encoded N. Setup: max(n) over all states = 3
    // (committed via burn), foreign UDA encodes ALICE-3 (which matches
    // the burned slot), backfill picks ALICE-4.
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let store = open_store_at(data_dir).await;
    let sqlite = Arc::new(
        SqliteConfigStore::new(&data_dir.join("config.sqlite").to_string_lossy())
            .await
            .unwrap(),
    );

    let user_id = create_user_with_prefix(store.as_ref(), "alice", "ALICE").await;

    // Seed allocation table state: 1 committed, 2 committed, 3 burned.
    // (burn detaches task_uuid from the row; max(n) over all states = 3.)
    let other_task_a = Uuid::new_v4().to_string();
    let other_task_b = Uuid::new_v4().to_string();
    for (n_expected, task_uuid) in [(1i64, &other_task_a), (2i64, &other_task_b)] {
        let (n, attempt) = store
            .reserve_task_key_pending(&user_id, "ALICE")
            .await
            .unwrap();
        assert_eq!(n, n_expected);
        store
            .attach_task_uuid_to_pending(&user_id, "ALICE", n, &attempt, task_uuid)
            .await
            .unwrap();
        store
            .commit_task_key(&user_id, "ALICE", n, &attempt)
            .await
            .unwrap();
    }
    let (n3, attempt3) = store
        .reserve_task_key_pending(&user_id, "ALICE")
        .await
        .unwrap();
    assert_eq!(n3, 3);
    store
        .burn_task_key(&user_id, "ALICE", n3, &attempt3)
        .await
        .unwrap();

    // Seed one TC task carrying a foreign UDA encoding the burned N.
    let uuids = seed_tc_tasks(data_dir, &user_id, 1).await;
    let task_uuid = uuids[0];
    write_cmdock_key_uda(data_dir, &user_id, task_uuid, "ALICE-3").await;
    write_uda(data_dir, &user_id, task_uuid, "cmdock_account", "FOREIGN").await;

    let state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();

    // Backfill picks ALICE-4 (max+1), not ALICE-3 (the burned/encoded N).
    let map = store
        .lookup_task_keys_by_uuids(&user_id, &[task_uuid.to_string()])
        .await
        .unwrap();
    assert_eq!(
        map.get(&task_uuid.to_string()),
        Some(&"ALICE-4".to_string()),
        "orphan picks max+1 (ALICE-4); burned ALICE-3 must NOT re-allocate"
    );
    assert_eq!(
        read_cmdock_key_uda(data_dir, &user_id, task_uuid).await,
        Some("ALICE-4".to_string()),
    );
    assert_eq!(
        read_cmdock_account_uda(data_dir, &user_id, task_uuid).await,
        Some("FOREIGN".to_string()),
        "backfill must not overwrite pre-existing cmdock_account (deprecated field)"
    );
}

#[tokio::test]
async fn orphan_mixed_with_empty_uda_preserves_entry_asc_ordering() {
    // Mixed batch: tasks at entry baseline+0s/+1s/+2s. The middle task
    // carries a foreign UDA; the others have empty UDA. After backfill,
    // all three must receive ALICE-1/ALICE-2/ALICE-3 in entry-asc order
    // — the orphan branch must not perturb sequencing.
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let store = open_store_at(data_dir).await;
    let sqlite = Arc::new(
        SqliteConfigStore::new(&data_dir.join("config.sqlite").to_string_lossy())
            .await
            .unwrap(),
    );

    let user_id = create_user_with_prefix(store.as_ref(), "alice", "ALICE").await;
    let uuids = seed_tc_tasks(data_dir, &user_id, 3).await;

    // Foreign UDA on the middle task only.
    write_cmdock_key_uda(data_dir, &user_id, uuids[1], "GHOST-99").await;
    write_uda(data_dir, &user_id, uuids[1], "cmdock_account", "GHOST").await;

    let state = make_state(data_dir, store.clone(), sqlite);
    ensure_user_task_keys_migrated(&state, &user_id)
        .await
        .unwrap();

    let map = store
        .lookup_task_keys_by_uuids(
            &user_id,
            &uuids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    // Expected cmdock_account per task: None for empty-UDA tasks, preserved
    // foreign value for the middle task (backfill must not write this field).
    let expected_accounts: [Option<&str>; 3] = [None, Some("GHOST"), None];
    for (i, uuid) in uuids.iter().enumerate() {
        let expected = format!("ALICE-{}", i + 1);
        assert_eq!(
            map.get(&uuid.to_string()),
            Some(&expected),
            "task {i} must receive {expected} regardless of UDA presence"
        );
        assert_eq!(
            read_cmdock_key_uda(data_dir, &user_id, *uuid).await,
            Some(expected),
            "task {i} canonical UDA"
        );
        assert_eq!(
            read_cmdock_account_uda(data_dir, &user_id, *uuid).await,
            expected_accounts[i].map(|s| s.to_string()),
            "task {i} account UDA (backfill must not write deprecated cmdock_account)"
        );
    }
}
