use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::auth::AuthUser;
use crate::store::models::{NewWebhookRecord, UpdateWebhookRecord};

use super::api::{
    api_error, internal_error, map_delivery_response, map_user_store_error, map_webhook_response,
    normalize_registration, CreateWebhookRequest, UpdateWebhookRequest, WebhookApiError,
    WebhookDetailResponse, WebhookErrorResponse, WebhookResponse, WebhookTestResponse,
    MAX_WEBHOOKS_PER_USER, RECENT_DELIVERY_LIMIT,
};
use super::delivery;

#[utoipa::path(
    get,
    path = "/api/webhooks",
    responses(
        (status = 200, description = "List of registered webhooks", body = Vec<WebhookResponse>),
        (status = 401, description = "Unauthorised"),
    ),
    tag = "webhooks"
)]
pub async fn list_webhooks(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<WebhookResponse>>, StatusCode> {
    let webhooks = state
        .store
        .list_webhooks(&auth.user_id)
        .await
        .map_err(|err| {
            tracing::error!("Failed to list webhooks: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(
        webhooks.into_iter().map(map_webhook_response).collect(),
    ))
}

#[utoipa::path(
    get,
    path = "/api/webhooks/{id}",
    params(("id" = String, Path, description = "Webhook ID")),
    responses(
        (status = 200, description = "Webhook details and recent deliveries", body = WebhookDetailResponse),
        (status = 404, description = "Webhook not found", body = WebhookErrorResponse),
        (status = 401, description = "Unauthorised"),
    ),
    tag = "webhooks"
)]
pub async fn get_webhook(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<WebhookDetailResponse>, (StatusCode, Json<WebhookErrorResponse>)> {
    let webhook = state
        .store
        .get_webhook(&auth.user_id, &id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| api_error(WebhookApiError::NotFound, "Webhook not found"))?;
    let deliveries = state
        .store
        .list_webhook_deliveries(&auth.user_id, &id, RECENT_DELIVERY_LIMIT)
        .await
        .map_err(internal_error)?;
    Ok(Json(WebhookDetailResponse {
        webhook: map_webhook_response(webhook),
        deliveries: deliveries.into_iter().map(map_delivery_response).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/webhooks",
    request_body = CreateWebhookRequest,
    responses(
        (status = 201, description = "Webhook created", body = WebhookResponse),
        (status = 400, description = "Validation failed", body = WebhookErrorResponse),
        (status = 409, description = "Duplicate URL", body = WebhookErrorResponse),
        (status = 422, description = "Webhook limit reached", body = WebhookErrorResponse),
        (status = 401, description = "Unauthorised"),
    ),
    tag = "webhooks"
)]
pub async fn create_webhook(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateWebhookRequest>,
) -> Result<(StatusCode, Json<WebhookResponse>), (StatusCode, Json<WebhookErrorResponse>)> {
    if state
        .store
        .list_webhooks(&auth.user_id)
        .await
        .map_err(internal_error)?
        .len()
        >= MAX_WEBHOOKS_PER_USER
    {
        return Err(api_error(
            WebhookApiError::LimitReached,
            "Webhook limit reached for this user",
        ));
    }

    let normalized = normalize_registration(
        &state,
        &body.url,
        Some(&body.secret),
        &body.events,
        body.modified_fields.as_ref(),
        body.name.as_deref(),
    )
    .await?;

    let created = state
        .store
        .create_webhook(&NewWebhookRecord {
            id: format!("wh_{}", Uuid::new_v4().simple()),
            user_id: auth.user_id,
            url: normalized.url,
            events: normalized.events,
            modified_fields: normalized.modified_fields,
            name: normalized.name,
            enabled: true,
            secret_enc: normalized.secret_enc.expect("create always has secret"),
        })
        .await
        .map_err(map_user_store_error)?;

    Ok((StatusCode::CREATED, Json(map_webhook_response(created))))
}

#[utoipa::path(
    put,
    path = "/api/webhooks/{id}",
    params(("id" = String, Path, description = "Webhook ID")),
    request_body = UpdateWebhookRequest,
    responses(
        (status = 200, description = "Webhook updated", body = WebhookResponse),
        (status = 400, description = "Validation failed", body = WebhookErrorResponse),
        (status = 404, description = "Webhook not found", body = WebhookErrorResponse),
        (status = 409, description = "Duplicate URL", body = WebhookErrorResponse),
        (status = 401, description = "Unauthorised"),
    ),
    tag = "webhooks"
)]
pub async fn update_webhook(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<UpdateWebhookRequest>,
) -> Result<Json<WebhookResponse>, (StatusCode, Json<WebhookErrorResponse>)> {
    let normalized = normalize_registration(
        &state,
        &body.url,
        body.secret.as_deref(),
        &body.events,
        body.modified_fields.as_ref(),
        body.name.as_deref(),
    )
    .await?;

    let updated = state
        .store
        .update_webhook(&UpdateWebhookRecord {
            id,
            user_id: auth.user_id,
            url: normalized.url,
            events: normalized.events,
            modified_fields: normalized.modified_fields,
            name: normalized.name,
            enabled: body.enabled.unwrap_or(true),
            secret_enc: normalized.secret_enc,
        })
        .await
        .map_err(map_user_store_error)?;

    let Some(updated) = updated else {
        return Err(api_error(WebhookApiError::NotFound, "Webhook not found"));
    };

    Ok(Json(map_webhook_response(updated)))
}

#[utoipa::path(
    delete,
    path = "/api/webhooks/{id}",
    params(("id" = String, Path, description = "Webhook ID")),
    responses(
        (status = 204, description = "Webhook deleted"),
        (status = 404, description = "Webhook not found", body = WebhookErrorResponse),
        (status = 401, description = "Unauthorised"),
    ),
    tag = "webhooks"
)]
pub async fn delete_webhook(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<WebhookErrorResponse>)> {
    let deleted = state
        .store
        .delete_webhook(&auth.user_id, &id)
        .await
        .map_err(internal_error)?;
    if !deleted {
        return Err(api_error(WebhookApiError::NotFound, "Webhook not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/api/webhooks/{id}/test",
    params(("id" = String, Path, description = "Webhook ID")),
    responses(
        (status = 200, description = "Test delivery attempted", body = WebhookTestResponse),
        (status = 404, description = "Webhook not found", body = WebhookErrorResponse),
        (status = 401, description = "Unauthorised"),
    ),
    tag = "webhooks"
)]
pub async fn test_webhook(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<WebhookTestResponse>, (StatusCode, Json<WebhookErrorResponse>)> {
    let webhook = state
        .store
        .get_webhook(&auth.user_id, &id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| api_error(WebhookApiError::NotFound, "Webhook not found"))?;
    let delivery = delivery::send_test_delivery(&state, &webhook, None)
        .await
        .map_err(internal_error)?;
    Ok(Json(WebhookTestResponse {
        delivery: map_delivery_response(delivery),
    }))
}
