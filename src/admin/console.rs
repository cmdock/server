use axum::{
    http::{header, HeaderMap, HeaderValue},
    response::{Html, IntoResponse, Response},
};

const HTML: &str = include_str!("assets/operator-console.html");
const CSS: &str = include_str!("assets/operator-console.css");
const JS: &str = include_str!("assets/operator-console.js");

fn static_headers(content_type: &'static str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    );
    headers
}

/// GET /admin/console — operator-facing console shell.
///
/// This route serves the operator console shell only. The actual admin auth
/// boundary remains the `/admin/*` API surface, which still requires the
/// separate operator bearer token on each API call.
pub async fn console_shell() -> impl IntoResponse {
    (static_headers("text/html; charset=utf-8"), Html(HTML))
}

/// GET /admin/console/style.css — operator console stylesheet.
pub async fn console_styles() -> Response {
    (static_headers("text/css; charset=utf-8"), CSS).into_response()
}

/// GET /admin/console/app.js — operator console client logic.
pub async fn console_script() -> Response {
    (static_headers("application/javascript; charset=utf-8"), JS).into_response()
}
