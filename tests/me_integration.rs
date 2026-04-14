mod common;

use std::sync::Arc;

use axum::http::{header, HeaderValue, Method};
use axum::Router;
use axum_test::TestServer;
use chrono::DateTime;
use serde_json::Value;
use tempfile::TempDir;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use cmdock_server::admin;
use cmdock_server::app_config;
use cmdock_server::app_state::AppState;
use cmdock_server::config_api;
use cmdock_server::health;
use cmdock_server::me;
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::summary;
use cmdock_server::sync;
use cmdock_server::tasks;
use cmdock_server::tc_sync;
use cmdock_server::views;

fn auth_header(token: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

struct TestEnv {
    server: TestServer,
    store: Arc<dyn ConfigStore>,
    _tmp: TempDir,
    user_id: String,
    username: String,
    token: String,
}

async fn setup() -> TestEnv {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(data_dir.join("users")).unwrap();

    let db_path = data_dir.join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "me_test_user".to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();

    let config = common::test_server_config(data_dir.clone());

    let state = AppState::new(store.clone(), &config);

    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(me::routes())
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

    let server = TestServer::new(app);

    TestEnv {
        server,
        store,
        _tmp: tmp,
        user_id: user.id,
        username: user.username,
        token,
    }
}

#[tokio::test]
async fn test_get_me_returns_authenticated_runtime_identity() {
    let env = setup().await;
    let (h, v) = auth_header(&env.token);

    let resp = env.server.get("/api/me").add_header(h, v).await;
    resp.assert_status_ok();
    let body: Value = resp.json();

    assert_eq!(body["id"], env.user_id);
    assert_eq!(body["username"], env.username);
    let created_at = body["createdAt"].as_str().unwrap();
    DateTime::parse_from_rfc3339(created_at).unwrap();
}

#[tokio::test]
async fn test_get_me_requires_auth() {
    let env = setup().await;

    env.server
        .get("/api/me")
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_get_me_marks_connect_config_token_used() {
    let env = setup().await;
    let token = env
        .store
        .create_connect_config_token(&env.user_id, "2099-01-01 00:00:00", 18)
        .await
        .unwrap()
        .token;
    let (h, v) = auth_header(&token);

    let resp = env.server.get("/api/me").add_header(h, v).await;
    resp.assert_status_ok();

    let token_row = env
        .store
        .list_api_tokens(&env.user_id)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.label.as_deref() == Some("connect-config"))
        .unwrap();

    assert!(token_row.first_used_at.is_some());
    assert!(token_row.last_used_at.is_some());
    assert_eq!(token_row.last_used_ip.as_deref(), Some("unknown"));
}
