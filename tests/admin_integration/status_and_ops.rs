//! Admin status, stats, and operator maintenance integration tests.

use super::support::*;
use serde_json::Value;
use uuid::Uuid;

#[tokio::test]
async fn test_admin_status_requires_auth() {
    let env = setup().await;

    // No auth header → 401
    let resp = env.server.get("/admin/status").await;
    resp.assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_admin_status_returns_expected_fields() {
    let env = setup().await;

    let (h, v) = auth_header(&env.admin_token);
    let resp = env.server.get("/admin/status").add_header(h, v).await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    assert_eq!(body["status"], "ok", "status field should be 'ok'");
    assert!(
        body["uptime_seconds"].is_f64() || body["uptime_seconds"].is_u64(),
        "uptime_seconds should be a number, got: {}",
        body["uptime_seconds"]
    );
    assert!(
        body["cached_replicas"].is_u64() || body["cached_replicas"].is_number(),
        "cached_replicas should be a number, got: {}",
        body["cached_replicas"]
    );
    assert!(
        body["quarantined_users"].is_u64() || body["quarantined_users"].is_number(),
        "quarantined_users should be a number, got: {}",
        body["quarantined_users"]
    );
}

#[tokio::test]
async fn test_admin_status_reports_quarantined_user_count() {
    let env = setup().await;
    let (admin_h, admin_v) = auth_header(&env.admin_token);

    env.server
        .post(&format!("/admin/user/{}/offline", env.user_id))
        .add_header(admin_h.clone(), admin_v.clone())
        .await
        .assert_status_ok();

    let resp = env
        .server
        .get("/admin/status")
        .add_header(admin_h, admin_v)
        .await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    assert_eq!(body["quarantined_users"], 1);
}

#[tokio::test]
async fn test_admin_status_includes_startup_recovery_summary() {
    let env = setup_with_startup_recovery_issue().await;
    let (admin_h, admin_v) = auth_header(&env.admin_token);

    let resp = env
        .server
        .get("/admin/status")
        .add_header(admin_h, admin_v)
        .await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    assert_eq!(body["quarantined_users"], 1);
    assert_eq!(body["startup_recovery"]["total_users"], 1);
    assert_eq!(
        body["startup_recovery"]["needs_operator_attention_users"],
        1
    );
    assert_eq!(
        body["startup_recovery"]["newly_offlined_users"][0],
        env.user_id
    );
}

#[tokio::test]
async fn test_admin_status_rejects_normal_user_bearer_token() {
    let env = setup().await;

    let (h, v) = auth_header(&env.token);
    let resp = env.server.get("/admin/status").add_header(h, v).await;
    resp.assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_admin_list_users_returns_user_summaries() {
    let env = setup().await;

    env.store
        .create_device(
            &env.user_id,
            "11111111-1111-1111-1111-111111111111",
            "Admin Listed Device",
            None,
        )
        .await
        .unwrap();
    env.store
        .touch_device("11111111-1111-1111-1111-111111111111", "203.0.113.9")
        .await
        .unwrap();

    let (h, v) = auth_header(&env.admin_token);
    let resp = env.server.get("/admin/users").add_header(h, v).await;

    resp.assert_status_ok();
    let body: Value = resp.json();
    let users = body.as_array().unwrap();
    let listed = users
        .iter()
        .find(|entry| entry["id"] == env.user_id)
        .expect("expected seeded user in admin list");

    assert_eq!(listed["username"], "admin_test_user");
    assert_eq!(listed["deviceCount"], 1);
    assert_rfc3339_timestamp(&listed["createdAt"]);
    assert_rfc3339_timestamp(&listed["lastSyncAt"]);
}

#[tokio::test]
async fn test_delete_admin_user_cascades_and_removes_replica_dir() {
    let env = setup().await;

    env.store
        .create_device(
            &env.user_id,
            "22222222-2222-2222-2222-222222222222",
            "Delete Me Device",
            None,
        )
        .await
        .unwrap();
    env.store
        .create_api_token(&env.user_id, Some("second"))
        .await
        .unwrap();
    let replica_dir = env._tmp.path().join("users").join(&env.user_id);
    std::fs::write(replica_dir.join("taskchampion.sqlite3"), b"placeholder").unwrap();

    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .delete(&format!("/admin/user/{}", env.user_id))
        .add_header(h, v)
        .await;

    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["userId"], env.user_id);
    assert_eq!(body["username"], "admin_test_user");
    assert_eq!(body["deviceCountRemoved"], 1);
    assert_eq!(body["replicaDirRemoved"], true);

    assert!(env
        .store
        .get_user_by_id(&env.user_id)
        .await
        .unwrap()
        .is_none());
    assert!(env
        .store
        .list_devices(&env.user_id)
        .await
        .unwrap()
        .is_empty());
    assert!(!replica_dir.exists());
}

#[tokio::test]
async fn test_checkpoint_no_replica_returns_success_false() {
    let env = setup().await;

    // User directory exists but no taskchampion.sqlite3 yet
    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .post(&format!("/admin/user/{}/checkpoint", env.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    assert_eq!(
        body["success"], false,
        "checkpoint should fail when no replica exists"
    );
    assert!(
        body["message"].as_str().unwrap().contains("not found"),
        "message should mention replica not found, got: {}",
        body["message"]
    );
}

#[tokio::test]
async fn test_checkpoint_with_warm_replica_returns_success_true() {
    let env = setup().await;

    // Create a task to warm the replica (creates taskchampion.sqlite3)
    let (h, v) = auth_header(&env.token);
    let resp = env
        .server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+test Checkpoint task"}))
        .await;
    resp.assert_status_ok();

    // Now checkpoint should succeed
    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .post(&format!("/admin/user/{}/checkpoint", env.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    assert_eq!(
        body["success"], true,
        "checkpoint should succeed after task creation"
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("checkpoint completed"),
        "message should confirm completion, got: {}",
        body["message"]
    );
}

#[tokio::test]
async fn test_user_stats_invalid_integrity_mode_returns_400() {
    let env = setup().await;

    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .get(&format!(
            "/admin/user/{}/stats?integrity=bogus",
            env.user_id
        ))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_user_stats_full_integrity_returns_integrity_check() {
    let env = setup().await;

    // Create a task so the replica DB exists
    let (h, v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+test Integrity full task"}))
        .await
        .assert_status_ok();

    // Request stats with full integrity check
    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .get(&format!("/admin/user/{}/stats?integrity=full", env.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    assert!(
        body["integrity_check"].is_object(),
        "integrity_check should be present when integrity=full"
    );
    let ic = &body["integrity_check"];
    assert_eq!(
        ic["replica"].as_str(),
        Some("ok"),
        "Full integrity check on healthy DB should report 'ok', got: {}",
        ic["replica"]
    );
}

#[tokio::test]
async fn test_user_stats_reports_rebuildable_missing_shared_sync_db() {
    let env = setup().await;

    let db_path = env._tmp.path().join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let client_id = uuid::Uuid::new_v4().to_string();
    store
        .create_replica(
            &env.user_id,
            &uuid::Uuid::new_v4().to_string(),
            "test-secret",
        )
        .await
        .unwrap();
    store
        .create_device(&env.user_id, &client_id, "Assessment device", Some("enc"))
        .await
        .unwrap();

    let (h, v) = auth_header(&env.token);
    env.server
        .post("/api/tasks")
        .add_header(h, v)
        .json(&serde_json::json!({"raw": "+test Recovery assessment task"}))
        .await
        .assert_status_ok();

    let user_dir = env._tmp.path().join("users").join(&env.user_id);
    let sync_db = user_dir.join("sync.sqlite");
    std::fs::remove_file(&sync_db).ok();

    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .get(&format!("/admin/user/{}/stats", env.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    assert_eq!(
        body["recovery_assessment"]["status"].as_str(),
        Some("rebuildable")
    );
    assert_eq!(
        body["recovery_assessment"]["shared_sync_db_exists"],
        serde_json::json!(false)
    );
}

#[tokio::test]
async fn test_path_traversal_rejected() {
    let env = setup().await;

    // The validate_user_id function rejects user IDs containing ".."
    // We encode the path traversal in the user_id segment.
    // "..%5Cetc%5Cpasswd" decodes to "..\etc\passwd" — contains ".." and "\"
    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .post("/admin/user/..%5Cetc%5Cpasswd/evict")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

    // Double-dot embedded in otherwise-valid segment: foo..bar
    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .post("/admin/user/foo..bar/evict")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

    // Backslash traversal: foo\bar
    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .post("/admin/user/foo%5Cbar/evict")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

    // Bare double-dot user ID
    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .post("/admin/user/..secret/evict")
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_evict_non_cached_user_returns_not_in_cache() {
    let env = setup().await;

    // Evict without ever warming the cache — should still return success
    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .post(&format!("/admin/user/{}/evict", env.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    assert_eq!(
        body["success"], true,
        "evict on non-cached user should still succeed"
    );
    assert!(
        body["message"].as_str().unwrap().contains("not in cache"),
        "message should mention 'not in cache', got: {}",
        body["message"]
    );
}

#[tokio::test]
async fn test_admin_user_offline_and_online_404_for_unknown_user() {
    let env = setup().await;
    let missing_user_id = Uuid::new_v4().to_string();
    let (h, v) = auth_header(&env.admin_token);

    env.server
        .post(&format!("/admin/user/{missing_user_id}/offline"))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status(axum::http::StatusCode::NOT_FOUND);

    env.server
        .post(&format!("/admin/user/{missing_user_id}/online"))
        .add_header(h, v)
        .await
        .assert_status(axum::http::StatusCode::NOT_FOUND);
}
