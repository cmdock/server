use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::auth::OperatorAuth;
use crate::store::models::{NewAdminWebhookRecord, UpdateAdminWebhookRecord};
use crate::webhooks::api::{
    api_error, internal_error, map_admin_store_error, map_admin_webhook_response,
    map_delivery_response, normalize_registration, CreateWebhookRequest, WebhookApiError,
    WebhookDetailResponse, WebhookErrorResponse, WebhookResponse, WebhookTestResponse,
    MAX_WEBHOOKS_PER_USER, RECENT_DELIVERY_LIMIT,
};
use crate::webhooks::delivery;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAdminWebhookRequest {
    pub enabled: bool,
}

#[utoipa::path(
    get,
    path = "/admin/webhooks",
    operation_id = "listAdminWebhooks",
    responses(
        (status = 200, description = "Admin/per-server webhooks", body = Vec<WebhookResponse>),
        (status = 401, description = "Invalid operator token"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn list_webhooks(
    State(state): State<AppState>,
    _auth: OperatorAuth,
) -> Result<Json<Vec<WebhookResponse>>, StatusCode> {
    let webhooks = state.store.list_admin_webhooks().await.map_err(|err| {
        tracing::error!("Failed to list admin webhooks: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(
        webhooks
            .into_iter()
            .map(map_admin_webhook_response)
            .collect(),
    ))
}

#[utoipa::path(
    get,
    path = "/admin/webhooks/{id}",
    operation_id = "getAdminWebhook",
    params(("id" = String, Path, description = "Admin webhook ID")),
    responses(
        (status = 200, description = "Admin webhook details and recent deliveries", body = WebhookDetailResponse),
        (status = 404, description = "Webhook not found", body = WebhookErrorResponse),
        (status = 401, description = "Invalid operator token"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn get_webhook(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path(id): Path<String>,
) -> Result<Json<WebhookDetailResponse>, (StatusCode, Json<WebhookErrorResponse>)> {
    let webhook = state
        .store
        .get_admin_webhook(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| api_error(WebhookApiError::NotFound, "Admin webhook not found"))?;
    let deliveries = state
        .store
        .list_admin_webhook_deliveries(&id, RECENT_DELIVERY_LIMIT)
        .await
        .map_err(internal_error)?;
    Ok(Json(WebhookDetailResponse {
        webhook: map_admin_webhook_response(webhook),
        deliveries: deliveries.into_iter().map(map_delivery_response).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/admin/webhooks",
    operation_id = "createAdminWebhook",
    request_body = CreateWebhookRequest,
    responses(
        (status = 201, description = "Admin webhook created", body = WebhookResponse),
        (status = 400, description = "Validation failed", body = WebhookErrorResponse),
        (status = 409, description = "Duplicate URL", body = WebhookErrorResponse),
        (status = 422, description = "Webhook limit reached", body = WebhookErrorResponse),
        (status = 401, description = "Invalid operator token"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn create_webhook(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Json(body): Json<CreateWebhookRequest>,
) -> Result<(StatusCode, Json<WebhookResponse>), (StatusCode, Json<WebhookErrorResponse>)> {
    if state
        .store
        .list_admin_webhooks()
        .await
        .map_err(internal_error)?
        .len()
        >= MAX_WEBHOOKS_PER_USER
    {
        return Err(api_error(
            WebhookApiError::LimitReached,
            "Admin webhook limit reached",
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
        .create_admin_webhook(&NewAdminWebhookRecord {
            id: format!("awh_{}", Uuid::new_v4().simple()),
            url: normalized.url,
            events: normalized.events,
            modified_fields: normalized.modified_fields,
            name: normalized.name,
            enabled: true,
            secret_enc: normalized
                .secret_enc
                .expect("admin webhook creation always encrypts a secret"),
        })
        .await
        .map_err(map_admin_store_error)?;

    Ok((
        StatusCode::CREATED,
        Json(map_admin_webhook_response(created)),
    ))
}

#[utoipa::path(
    patch,
    path = "/admin/webhooks/{id}",
    operation_id = "updateAdminWebhook",
    params(("id" = String, Path, description = "Admin webhook ID")),
    request_body = UpdateAdminWebhookRequest,
    responses(
        (status = 200, description = "Admin webhook updated", body = WebhookResponse),
        (status = 404, description = "Webhook not found", body = WebhookErrorResponse),
        (status = 401, description = "Invalid operator token"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn update_webhook(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path(id): Path<String>,
    Json(body): Json<UpdateAdminWebhookRequest>,
) -> Result<Json<WebhookResponse>, (StatusCode, Json<WebhookErrorResponse>)> {
    let updated = state
        .store
        .update_admin_webhook(&UpdateAdminWebhookRecord {
            id,
            enabled: body.enabled,
        })
        .await
        .map_err(internal_error)?;

    let Some(updated) = updated else {
        return Err(api_error(
            WebhookApiError::NotFound,
            "Admin webhook not found",
        ));
    };

    Ok(Json(map_admin_webhook_response(updated)))
}

#[utoipa::path(
    delete,
    path = "/admin/webhooks/{id}",
    operation_id = "deleteAdminWebhook",
    params(("id" = String, Path, description = "Admin webhook ID")),
    responses(
        (status = 204, description = "Admin webhook deleted"),
        (status = 404, description = "Webhook not found", body = WebhookErrorResponse),
        (status = 401, description = "Invalid operator token"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn delete_webhook(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<WebhookErrorResponse>)> {
    let deleted = state
        .store
        .delete_admin_webhook(&id)
        .await
        .map_err(internal_error)?;

    if !deleted {
        return Err(api_error(
            WebhookApiError::NotFound,
            "Admin webhook not found",
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/admin/webhooks/{id}/test",
    operation_id = "testAdminWebhook",
    params(("id" = String, Path, description = "Admin webhook ID")),
    responses(
        (status = 200, description = "Test delivery attempted", body = WebhookTestResponse),
        (status = 404, description = "Webhook not found", body = WebhookErrorResponse),
        (status = 401, description = "Invalid operator token"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn test_webhook(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path(id): Path<String>,
) -> Result<Json<WebhookTestResponse>, (StatusCode, Json<WebhookErrorResponse>)> {
    let webhook = state
        .store
        .get_admin_webhook(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| api_error(WebhookApiError::NotFound, "Admin webhook not found"))?;
    let delivery = delivery::send_admin_test_delivery(&state, &webhook, None)
        .await
        .map_err(internal_error)?;
    Ok(Json(WebhookTestResponse {
        delivery: map_delivery_response(delivery),
    }))
}
