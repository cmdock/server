//! Admin bootstrap, connect-config, sync-identity, and device lifecycle integration tests.

use super::support::*;
use serde_json::Value;
use uuid::Uuid;

#[tokio::test]
async fn test_create_connect_config_requires_auth() {
    let env = setup_connect_config().await;

    env.server
        .post(&format!("/admin/user/{}/connect-config", env.user_id))
        .json(&serde_json::json!({}))
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_create_connect_config_returns_short_lived_credential_tuple() {
    let env = setup_connect_config().await;
    let (h, v) = auth_header(&env.admin_token);

    let resp = env
        .server
        .post(&format!("/admin/user/{}/connect-config", env.user_id))
        .add_header(h, v)
        .json(&serde_json::json!({
            "name": "Simon's iPhone"
        }))
        .await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    let credential = body["credential"].as_str().unwrap();
    let token_id = body["tokenId"].as_str().unwrap();
    let server_url = body["serverUrl"].as_str().unwrap();

    assert!(!credential.is_empty(), "credential should be present");
    assert!(token_id.starts_with("cc_"), "tokenId should use cc_ prefix");
    assert!(
        token_id.len() <= 20,
        "tokenId should respect the contract length budget"
    );
    assert_eq!(server_url, "https://tasks.example.com");

    let lookup = env
        .store
        .lookup_connect_config_token(credential)
        .await
        .unwrap()
        .expect("credential should exist in api_tokens");
    assert_eq!(lookup.user_id, env.user_id);
    assert_eq!(lookup.token_id, token_id);
    assert!(!lookup.is_expired);
}

#[tokio::test]
async fn test_create_connect_config_requires_public_base_url() {
    let mut env = setup().await;
    let mut config =
        common::test_server_config_with_admin_token(env._tmp.path().to_path_buf(), ADMIN_TOKEN);
    config.server.public_base_url = None;

    let state = AppState::new(env.store.clone(), &config);
    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(admin::routes())
        .with_state(state.clone())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .merge(tc_sync::routes().with_state(state))
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );
    env.server = TestServer::new(app);

    let (h, v) = auth_header(&env.admin_token);

    let resp = env
        .server
        .post(&format!("/admin/user/{}/connect-config", env.user_id))
        .add_header(h, v)
        .json(&serde_json::json!({}))
        .await;
    resp.assert_status(axum::http::StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn test_bootstrap_user_device_creates_user_and_replays_same_request() {
    let env = setup_bootstrap().await;
    let bootstrap_request_id = Uuid::new_v4().to_string();
    let (h, v) = auth_header(&env.admin_token);

    let payload = serde_json::json!({
        "username": "bootstrap-user",
        "createUserIfMissing": true,
        "deviceName": "Bootstrap MacBook",
        "bootstrapRequestId": bootstrap_request_id,
    });

    let resp = env
        .server
        .post("/admin/bootstrap/user-device")
        .add_header(h.clone(), v.clone())
        .json(&payload)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["username"], "bootstrap-user");
    assert_eq!(body["serverUrl"], "https://test.invalid");
    assert_eq!(body["bootstrapStatus"], "pending_delivery");
    assert_eq!(body["createdUser"], true);
    let bootstrapped_user_id = body["userId"].as_str().unwrap().to_string();
    let device_client_id = body["deviceClientId"].as_str().unwrap().to_string();
    let encryption_secret = body["encryptionSecret"].as_str().unwrap().to_string();

    let replay = env
        .server
        .post("/admin/bootstrap/user-device")
        .add_header(h, v)
        .json(&payload)
        .await;
    replay.assert_status_ok();
    let replay_body: Value = replay.json();
    assert_eq!(replay_body["deviceClientId"], device_client_id);
    assert_eq!(replay_body["encryptionSecret"], encryption_secret);
    assert_eq!(replay_body["createdUser"], false);

    let device = env
        .store
        .get_device(&device_client_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        device.bootstrap_request_id.as_deref(),
        Some(bootstrap_request_id.as_str())
    );
    assert_eq!(device.bootstrap_status.as_deref(), Some("pending_delivery"));

    let (show_h, show_v) = auth_header(&env.admin_token);
    let show = env
        .server
        .get(&format!(
            "/admin/user/{bootstrapped_user_id}/devices/{device_client_id}"
        ))
        .add_header(show_h, show_v)
        .await;
    show.assert_status_ok();
    let show_body: Value = show.json();
    assert_rfc3339_timestamp(&show_body["registeredAt"]);
    assert_rfc3339_timestamp(&show_body["bootstrapExpiresAt"]);
}

#[tokio::test]
async fn test_bootstrap_user_device_rejects_conflicting_retry_payload() {
    let env = setup_bootstrap().await;
    let bootstrap_request_id = Uuid::new_v4().to_string();
    let (h, v) = auth_header(&env.admin_token);

    env.server
        .post("/admin/bootstrap/user-device")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "username": "bootstrap-conflict-user",
            "createUserIfMissing": true,
            "deviceName": "Bootstrap MacBook",
            "bootstrapRequestId": bootstrap_request_id,
        }))
        .await
        .assert_status_ok();

    env.server
        .post("/admin/bootstrap/user-device")
        .add_header(h, v)
        .json(&serde_json::json!({
            "username": "bootstrap-conflict-user",
            "createUserIfMissing": true,
            "deviceName": "Other Device",
            "bootstrapRequestId": bootstrap_request_id,
        }))
        .await
        .assert_status(axum::http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_bootstrap_acknowledge_marks_request_acknowledged() {
    let env = setup_bootstrap().await;
    let bootstrap_request_id = Uuid::new_v4().to_string();
    let (h, v) = auth_header(&env.admin_token);

    let resp = env
        .server
        .post("/admin/bootstrap/user-device")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "username": "bootstrap-ack-user",
            "createUserIfMissing": true,
            "deviceName": "Bootstrap iPhone",
            "bootstrapRequestId": bootstrap_request_id,
        }))
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let device_client_id = body["deviceClientId"].as_str().unwrap().to_string();

    env.server
        .post(&format!("/admin/bootstrap/{bootstrap_request_id}/ack"))
        .add_header(h, v)
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    let device = env
        .store
        .get_device(&device_client_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(device.bootstrap_status.as_deref(), Some("acknowledged"));
}

