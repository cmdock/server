//! Integration tests for the typed geofence API and app-config read-through.

mod common;

use std::sync::Arc;

use axum::http::{header, HeaderValue, Method};
use axum::Router;
use axum_test::TestServer;
use serde_json::Value;
use tempfile::TempDir;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use cmdock_server::app_config;
use cmdock_server::app_state::AppState;
use cmdock_server::config_api;
use cmdock_server::geofences;
use cmdock_server::health;
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::summary;
use cmdock_server::sync;
use cmdock_server::tasks;
use cmdock_server::views;

fn auth_header(token: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

struct UserInfo {
    token: String,
}

struct TestEnv {
    server: TestServer,
    _tmp: TempDir,
    user_a: UserInfo,
    user_b: UserInfo,
}

async fn create_user_with_token(store: &Arc<dyn ConfigStore>, username: &str) -> UserInfo {
    let user = store
        .create_user(&NewUser {
            username: username.to_string(),
            password_hash: "not-real".to_string(),
        })
        .await
        .unwrap();

    let token = store
        .create_api_token(&user.id, Some("test"))
        .await
        .unwrap();
    UserInfo { token }
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

    let user_a = create_user_with_token(&store, "geo_user_a").await;
    let user_b = create_user_with_token(&store, "geo_user_b").await;

    let config = common::test_server_config(data_dir.clone());

    let state = AppState::new(store, &config);
    let app = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(geofences::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );

    TestEnv {
        server: TestServer::new(app),
        _tmp: tmp,
        user_a,
        user_b,
    }
}

#[tokio::test]
async fn test_geofence_roundtrip_and_app_config_read_through() {
    let env = setup().await;
    let (h, v) = auth_header(&env.user_a.token);

    env.server
        .put("/api/geofences/home")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Home",
            "latitude": -33.8,
            "longitude": 151.2,
            "radius": 150.0,
            "type": "home",
            "contextId": "errands",
            "viewId": "today",
            "storeTag": "shopping"
        }))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/geofences").add_header(h, v).await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], "home");
    assert_eq!(items[0]["label"], "Home");
    assert_eq!(items[0]["contextId"], "errands");

    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/app-config").add_header(h, v).await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let geofences = body["geofences"].as_array().unwrap();
    assert_eq!(geofences.len(), 1);
    assert_eq!(geofences[0]["id"], "home");
    assert_eq!(geofences[0]["storeTag"], "shopping");
}

#[tokio::test]
async fn test_geofence_requires_auth() {
    let env = setup().await;

    env.server
        .get("/api/geofences")
        .await
        .assert_status_unauthorized();
    env.server
        .put("/api/geofences/home")
        .json(&serde_json::json!({
            "label": "Home",
            "latitude": -33.8,
            "longitude": 151.2
        }))
        .await
        .assert_status_unauthorized();
    env.server
        .delete("/api/geofences/home")
        .await
        .assert_status_unauthorized();
}

#[tokio::test]
async fn test_geofence_validation_and_delete() {
    let env = setup().await;
    let (h, v) = auth_header(&env.user_a.token);

    env.server
        .put("/api/geofences/bad")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Invalid",
            "latitude": 120.0,
            "longitude": 151.2
        }))
        .await
        .assert_status_bad_request();

    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .put("/api/geofences/office")
        .add_header(h.clone(), v.clone())
        .json(&serde_json::json!({
            "label": "   ",
            "latitude": -33.9,
            "longitude": 151.1
        }))
        .await
        .assert_status_bad_request();

    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .put("/api/geofences/office")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Office",
            "latitude": -33.9,
            "longitude": 151.1,
            "contextId": "   "
        }))
        .await
        .assert_status_bad_request();

    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .put("/api/geofences/office")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Office",
            "latitude": -33.9,
            "longitude": 151.1
        }))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .delete("/api/geofences/office")
        .add_header(h, v)
        .await
        .assert_status_no_content();

    let (h, v) = auth_header(&env.user_a.token);
    let resp = env.server.get("/api/geofences").add_header(h, v).await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_geofence_isolated_per_user() {
    let env = setup().await;

    let (h, v) = auth_header(&env.user_a.token);
    env.server
        .put("/api/geofences/home")
        .add_header(h, v)
        .json(&serde_json::json!({
            "label": "Home",
            "latitude": -33.8,
            "longitude": 151.2
        }))
        .await
        .assert_status_ok();

    let (h, v) = auth_header(&env.user_b.token);
    let resp = env.server.get("/api/geofences").add_header(h, v).await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert!(body.as_array().unwrap().is_empty());
}
