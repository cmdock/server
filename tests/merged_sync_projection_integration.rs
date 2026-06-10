//! Phase 4 merged-sync personal projection integration tests.

mod common;

use std::sync::Arc;

use cmdock_server::app_state::AppState;
use cmdock_server::merged_sync_gateway::projection::project_personal_now;
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use taskchampion::storage::AccessMode;
use taskchampion::{Annotation, Operations, Replica, SqliteStorage, Status};
use tempfile::TempDir;
use uuid::Uuid;

async fn open_source(tmp: &TempDir, user_id: &str) -> Replica<SqliteStorage> {
    let storage = SqliteStorage::new(
        &tmp.path().join("users").join(user_id),
        AccessMode::ReadWrite,
        true,
    )
    .await
    .unwrap();
    Replica::new(storage)
}

async fn open_merged(tmp: &TempDir, user_id: &str) -> Replica<SqliteStorage> {
    let storage = SqliteStorage::new(
        &tmp.path()
            .join("users")
            .join(user_id)
            .join("merged/replica"),
        AccessMode::ReadWrite,
        true,
    )
    .await
    .unwrap();
    Replica::new(storage)
}

async fn make_state(tmp: &TempDir) -> (AppState, Arc<dyn ConfigStore>, String, Uuid) {
    let db_path = tmp.path().join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "projector".to_string(),
            password_hash: "hash".to_string(),
        })
        .await
        .unwrap();
    store.set_user_prefix(&user.id, "PERS").await.unwrap();

    let task_uuid = Uuid::new_v4();
    let user_dir = tmp.path().join("users").join(&user.id);
    tokio::fs::create_dir_all(&user_dir).await.unwrap();
    let storage = SqliteStorage::new(&user_dir, AccessMode::ReadWrite, true)
        .await
        .unwrap();
    let mut replica = Replica::new(storage);
    let mut ops = Operations::new();
    let mut task = replica.create_task(task_uuid, &mut ops).await.unwrap();
    task.set_status(Status::Pending, &mut ops).unwrap();
    task.set_description("projected from source".to_string(), &mut ops)
        .unwrap();
    task.set_value("energy", Some("high".to_string()), &mut ops)
        .unwrap();
    task.set_value("start", Some("1777777777".to_string()), &mut ops)
        .unwrap();
    task.set_value("end", Some("1777778888".to_string()), &mut ops)
        .unwrap();
    task.add_annotation(
        Annotation {
            entry: "2026-05-09T08:00:00Z".parse().unwrap(),
            description: "project this annotation".to_string(),
        },
        &mut ops,
    )
    .unwrap();
    replica.commit_operations(ops).await.unwrap();

    let config = common::test_server_config(tmp.path().to_path_buf());
    let state = AppState::new(store.clone(), sqlite, &config);
    (state, store, user.id, task_uuid)
}

#[tokio::test]
async fn project_personal_now_populates_merged_replica_and_chain() {
    let tmp = TempDir::new().unwrap();
    let (state, _store, user_id, task_uuid) = make_state(&tmp).await;

    let summary = project_personal_now(&state, &user_id).await.unwrap();
    assert_eq!(summary.source_tasks, 1);
    assert!(summary.changed);

    let merged_dir = tmp.path().join("users").join(&user_id).join("merged");
    assert!(merged_dir.join("replica/taskchampion.sqlite3").exists());
    assert!(merged_dir.join("sync.sqlite").exists());

    let mut merged = open_merged(&tmp, &user_id).await;
    let projected = merged.get_task(task_uuid).await.unwrap().unwrap();
    assert_eq!(projected.get_description(), "projected from source");
    assert_eq!(projected.get_value("cmdock_task_scope"), Some("PERS"));
    assert_eq!(projected.get_value("cmdock_key"), Some("PERS-1"));
    assert_eq!(projected.get_value("energy"), Some("high"));
    assert_eq!(projected.get_value("start"), Some("1777777777"));
    assert_eq!(projected.get_value("end"), Some("1777778888"));
    let annotations = projected.get_annotations().collect::<Vec<_>>();
    assert_eq!(annotations.len(), 1);
    assert_eq!(annotations[0].description, "project this annotation");
}

#[tokio::test]
async fn project_personal_now_updates_and_removes_annotations() {
    let tmp = TempDir::new().unwrap();
    let (state, _store, user_id, task_uuid) = make_state(&tmp).await;
    project_personal_now(&state, &user_id).await.unwrap();

    let entry = "2026-05-09T08:00:00Z".parse().unwrap();
    let mut source = open_source(&tmp, &user_id).await;
    let mut task = source.get_task(task_uuid).await.unwrap().unwrap();
    let mut ops = Operations::new();
    task.add_annotation(
        Annotation {
            entry,
            description: "updated annotation".to_string(),
        },
        &mut ops,
    )
    .unwrap();
    source.commit_operations(ops).await.unwrap();

    project_personal_now(&state, &user_id).await.unwrap();
    let mut merged = open_merged(&tmp, &user_id).await;
    let projected = merged.get_task(task_uuid).await.unwrap().unwrap();
    let annotations = projected.get_annotations().collect::<Vec<_>>();
    assert_eq!(annotations.len(), 1);
    assert_eq!(annotations[0].description, "updated annotation");

    let mut source = open_source(&tmp, &user_id).await;
    let mut task = source.get_task(task_uuid).await.unwrap().unwrap();
    let mut ops = Operations::new();
    task.remove_annotation(entry, &mut ops).unwrap();
    source.commit_operations(ops).await.unwrap();

    project_personal_now(&state, &user_id).await.unwrap();
    let mut merged = open_merged(&tmp, &user_id).await;
    let projected = merged.get_task(task_uuid).await.unwrap().unwrap();
    assert_eq!(projected.get_annotations().count(), 0);
}