#[tokio::test]
async fn test_operator_sync_identity_ensure_and_get() {
    let env = setup_bootstrap().await;
    let (h, v) = auth_header(&env.admin_token);

    env.server
        .get(&format!("/admin/user/{}/sync-identity", env.user_id))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status(axum::http::StatusCode::NOT_FOUND);

    let resp = env
        .server
        .post(&format!("/admin/user/{}/sync-identity/ensure", env.user_id))
        .add_header(h.clone(), v.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["userId"], env.user_id);
    assert_eq!(body["created"], true);
    assert_rfc3339_timestamp(&body["createdAt"]);
    let client_id = body["clientId"].as_str().unwrap().to_string();

    let replay = env
        .server
        .post(&format!("/admin/user/{}/sync-identity/ensure", env.user_id))
        .add_header(h.clone(), v.clone())
        .await;
    replay.assert_status_ok();
    let replay_body: Value = replay.json();
    assert_eq!(replay_body["clientId"], client_id);
    assert_eq!(replay_body["created"], false);

    let get_resp = env
        .server
        .get(&format!("/admin/user/{}/sync-identity", env.user_id))
        .add_header(h, v)
        .await;
    get_resp.assert_status_ok();
    let get_body: Value = get_resp.json();
    assert_eq!(get_body["clientId"], client_id);
    assert_rfc3339_timestamp(&get_body["createdAt"]);
}

