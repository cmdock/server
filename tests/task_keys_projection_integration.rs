//! REST projection lookup-time expiry tests (#130 Phase 5d).
//!
//! Covers the contract rule from `task-write-contract.md` § REST projects
//! from the allocation table:
//!
//! > REST projects from:
//! >   - Every `committed` row.
//! >   - Every `pending` row whose `created_at + pending_timeout > now`.
//! > `burned` rows are invisible.
//!
//! REST correctness is decoupled from reaper scheduling — a stranded
//! pending row whose timeout has expired projects as `null` even if the
//! reaper has not yet physically transitioned it to `burned`.

mod common;

use std::sync::Arc;

use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use tempfile::TempDir;

async fn build_store(data_dir: std::path::PathBuf) -> Arc<dyn ConfigStore> {
    let db_path = data_dir.join("config.sqlite");
    std::fs::create_dir_all(data_dir.join("users")).unwrap();
    let sqlite_store = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite_store.clone();
    store.run_migrations().await.unwrap();
    store
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

/// Mirror of the reaper test helper. Forges `created_at` on the only
/// pending row matching `(user_id, n)` so we can assert lookup-time
/// expiry without waiting wall-clock seconds.
async fn forge_created_at(db_path: &std::path::Path, user_id: &str, n: i64, age_seconds: i64) {
    let conn = tokio_rusqlite::Connection::open(db_path).await.unwrap();
    let user_id = user_id.to_string();
    conn.call(move |conn| {
        conn.execute(
            &format!(
                "UPDATE task_key_allocations \
                 SET created_at = datetime('now', '-{age_seconds} seconds') \
                 WHERE user_id = ?1 AND n = ?2"
            ),
            rusqlite::params![user_id, n],
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .unwrap();
}

const PENDING_TIMEOUT_SECONDS: u32 = 300;

/// Helper: invoke projection with a fixed `now` (wall-clock unix seconds).
/// Test code uses `chrono::Utc::now().timestamp()` so the relative ages set
/// by `forge_created_at` line up with the reference time the projection
/// evaluates against.
async fn projection(
    store: &Arc<dyn ConfigStore>,
    user_id: &str,
    uuids: &[String],
) -> std::collections::HashMap<String, String> {
    let now = chrono::Utc::now().timestamp();
    store
        .lookup_task_keys_for_projection(user_id, uuids, now, PENDING_TIMEOUT_SECONDS)
        .await
        .unwrap()
}

#[tokio::test]
async fn pending_within_window_projects_key() {
    let tmp = TempDir::new().unwrap();
    let store = build_store(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "alice").await;

    let task_uuid = uuid::Uuid::new_v4().to_string();
    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid)
        .await
        .unwrap();

    // Fresh pending row (created_at = now, within window).
    let map = projection(&store, &user_id, &[task_uuid.clone()]).await;
    assert_eq!(
        map.get(&task_uuid).map(String::as_str),
        Some(format!("WORK-{n}").as_str()),
        "non-expired pending row must project its key",
    );
}

#[tokio::test]
async fn pending_past_timeout_projects_null() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let store = build_store(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "bob").await;

    let task_uuid = uuid::Uuid::new_v4().to_string();
    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid)
        .await
        .unwrap();

    // Forge created_at to PENDING_TIMEOUT + 1 second ago. Row is still
    // physically `pending` in the DB — the reaper has not run.
    forge_created_at(
        &db_path,
        &user_id,
        n,
        i64::from(PENDING_TIMEOUT_SECONDS) + 1,
    )
    .await;

    let map = projection(&store, &user_id, &[task_uuid.clone()]).await;
    assert!(
        !map.contains_key(&task_uuid),
        "expired pending row must project as null (absent from map): got {:?}",
        map.get(&task_uuid),
    );
}

#[tokio::test]
async fn burned_row_projects_null() {
    let tmp = TempDir::new().unwrap();
    let store = build_store(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "carol").await;

    let task_uuid = uuid::Uuid::new_v4().to_string();
    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid)
        .await
        .unwrap();
    store
        .burn_task_key(&user_id, "WORK", n, &attempt)
        .await
        .unwrap();

    let map = projection(&store, &user_id, &[task_uuid.clone()]).await;
    assert!(
        !map.contains_key(&task_uuid),
        "burned row must never project a key (allocation table source-of-truth)",
    );
}

#[tokio::test]
async fn committed_row_always_projects_key() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let store = build_store(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "dave").await;

    let task_uuid = uuid::Uuid::new_v4().to_string();
    let (n, attempt) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n, &attempt, &task_uuid)
        .await
        .unwrap();
    store
        .commit_task_key(&user_id, "WORK", n, &attempt)
        .await
        .unwrap();

    // Even with a forged-old created_at, committed rows are not subject
    // to expiry — only pending rows are.
    forge_created_at(
        &db_path,
        &user_id,
        n,
        i64::from(PENDING_TIMEOUT_SECONDS) * 10,
    )
    .await;

    let map = projection(&store, &user_id, &[task_uuid.clone()]).await;
    assert_eq!(
        map.get(&task_uuid).map(String::as_str),
        Some(format!("WORK-{n}").as_str()),
        "committed rows must always project their key (no expiry)",
    );
}

#[tokio::test]
async fn no_allocation_row_projects_null() {
    // Pre-feature / orphan case from `task-write-contract.md` § Wire
    // exposure (causes 1 + 4): no row in `task_key_allocations` → null.
    let tmp = TempDir::new().unwrap();
    let store = build_store(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "ellis").await;

    let phantom = uuid::Uuid::new_v4().to_string();
    let map = projection(&store, &user_id, &[phantom.clone()]).await;
    assert!(
        map.is_empty(),
        "task UUID with no allocation row must project as null",
    );
}

#[tokio::test]
async fn reaper_lag_independence() {
    // Pin the contract guarantee: REST correctness is decoupled from
    // reaper scheduling. A row physically `pending` in the DB whose
    // `created_at` is far past the timeout projects as null on REST,
    // identically to a burned row.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("config.sqlite");
    let store = build_store(tmp.path().to_path_buf()).await;
    let user_id = create_user(&store, "frank").await;

    let stranded = uuid::Uuid::new_v4().to_string();
    let (n_pending, attempt_p) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n_pending, &attempt_p, &stranded)
        .await
        .unwrap();

    // 10 minutes past timeout; reaper has not run.
    forge_created_at(&db_path, &user_id, n_pending, 600).await;

    let burned = uuid::Uuid::new_v4().to_string();
    let (n_burned, attempt_b) = store
        .reserve_task_key_pending(&user_id, "WORK")
        .await
        .unwrap();
    store
        .attach_task_uuid_to_pending(&user_id, "WORK", n_burned, &attempt_b, &burned)
        .await
        .unwrap();
    store
        .burn_task_key(&user_id, "WORK", n_burned, &attempt_b)
        .await
        .unwrap();

    let map = projection(&store, &user_id, &[stranded.clone(), burned.clone()]).await;
    assert!(
        !map.contains_key(&stranded),
        "stranded pending (reaper-lagged) must project null identically to burned",
    );
    assert!(!map.contains_key(&burned), "burned must project null");
}
