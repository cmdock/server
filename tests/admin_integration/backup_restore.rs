//! Admin backup and restore integration tests.

use super::support::*;
use cmdock_server::runtime_policy::{RuntimeAccessMode, RuntimeDeleteAction, RuntimePolicy};
use cmdock_server::store::models::NewUser;
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn test_admin_backup_create_and_list_ignore_incomplete_snapshot() {
    let env = setup().await;

    let (user_h, user_v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(user_h, user_v)
        .json(&serde_json::json!({"raw": "+test Backup manifest task"}))
        .await
        .assert_status_ok();

    std::fs::create_dir_all(backup_root(&env).join("incomplete-snapshot")).unwrap();

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let create = env
        .server
        .post("/admin/backup?include_secrets=true")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let created: Value = create.json();
    let timestamp = created["timestamp"].as_str().unwrap();
    assert_eq!(created["users"], 1);
    assert_eq!(created["secretsIncluded"], true);
    assert!(
        backup_manifest_path(&env, timestamp).exists(),
        "backup should write manifest.json last"
    );

    let manifest: Value =
        serde_json::from_slice(&std::fs::read(backup_manifest_path(&env, timestamp)).unwrap())
            .unwrap();
    assert_eq!(manifest["backup_type"], "full");
    assert_eq!(manifest["secrets_included"], true);
    assert_eq!(manifest["secrets"]["admin_token"], env.admin_token);

    let list = env
        .server
        .get("/admin/backup/list")
        .add_header(admin_h, admin_v)
        .await;
    list.assert_status_ok();
    let listed: Value = list.json();
    let backups = listed["backups"].as_array().unwrap();
    assert_eq!(backups.len(), 1, "incomplete snapshot should be ignored");
    assert_eq!(backups[0]["timestamp"], timestamp);
    assert_eq!(backups[0]["backupType"], "full");
    assert_eq!(backups[0]["taskCount"], 1);
}

#[tokio::test]
async fn test_admin_backup_restore_without_secrets_preserves_current_operator_token() {
    let env = setup().await;

    let (user_h, user_v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(user_h, user_v)
        .json(&serde_json::json!({"raw": "+test Restore without secrets"}))
        .await
        .assert_status_ok();

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let create = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let created: Value = create.json();
    let timestamp = created["timestamp"].as_str().unwrap().to_string();

    let later_user = env
        .store
        .create_user(&NewUser {
            username: "post-backup-user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    restore.assert_status_ok();
    let restored: Value = restore.json();
    assert_eq!(restored["restoredFrom"], timestamp);
    assert_eq!(restored["secretsRestored"], false);
    assert_eq!(restored["configDatabaseRestored"], true);

    assert!(
        env.store
            .get_user_by_id(&later_user.id)
            .await
            .unwrap()
            .is_none(),
        "restore should replace config state with the snapshot contents"
    );

    env.server
        .get("/admin/status")
        .add_header(admin_h, admin_v)
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn test_admin_backup_restore_accepts_older_compatible_manifest() {
    let env = setup().await;

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let create = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let created: Value = create.json();
    let timestamp = created["timestamp"].as_str().unwrap().to_string();

    let manifest_path = backup_manifest_path(&env, &timestamp);
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["minimum_server_version"] = Value::String("0.0.1".to_string());
    if let Some(schema_version) = manifest["schema_version"].as_i64() {
        manifest["schema_version"] = Value::Number((schema_version - 1).max(0).into());
    }
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    restore.assert_status_ok();
    let restored: Value = restore.json();
    assert_eq!(restored["restoredFrom"], timestamp);
    assert_eq!(restored["configDatabaseRestored"], true);

    env.server
        .get("/admin/status")
        .add_header(admin_h, admin_v)
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn test_admin_backup_restore_tc_sync_updates_admin_last_sync() {
    let env = setup_bootstrap().await;

    let client_id = "33333333-3333-3333-3333-333333333333";
    env.store
        .create_device(&env.user_id, client_id, "Restore Sync Device", None)
        .await
        .unwrap();

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let users_before = env
        .server
        .get("/admin/users")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    users_before.assert_status_ok();
    let listed_before: Value = users_before.json();
    let user_before = listed_before
        .as_array()
        .unwrap()
        .iter()
        .find(|user| user["id"] == env.user_id)
        .unwrap();
    assert!(user_before["lastSyncAt"].is_null());

    let create = env
        .server
        .post("/admin/backup?include_secrets=true")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let timestamp = create.json::<Value>()["timestamp"]
        .as_str()
        .unwrap()
        .to_string();

    let restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    restore.assert_status_ok();

    let sync = env
        .server
        .get(&format!(
            "/v1/client/get-child-version/{}",
            uuid::Uuid::nil()
        ))
        .add_header(header::HeaderName::from_static("x-client-id"), client_id)
        .await;
    let status = sync.status_code();
    assert!(
        status == axum::http::StatusCode::OK || status == axum::http::StatusCode::NOT_FOUND,
        "expected TC sync read to authenticate after restore, got {status}"
    );

    let users_after = env
        .server
        .get("/admin/users")
        .add_header(admin_h, admin_v)
        .await;
    users_after.assert_status_ok();
    let listed_after: Value = users_after.json();
    let user_after = listed_after
        .as_array()
        .unwrap()
        .iter()
        .find(|user| user["id"] == env.user_id)
        .unwrap();
    assert_rfc3339_timestamp(&user_after["lastSyncAt"]);
}

#[tokio::test]
async fn test_admin_backup_restore_empty_instance_removes_later_state() {
    let env = setup_empty().await;

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let create = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let created: Value = create.json();
    let timestamp = created["timestamp"].as_str().unwrap().to_string();
    assert_eq!(created["users"], 0);

    env.store
        .create_user(&NewUser {
            username: "later-user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    restore.assert_status_ok();
    let restored: Value = restore.json();
    assert_eq!(restored["restoredFrom"], timestamp);
    assert_eq!(restored["usersRestored"], 0);
    assert_eq!(restored["replicasRestored"], 0);
    assert_eq!(restored["secretsRestored"], false);
    assert_eq!(restored["configDatabaseRestored"], true);
    assert_eq!(restored["replicas"].as_array().unwrap().len(), 0);

    let users = env
        .server
        .get("/admin/users")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    users.assert_status_ok();
    assert!(users.json::<Value>().as_array().unwrap().is_empty());

    let status = env
        .server
        .get("/admin/status")
        .add_header(admin_h, admin_v)
        .await;
    status.assert_status_ok();
    let body: Value = status.json();
    assert_eq!(body["quarantined_users"], 0);
}

#[tokio::test]
async fn test_admin_backup_retention_prunes_oldest_snapshot_and_keeps_retained_restoreable() {
    let env = setup_with_backup_retention(2).await;

    let (admin_h, admin_v) = auth_header(&env.admin_token);

    let first = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    first.assert_status(axum::http::StatusCode::CREATED);
    let first_timestamp = first.json::<Value>()["timestamp"]
        .as_str()
        .unwrap()
        .to_string();

    tokio::time::sleep(Duration::from_secs(1)).await;
    let retained_user = env
        .store
        .create_user(&NewUser {
            username: "retained-user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let second = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    second.assert_status(axum::http::StatusCode::CREATED);
    let second_timestamp = second.json::<Value>()["timestamp"]
        .as_str()
        .unwrap()
        .to_string();

    tokio::time::sleep(Duration::from_secs(1)).await;
    let later_user = env
        .store
        .create_user(&NewUser {
            username: "later-user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let third = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    third.assert_status(axum::http::StatusCode::CREATED);
    let third_timestamp = third.json::<Value>()["timestamp"]
        .as_str()
        .unwrap()
        .to_string();

    let list = env
        .server
        .get("/admin/backup/list")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    list.assert_status_ok();
    let backups = list.json::<Value>()["backups"].as_array().unwrap().clone();
    assert_eq!(backups.len(), 2);
    assert!(
        backups
            .iter()
            .any(|entry| entry["timestamp"] == second_timestamp),
        "retained snapshot should remain listed"
    );
    assert!(
        backups
            .iter()
            .any(|entry| entry["timestamp"] == third_timestamp),
        "newest snapshot should remain listed"
    );
    assert!(
        backups
            .iter()
            .all(|entry| entry["timestamp"] != first_timestamp),
        "oldest snapshot should be pruned by retention"
    );

    let restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": second_timestamp }))
        .await;
    restore.assert_status_ok();

    assert!(
        env.store
            .get_user_by_id(&retained_user.id)
            .await
            .unwrap()
            .is_some(),
        "retained snapshot should still restore its saved state"
    );
    assert!(
        env.store
            .get_user_by_id(&later_user.id)
            .await
            .unwrap()
            .is_none(),
        "restoring the retained snapshot should remove later state"
    );
}

#[tokio::test]
async fn test_admin_backup_restore_observability_matches_restored_state() {
    let env = setup_bootstrap().await;

    env.store
        .create_device(
            &env.user_id,
            "11111111-1111-1111-1111-111111111111",
            "Primary Device",
            None,
        )
        .await
        .unwrap();
    env.store
        .touch_device("11111111-1111-1111-1111-111111111111", "203.0.113.9")
        .await
        .unwrap();

    let second_user = env
        .store
        .create_user(&NewUser {
            username: "observed-user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();
    let second_token = env
        .store
        .create_api_token(&second_user.id, Some("observed"))
        .await
        .unwrap();
    std::fs::create_dir_all(env._tmp.path().join("users").join(&second_user.id)).unwrap();
    env.store
        .create_device(
            &second_user.id,
            "22222222-2222-2222-2222-222222222222",
            "Observed Device",
            None,
        )
        .await
        .unwrap();
    env.store
        .touch_device("22222222-2222-2222-2222-222222222222", "203.0.113.10")
        .await
        .unwrap();

    let (user_h, user_v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(user_h, user_v)
        .json(&serde_json::json!({"raw": "+test Observability user a"}))
        .await
        .assert_status_ok();

    let (second_h, second_v) = auth_header(&second_token);
    env.server
        .post("/api/tasks")
        .add_header(second_h.clone(), second_v.clone())
        .json(&serde_json::json!({"raw": "+test Observability user b"}))
        .await
        .assert_status_ok();

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let create = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let timestamp = create.json::<Value>()["timestamp"]
        .as_str()
        .unwrap()
        .to_string();

    env.store
        .create_user(&NewUser {
            username: "post-backup-user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    restore.assert_status_ok();
    let restored: Value = restore.json();
    assert_eq!(restored["usersRestored"], 2);
    assert_eq!(restored["replicasRestored"], 2);

    let restore_replicas = restored["replicas"].as_array().unwrap();
    let mut expected_task_counts = std::collections::HashMap::new();
    for replica in restore_replicas {
        expected_task_counts.insert(
            replica["userId"].as_str().unwrap().to_string(),
            replica["taskCount"].as_u64().unwrap_or(0) as usize,
        );
    }

    let (first_h, first_v) = auth_header(&env.token);
    env.server
        .get("/api/tasks")
        .add_header(second_h, second_v)
        .await
        .assert_status_ok();
    env.server
        .get("/api/tasks")
        .add_header(first_h, first_v)
        .await
        .assert_status_ok();

    let users = env
        .server
        .get("/admin/users")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    users.assert_status_ok();
    let users_body = users.json::<Value>();
    let listed_users = users_body.as_array().unwrap();
    assert_eq!(listed_users.len(), 2);

    for listed in listed_users {
        assert_eq!(listed["deviceCount"], 1);
        assert!(
            !listed["lastSyncAt"].is_null(),
            "restored users with touched devices should report lastSyncAt"
        );

        let user_id = listed["id"].as_str().unwrap();
        let stats = env
            .server
            .get(&format!("/admin/user/{user_id}/stats"))
            .add_header(admin_h.clone(), admin_v.clone())
            .await;
        stats.assert_status_ok();
        let stats_body: Value = stats.json();
        assert_eq!(stats_body["user_id"], user_id);
        assert_eq!(stats_body["replica_cached"], true);
        assert_eq!(stats_body["replica_dir_exists"], true);
        assert_eq!(
            stats_body["task_count"].as_u64().unwrap_or(0) as usize,
            *expected_task_counts.get(user_id).unwrap()
        );
    }

    let status = env
        .server
        .get("/admin/status")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    status.assert_status_ok();
    let status_body: Value = status.json();
    assert_eq!(status_body["status"], "ok");
    assert!(
        status_body["quarantined_users"].is_u64() || status_body["quarantined_users"].is_number(),
        "status should report a numeric quarantined_users count"
    );
}

#[tokio::test]
async fn test_admin_backup_restore_reverts_state_and_rotates_operator_token() {
    let env = setup().await;

    let (user_h, user_v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(user_h, user_v)
        .json(&serde_json::json!({"raw": "+test Restore verification task"}))
        .await
        .assert_status_ok();

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let create = env
        .server
        .post("/admin/backup?include_secrets=true")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let created: Value = create.json();
    let timestamp = created["timestamp"].as_str().unwrap().to_string();

    let later_user = env
        .store
        .create_user(&NewUser {
            username: "post-backup-user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let manifest_path = backup_manifest_path(&env, &timestamp);
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["secrets"]["admin_token"] = Value::String("rotated-admin-token".to_string());
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    restore.assert_status_ok();
    let restored: Value = restore.json();
    assert!(restored["preRestoreSnapshot"]
        .as_str()
        .unwrap()
        .starts_with("pre-restore-"));
    assert_eq!(restored["secretsRestored"], true);
    assert_eq!(restored["configDatabaseRestored"], true);
    let replicas = restored["replicas"].as_array().unwrap();
    assert_eq!(replicas.len(), 1);
    assert_eq!(replicas[0]["userId"], env.user_id);
    assert_eq!(replicas[0]["username"], "admin_test_user");
    assert_eq!(replicas[0]["taskCount"], 1);

    assert!(
        env.store
            .get_user_by_id(&later_user.id)
            .await
            .unwrap()
            .is_none(),
        "restore should replace config state with the snapshot contents"
    );

    env.server
        .get("/admin/status")
        .add_header(admin_h.clone(), admin_v.clone())
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);

    let (new_h, new_v) = auth_header("rotated-admin-token");
    env.server
        .get("/admin/status")
        .add_header(new_h, new_v)
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn test_admin_backup_restore_failure_rolls_back_pre_restore_state() {
    let env = setup().await;

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let create = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let created: Value = create.json();
    let timestamp = created["timestamp"].as_str().unwrap().to_string();

    let later_user = env
        .store
        .create_user(&NewUser {
            username: "rollback-survivor".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let snapshot_dir = backup_root(&env).join(&timestamp);
    let manifest_path = snapshot_dir.join("manifest.json");
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    std::fs::write(snapshot_dir.join("config.sqlite"), b"not-a-sqlite-database").unwrap();
    manifest["contents"]["config_db"]["size_bytes"] =
        Value::Number(serde_json::Number::from(21_u64));
    let digest = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"not-a-sqlite-database");
        format!("{:x}", hasher.finalize())
    };
    manifest["contents"]["config_db"]["sha256"] = Value::String(digest);
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    restore.assert_status(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    let body: Value = restore.json();
    assert_eq!(body["code"], "RESTORE_FAILED_ROLLED_BACK");
    assert!(
        body["message"].as_str().unwrap().contains("rolled back"),
        "restore failure should report rollback behaviour"
    );

    assert!(
        env.store
            .get_user_by_id(&later_user.id)
            .await
            .unwrap()
            .is_some(),
        "rollback should preserve the pre-restore state"
    );

    env.server
        .get("/admin/status")
        .add_header(admin_h, admin_v)
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn test_admin_backup_restore_rejects_newer_minimum_server_version() {
    let env = setup().await;

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let create = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let created: Value = create.json();
    let timestamp = created["timestamp"].as_str().unwrap().to_string();

    let manifest_path = backup_manifest_path(&env, &timestamp);
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["minimum_server_version"] = Value::String("999.0.0".to_string());
    manifest["restore_instructions"]["minimum_server_version"] =
        Value::String("999.0.0".to_string());
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    restore.assert_status(axum::http::StatusCode::CONFLICT);
    let body: Value = restore.json();
    assert_eq!(body["code"], "VERSION_INCOMPATIBLE");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("requires server version"),
        "version mismatch should explain the required server version"
    );

    env.server
        .get("/admin/status")
        .add_header(admin_h, admin_v)
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn test_admin_backup_restore_rejects_newer_schema_version() {
    let env = setup().await;

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let create = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let created: Value = create.json();
    let timestamp = created["timestamp"].as_str().unwrap().to_string();

    let manifest_path = backup_manifest_path(&env, &timestamp);
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["schema_version"] = Value::Number(serde_json::Number::from(9_999_u64));
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    restore.assert_status(axum::http::StatusCode::CONFLICT);
    let body: Value = restore.json();
    assert_eq!(body["code"], "SCHEMA_INCOMPATIBLE");
    assert!(
        body["message"].as_str().unwrap().contains("schema version"),
        "schema mismatch should explain the incompatible schema version"
    );

    env.server
        .get("/admin/status")
        .add_header(admin_h, admin_v)
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn test_admin_backup_restore_preserves_multi_user_multi_device_state() {
    let env = setup_bootstrap().await;

    let second_user = env
        .store
        .create_user(&NewUser {
            username: "second-backup-user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    std::fs::create_dir_all(env._tmp.path().join("users").join(&second_user.id)).unwrap();
    write_minimal_sqlite(
        &env._tmp
            .path()
            .join("users")
            .join(&env.user_id)
            .join("taskchampion.sqlite3"),
    );
    write_minimal_sqlite(
        &env._tmp
            .path()
            .join("users")
            .join(&second_user.id)
            .join("taskchampion.sqlite3"),
    );

    let user_a_device_1 = Uuid::new_v4().to_string();
    let user_a_device_2 = Uuid::new_v4().to_string();
    let user_b_device_1 = Uuid::new_v4().to_string();
    let user_b_device_2 = Uuid::new_v4().to_string();
    env.store
        .create_device(
            &env.user_id,
            &user_a_device_1,
            "User A Laptop",
            Some("enc-a1"),
        )
        .await
        .unwrap();
    env.store
        .create_device(
            &env.user_id,
            &user_a_device_2,
            "User A Phone",
            Some("enc-a2"),
        )
        .await
        .unwrap();
    env.store
        .create_device(
            &second_user.id,
            &user_b_device_1,
            "User B Laptop",
            Some("enc-b1"),
        )
        .await
        .unwrap();
    env.store
        .create_device(
            &second_user.id,
            &user_b_device_2,
            "User B Phone",
            Some("enc-b2"),
        )
        .await
        .unwrap();
    env.store
        .touch_device(&user_a_device_2, "203.0.113.10")
        .await
        .unwrap();
    env.store
        .touch_device(&user_b_device_2, "203.0.113.11")
        .await
        .unwrap();

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let create = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let created: Value = create.json();
    let timestamp = created["timestamp"].as_str().unwrap().to_string();

    let later_user = env
        .store
        .create_user(&NewUser {
            username: "post-backup-user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();
    env.store
        .delete_device(&env.user_id, &user_a_device_1)
        .await
        .unwrap();
    env.store.delete_user(&second_user.id).await.unwrap();

    let restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    restore.assert_status_ok();

    let users = env.store.list_users().await.unwrap();
    assert_eq!(
        users.len(),
        2,
        "restore should replace user set with snapshot"
    );
    assert!(users.iter().any(|user| user.id == env.user_id));
    assert!(users.iter().any(|user| user.id == second_user.id));
    assert!(
        env.store
            .get_user_by_id(&later_user.id)
            .await
            .unwrap()
            .is_none(),
        "post-backup user should be removed by restore"
    );

    assert_eq!(env.store.list_devices(&env.user_id).await.unwrap().len(), 2);
    assert_eq!(
        env.store.list_devices(&second_user.id).await.unwrap().len(),
        2
    );
    assert!(env
        ._tmp
        .path()
        .join("users")
        .join(&env.user_id)
        .join("taskchampion.sqlite3")
        .exists());
    assert!(env
        ._tmp
        .path()
        .join("users")
        .join(&second_user.id)
        .join("taskchampion.sqlite3")
        .exists());

    let listed = env
        .server
        .get("/admin/users")
        .add_header(admin_h, admin_v)
        .await;
    listed.assert_status_ok();
    let body: Value = listed.json();
    let listed_users = body.as_array().unwrap();
    assert_eq!(listed_users.len(), 2);
    assert!(listed_users.iter().any(|user| {
        user["id"] == env.user_id && user["deviceCount"] == 2 && !user["lastSyncAt"].is_null()
    }));
    assert!(listed_users.iter().any(|user| {
        user["id"] == second_user.id && user["deviceCount"] == 2 && !user["lastSyncAt"].is_null()
    }));
}

#[tokio::test]
async fn test_admin_backup_restore_restores_runtime_policy_state() {
    let env = setup_bootstrap().await;
    let blocked = RuntimePolicy {
        runtime_access: RuntimeAccessMode::Block,
        delete_action: RuntimeDeleteAction::Forbid,
    };
    let allowed = RuntimePolicy {
        runtime_access: RuntimeAccessMode::Allow,
        delete_action: RuntimeDeleteAction::Allow,
    };

    env.store
        .upsert_runtime_policy(
            &env.user_id,
            "blocked-v1",
            &blocked,
            Some("blocked-v1"),
            Some(&blocked),
            Some("2026-04-03 12:00:00"),
        )
        .await
        .unwrap();

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let create = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let created: Value = create.json();
    let timestamp = created["timestamp"].as_str().unwrap().to_string();

    env.store
        .upsert_runtime_policy(
            &env.user_id,
            "allowed-v2",
            &allowed,
            Some("allowed-v2"),
            Some(&allowed),
            Some("2026-04-04 12:00:00"),
        )
        .await
        .unwrap();

    let restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    restore.assert_status_ok();

    let readback = env
        .server
        .get(&format!("/admin/user/{}/runtime-policy", env.user_id))
        .add_header(admin_h, admin_v)
        .await;
    readback.assert_status_ok();
    let body: Value = readback.json();
    assert_eq!(body["desiredVersion"], "blocked-v1");
    assert_eq!(body["appliedVersion"], "blocked-v1");
    assert_eq!(body["desiredPolicy"]["runtimeAccess"], "block");
    assert_eq!(body["desiredPolicy"]["deleteAction"], "forbid");
    assert_eq!(body["appliedPolicy"]["runtimeAccess"], "block");
    assert_eq!(body["appliedPolicy"]["deleteAction"], "forbid");
    assert_eq!(body["enforcementState"], "current");
}

#[tokio::test]
async fn test_admin_backup_restore_repeated_restore_creates_distinct_pre_restore_snapshots() {
    let env = setup().await;

    let (admin_h, admin_v) = auth_header(&env.admin_token);
    let create = env
        .server
        .post("/admin/backup")
        .add_header(admin_h.clone(), admin_v.clone())
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let created: Value = create.json();
    let timestamp = created["timestamp"].as_str().unwrap().to_string();

    let first_restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    first_restore.assert_status_ok();
    let first_body: Value = first_restore.json();
    let first_pre_restore = first_body["preRestoreSnapshot"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(first_pre_restore.starts_with("pre-restore-"));

    tokio::time::sleep(Duration::from_secs(1)).await;

    let second_restore = env
        .server
        .post("/admin/backup/restore")
        .add_header(admin_h.clone(), admin_v.clone())
        .json(&serde_json::json!({ "timestamp": timestamp }))
        .await;
    second_restore.assert_status_ok();
    let second_body: Value = second_restore.json();
    let second_pre_restore = second_body["preRestoreSnapshot"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(second_pre_restore.starts_with("pre-restore-"));
    assert_ne!(first_pre_restore, second_pre_restore);

    let list = env
        .server
        .get("/admin/backup/list")
        .add_header(admin_h, admin_v)
        .await;
    list.assert_status_ok();
    let listed: Value = list.json();
    let backups = listed["backups"].as_array().unwrap();
    assert!(backups.iter().any(|entry| {
        entry["timestamp"] == first_pre_restore && entry["backupType"] == "pre_restore"
    }));
    assert!(backups.iter().any(|entry| {
        entry["timestamp"] == second_pre_restore && entry["backupType"] == "pre_restore"
    }));
}
