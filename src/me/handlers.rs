use axum::{http::StatusCode, Json};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;

use crate::auth::AuthUser;

#[derive(Debug, Serialize, ToSchema)]
#[schema(example = json!({
    "id": "86a9cca3-5689-41e4-8361-8075c9c49b38",
    "username": "alice",
    "createdAt": "2026-04-02T09:20:42+00:00"
}))]
#[serde(rename_all = "camelCase")]
pub struct MeResponse {
    #[schema(format = "uuid", example = "86a9cca3-5689-41e4-8361-8075c9c49b38")]
    pub id: String,
    pub username: String,
    #[schema(format = "date-time", example = "2026-04-02T09:20:42+00:00")]
    pub created_at: String,
}

#[utoipa::path(
    get,
    path = "/api/me",
    operation_id = "getAuthenticatedUser",
    responses(
        (status = 200, description = "Authenticated runtime identity", body = MeResponse),
        (status = 401, description = "Unauthorised"),
    ),
    tag = "me"
)]
pub async fn get_me(auth: AuthUser) -> Result<Json<MeResponse>, StatusCode> {
    Ok(Json(MeResponse {
        id: auth.user_id,
        username: auth.username,
        created_at: sqlite_utc_to_rfc3339(&auth.created_at),
    }))
}

fn sqlite_utc_to_rfc3339(value: &str) -> String {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .map(|dt| DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc).to_rfc3339())
        .unwrap_or_else(|_| value.to_string())
}
