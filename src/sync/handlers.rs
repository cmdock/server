use axum::Json;

use crate::auth::AuthUser;
use crate::tasks::models::TaskActionResponse;

/// Trigger sync (legacy compatibility no-op — server is the source of truth).
///
/// This endpoint remains only for backwards compatibility with first-party
/// clients that still call it after mutations.
#[utoipa::path(
    post,
    path = "/api/sync",
    operation_id = "triggerSync",
    responses(
        (status = 200, description = "Compatibility sync acknowledged", body = TaskActionResponse),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "sync"
)]
pub async fn sync(_auth: AuthUser) -> Json<TaskActionResponse> {
    Json(TaskActionResponse {
        success: true,
        output: "Server is source of truth. No sync needed.".to_string(),
    })
}
