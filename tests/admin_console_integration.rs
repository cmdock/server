mod common;

use std::sync::Arc;

use axum::http::{header, Method};
use axum::Router;
use axum_test::TestServer;
use tempfile::TempDir;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use cmdock_server::admin;
use cmdock_server::app_config;
use cmdock_server::app_state::AppState;
use cmdock_server::config_api;
use cmdock_server::geofences;
use cmdock_server::health;
use cmdock_server::me;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::summary;
use cmdock_server::sync;
use cmdock_server::tasks;
use cmdock_server::tc_sync;
use cmdock_server::views;

fn build_app(state: AppState) -> Router {
    let rest_routes = Router::new()
        .merge(health::routes())
        .merge(tasks::routes())
        .merge(views::routes())
        .merge(config_api::routes())
        .merge(app_config::routes())
        .merge(geofences::routes())
        .merge(summary::routes())
        .merge(sync::routes())
        .merge(me::routes())
        .merge(admin::routes())
        .with_state(state.clone())
        .layer(RequestBodyLimitLayer::new(1024 * 1024));

    let sync_routes = tc_sync::routes().with_state(state);

    Router::new()
        .merge(rest_routes)
        .merge(sync_routes)
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::PATCH,
                    Method::DELETE,
                ])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        )
}

async fn setup() -> TestServer {
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

    let config = common::test_server_config_with_admin_token(data_dir, "console-test-token");
    let state = AppState::new(store, &config);
    TestServer::new(build_app(state))
}

#[tokio::test]
async fn test_operator_console_shell_loads_without_auth() {
    let server = setup().await;

    let response = server.get("/admin/console").await;
    response.assert_status_ok();
    let body = response.text();

    assert!(body.contains("Operator Console"));
    assert!(body.contains("/admin/console/style.css"));
    assert!(body.contains("/admin/console/app.js"));
}

#[tokio::test]
async fn test_operator_console_assets_load() {
    let server = setup().await;

    let css = server.get("/admin/console/style.css").await;
    css.assert_status_ok();
    assert!(css.text().contains(".masthead"));

    let js = server.get("/admin/console/app.js").await;
    js.assert_status_ok();
    let body = js.text();
    assert!(body.contains("/admin/status"));
    assert!(body.contains("/admin/user/"));
    assert!(body.contains("sessionStorage"));
}
