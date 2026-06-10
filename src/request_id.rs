//! X-Request-ID middleware — generate or echo a request correlation ID.
//!
//! Injects the ID into request headers (so downstream handlers and audit
//! functions that read `x-request-id` see a stable value) and echoes it
//! in the response (so clients can correlate responses with their requests).

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

static X_REQUEST_ID: axum::http::HeaderName = axum::http::HeaderName::from_static("x-request-id");

/// Middleware that generates or echoes the `X-Request-ID` header.
///
/// - If the incoming request carries `X-Request-ID`, that value is used.
/// - Otherwise a fresh UUID v4 is generated.
///
/// The ID is inserted into the request headers (visible to all downstream
/// handlers) and echoed in the response headers.
pub async fn request_id_middleware(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get(&X_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    if let Ok(val) = axum::http::HeaderValue::from_str(&id) {
        req.headers_mut().insert(X_REQUEST_ID.clone(), val);
    }

    let mut response = next.run(req).await;

    if let Ok(val) = axum::http::HeaderValue::from_str(&id) {
        response.headers_mut().insert(X_REQUEST_ID.clone(), val);
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use axum_test::TestServer;

    async fn echo_request_id_handler(
        axum::extract::State(()): axum::extract::State<()>,
        req: Request<Body>,
    ) -> String {
        req.headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }

    fn test_server() -> TestServer {
        let app = Router::new()
            .route("/", get(echo_request_id_handler))
            .with_state(())
            .layer(middleware::from_fn(request_id_middleware));
        TestServer::new(app)
    }

    #[tokio::test]
    async fn generates_id_when_absent() {
        let server = test_server();
        let resp = server.get("/").await;
        resp.assert_status(StatusCode::OK);
        let echo = resp.text();
        assert!(!echo.is_empty(), "handler must see an injected request id");
        assert_eq!(
            resp.header("x-request-id").to_str().unwrap(),
            echo.as_str(),
            "response header must match injected request id"
        );
        // Value should be a valid UUID.
        Uuid::parse_str(&echo).expect("generated id should be a valid UUID v4");
    }

    #[tokio::test]
    async fn echoes_provided_id() {
        let server = test_server();
        let resp = server
            .get("/")
            .add_header(
                axum::http::HeaderName::from_static("x-request-id"),
                axum::http::HeaderValue::from_static("client-supplied-id"),
            )
            .await;
        resp.assert_status(StatusCode::OK);
        assert_eq!(resp.text(), "client-supplied-id");
        assert_eq!(
            resp.header("x-request-id").to_str().unwrap(),
            "client-supplied-id"
        );
    }
}