#[tokio::test]
async fn test_operator_sync_identity_ensure_requires_master_key() {
    let env = setup().await;
    let (h, v) = auth_header(&env.admin_token);

    let resp = env
        .server
        .post(&format!("/admin/user/{}/sync-identity/ensure", env.user_id))
        .add_header(h, v)
        .await;
    resp.assert_status(axum::http::StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn test_operator_device_create_requires_canonical_sync_identity() {
    let env = setup_bootstrap().await;
    let (h, v) = auth_header(&env.admin_token);

    env.server
        .post(&format!("/admin/user/{}/devices", env.user_id))
        .add_header(h, v)
        .json(&serde_json::json!({
            "name": "Operator-created device",
        }))
        .await
        .assert_status(axum::http::StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn test_operator_device_create_blocked_by_runtime_policy() {
    let env = setup_bootstrap().await;
    let policy = RuntimePolicy {
        runtime_access: RuntimeAccessMode::Block,
        delete_action: RuntimeDeleteAction::Allow,
    };
    env.store
        .upsert_runtime_policy(
            &env.user_id,
            "policy-v1",
            &policy,
            Some("policy-v1"),
            Some(&policy),
            Some("2026-04-03 12:00:00"),
        )
        .await
        .unwrap();

    let (h, v) = auth_header(&env.admin_token);
    env.server
        .post(&format!("/admin/user/{}/sync-identity/ensure", env.user_id))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status_ok();

    let resp = env
        .server
        .post(&format!("/admin/user/{}/devices", env.user_id))
        .add_header(h, v)
        .json(&serde_json::json!({
            "name": "Blocked Operator Device",
        }))
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
    assert!(resp.text().contains("Runtime access blocked by policy"));
}

#[tokio::test]
async fn test_operator_device_lifecycle_round_trip() {
    let env = setup_bootstrap().await;
    let (h, v) = auth_header(&env.admin_token);

    env.server
        .post(&format!("/admin/user/{}/sync-identity/ensure", env.user_id))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status_ok();

    let create = env
        .server
        .post(&format!("/admin/user/{}/devices", env.user_id))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "name": "Operator MacBook",
        }))
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let create_body: Value = create.json();
    assert_eq!(create_body["userId"], env.user_id);
    assert_eq!(create_body["name"], "Operator MacBook");
    assert_eq!(
        create_body["taskrcLines"][0].as_str(),
        Some("sync.server.url=https://test.invalid")
    );
    let client_id = create_body["clientId"].as_str().unwrap().to_string();

    let list = env
        .server
        .get(&format!("/admin/user/{}/devices", env.user_id))
        .add_header(h.clone(), v.clone())
        .await;
    list.assert_status_ok();
    let list_body: Vec<Value> = list.json();
    assert_eq!(list_body.len(), 1);
    assert_eq!(list_body[0]["clientId"], client_id);
    assert_eq!(list_body[0]["status"], "active");
    assert_rfc3339_timestamp(&list_body[0]["registeredAt"]);

    let show = env
        .server
        .get(&format!(
            "/admin/user/{}/devices/{}",
            env.user_id, client_id
        ))
        .add_header(h.clone(), v.clone())
        .await;
    show.assert_status_ok();
    let show_body: Value = show.json();
    assert_eq!(show_body["name"], "Operator MacBook");
    assert_rfc3339_timestamp(&show_body["registeredAt"]);

    env.server
        .patch(&format!(
            "/admin/user/{}/devices/{}",
            env.user_id, client_id
        ))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "name": "Renamed Operator MacBook",
        }))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    let renamed = env.store.get_device(&client_id).await.unwrap().unwrap();
    assert_eq!(renamed.name, "Renamed Operator MacBook");

    env.server
        .delete(&format!(
            "/admin/user/{}/devices/{}",
            env.user_id, client_id
        ))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status(axum::http::StatusCode::CONFLICT);

    env.server
        .post(&format!(
            "/admin/user/{}/devices/{}/revoke",
            env.user_id, client_id
        ))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    assert_eq!(
        env.store
            .get_device(&client_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "revoked"
    );

    env.server
        .post(&format!(
            "/admin/user/{}/devices/{}/unrevoke",
            env.user_id, client_id
        ))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    assert_eq!(
        env.store
            .get_device(&client_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "active"
    );

    env.server
        .post(&format!(
            "/admin/user/{}/devices/{}/revoke",
            env.user_id, client_id
        ))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    env.server
        .delete(&format!(
            "/admin/user/{}/devices/{}",
            env.user_id, client_id
        ))
        .add_header(h, v)
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    assert!(env.store.get_device(&client_id).await.unwrap().is_none());
}

#[tokio::test]
async fn test_operator_device_create_honors_public_server_url_override() {
    let env = setup_bootstrap().await;
    let (h, v) = auth_header(&env.admin_token);

    env.server
        .post(&format!("/admin/user/{}/sync-identity/ensure", env.user_id))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status_ok();

    let create = env
        .server
        .post(&format!("/admin/user/{}/devices", env.user_id))
        .add_header(h, v)
        .json(&serde_json::json!({
            "name": "Override URL Device",
            "publicServerUrlOverride": "https://override.example.com",
        }))
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let body: Value = create.json();
    assert_eq!(
        body["taskrcLines"][0].as_str(),
        Some("sync.server.url=https://override.example.com")
    );
}

#[tokio::test]
async fn test_operator_device_rename_rejects_invalid_names() {
    let env = setup_bootstrap().await;
    let (h, v) = auth_header(&env.admin_token);

    env.server
        .post(&format!("/admin/user/{}/sync-identity/ensure", env.user_id))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status_ok();

    let create = env
        .server
        .post(&format!("/admin/user/{}/devices", env.user_id))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "name": "Rename Validation Device",
        }))
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let body: Value = create.json();
    let client_id = body["clientId"].as_str().unwrap().to_string();

    env.server
        .patch(&format!(
            "/admin/user/{}/devices/{}",
            env.user_id, client_id
        ))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "name": "",
        }))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);

    env.server
        .patch(&format!(
            "/admin/user/{}/devices/{}",
            env.user_id, client_id
        ))
        .add_header(h, v)
        .json(&serde_json::json!({
            "name": "x".repeat(256),
        }))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_operator_device_create_rejects_invalid_payload() {
    let env = setup_bootstrap().await;
    let (h, v) = auth_header(&env.admin_token);

    env.server
        .post(&format!("/admin/user/{}/devices", env.user_id))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "name": "   ",
        }))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);

    env.server
        .post(&format!("/admin/user/{}/devices", env.user_id))
        .add_header(h, v)
        .json(&serde_json::json!({
            "name": "Valid Name",
            "publicServerUrlOverride": "   ",
        }))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_operator_device_returns_404_for_nonexistent_valid_uuid() {
    let env = setup_bootstrap().await;
    let (h, v) = auth_header(&env.admin_token);
    let missing_client_id = Uuid::new_v4().to_string();

    env.server
        .get(&format!(
            "/admin/user/{}/devices/{}",
            env.user_id, missing_client_id
        ))
        .add_header(h, v)
        .await
        .assert_status(axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_operator_device_cross_user_access_returns_404() {
    let env = setup_bootstrap().await;
    let (h, v) = auth_header(&env.admin_token);

    let other_user = env
        .store
        .create_user(&NewUser {
            username: "other-operator-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();
    std::fs::create_dir_all(env._tmp.path().join("users").join(&other_user.id)).unwrap();

    env.server
        .post(&format!(
            "/admin/user/{}/sync-identity/ensure",
            other_user.id
        ))
        .add_header(h.clone(), v.clone())
        .await
        .assert_status_ok();

    let create = env
        .server
        .post(&format!("/admin/user/{}/devices", other_user.id))
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "name": "Other User Device",
        }))
        .await;
    create.assert_status(axum::http::StatusCode::CREATED);
    let body: Value = create.json();
    let other_client_id = body["clientId"].as_str().unwrap().to_string();

    env.server
        .get(&format!(
            "/admin/user/{}/devices/{}",
            env.user_id, other_client_id
        ))
        .add_header(h, v)
        .await
        .assert_status(axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_first_sync_implicitly_acknowledges_bootstrap_device() {
    let env = setup_bootstrap().await;
    let bootstrap_request_id = Uuid::new_v4().to_string();
    let (h, v) = auth_header(&env.admin_token);

    let resp = env
        .server
        .post("/admin/bootstrap/user-device")
        .add_header(h, v)
        .json(&serde_json::json!({
            "username": "bootstrap-sync-user",
            "createUserIfMissing": true,
            "deviceName": "Bootstrap Linux Box",
            "bootstrapRequestId": bootstrap_request_id,
        }))
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let device_client_id = body["deviceClientId"].as_str().unwrap().to_string();

    let nil = Uuid::nil();
    let (ch, cv) = client_id_header(&device_client_id);
    env.server
        .get(&format!("/v1/client/get-child-version/{nil}"))
        .add_header(ch, cv)
        .await
        .assert_status(axum::http::StatusCode::NOT_FOUND);

    tokio::time::sleep(Duration::from_millis(50)).await;

    let device = env
        .store
        .get_device(&device_client_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(device.bootstrap_status.as_deref(), Some("acknowledged"));
}

#[tokio::test]
async fn test_bootstrap_existing_user_blocked_by_runtime_policy() {
    let env = setup_bootstrap().await;
    let policy = RuntimePolicy {
        runtime_access: RuntimeAccessMode::Block,
        delete_action: RuntimeDeleteAction::Allow,
    };
    env.store
        .upsert_runtime_policy(
            &env.user_id,
            "policy-v1",
            &policy,
            Some("policy-v1"),
            Some(&policy),
            Some("2026-04-03 12:00:00"),
        )
        .await
        .unwrap();

    let bootstrap_request_id = Uuid::new_v4().to_string();
    let (h, v) = auth_header(&env.admin_token);
    let resp = env
        .server
        .post("/admin/bootstrap/user-device")
        .add_header(h, v)
        .json(&serde_json::json!({
            "userId": env.user_id,
            "deviceName": "Blocked Bootstrap Device",
            "bootstrapRequestId": bootstrap_request_id,
        }))
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
    assert!(resp.text().contains("Runtime access blocked by policy"));
    assert!(env
        .store
        .list_devices(&env.user_id)
        .await
        .unwrap()
        .is_empty());
    assert!(env
        .store
        .get_replica_by_user(&env.user_id)
        .await
        .unwrap()
        .is_none());
}
