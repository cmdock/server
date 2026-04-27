use axum::{http::StatusCode, Json};
use base64::Engine;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::app_state::AppState;
use crate::crypto;
use crate::store::models::{AdminWebhookRecord, WebhookDeliveryRecord, WebhookRecord};

use super::security;

pub const MAX_WEBHOOKS_PER_USER: usize = 10;
pub const RECENT_DELIVERY_LIMIT: usize = 20;
pub const ALLOWED_EVENTS: &[&str] = &[
    "*",
    "task.*",
    "task.created",
    "task.completed",
    "task.deleted",
    "task.modified",
    "sync.completed",
    "task.due",
    "task.overdue",
];
/// Known task fields that can appear in webhook modified_fields filters.
/// UDA field names are also accepted (open-ended) — validation only
/// rejects empty strings, not unknown names.
pub const KNOWN_MODIFIED_FIELDS: &[&str] = &[
    "description",
    "project",
    "priority",
    "due",
    "tags",
    "status",
    "blocked",
    "waiting",
    "depends",
];

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateWebhookRequest {
    pub url: String,
    pub secret: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateWebhookRequest {
    pub url: String,
    pub secret: Option<String>,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebhookResponse {
    pub id: String,
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub consecutive_failures: u32,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebhookDeliveryResponse {
    pub delivery_id: String,
    pub event_id: String,
    pub event: String,
    pub timestamp: String,
    pub status: String,
    pub response_status: Option<u16>,
    pub attempt: u32,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebhookDetailResponse {
    #[serde(flatten)]
    pub webhook: WebhookResponse,
    pub deliveries: Vec<WebhookDeliveryResponse>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebhookTestResponse {
    pub delivery: WebhookDeliveryResponse,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookErrorResponse {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy)]
pub enum WebhookApiError {
    InvalidUrl,
    InvalidEvents,
    InvalidModifiedFields,
    SecretTooShort,
    DuplicateUrl,
    LimitReached,
    NotFound,
    MasterKeyRequired,
    Internal,
}

impl WebhookApiError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidUrl => "INVALID_URL",
            Self::InvalidEvents => "INVALID_EVENTS",
            Self::InvalidModifiedFields => "INVALID_MODIFIED_FIELDS",
            Self::SecretTooShort => "SECRET_TOO_SHORT",
            Self::DuplicateUrl => "DUPLICATE_URL",
            Self::LimitReached => "LIMIT_REACHED",
            Self::NotFound => "WEBHOOK_NOT_FOUND",
            Self::MasterKeyRequired => "MASTER_KEY_REQUIRED",
            Self::Internal => "INTERNAL_ERROR",
        }
    }

    pub fn status(self) -> StatusCode {
        match self {
            Self::InvalidUrl
            | Self::InvalidEvents
            | Self::InvalidModifiedFields
            | Self::SecretTooShort => StatusCode::BAD_REQUEST,
            Self::DuplicateUrl => StatusCode::CONFLICT,
            Self::LimitReached => StatusCode::UNPROCESSABLE_ENTITY,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::MasterKeyRequired | Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

pub struct NormalizedRegistration {
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub secret_enc: Option<String>,
}

pub async fn normalize_registration(
    state: &AppState,
    url: &str,
    secret: Option<&str>,
    events: &[String],
    modified_fields: Option<&Vec<String>>,
    name: Option<&str>,
) -> Result<NormalizedRegistration, (StatusCode, Json<WebhookErrorResponse>)> {
    let url = validate_url(state, url).await?;
    let events = normalize_events(events)?;
    let modified_fields = normalize_modified_fields(&events, modified_fields)?;
    let name = normalize_name(name)?;
    let secret_enc = secret
        .map(|secret| encrypt_secret(state, secret))
        .transpose()?;

    Ok(NormalizedRegistration {
        url,
        events,
        modified_fields,
        name,
        secret_enc,
    })
}

pub async fn validate_url(
    state: &AppState,
    raw: &str,
) -> Result<String, (StatusCode, Json<WebhookErrorResponse>)> {
    let target = security::prepare_target(state, raw).await.map_err(|_| {
        api_error(
            WebhookApiError::InvalidUrl,
            "Webhook URL must be valid HTTPS and resolve only to public addresses",
        )
    })?;
    Ok(target.url)
}

pub fn normalize_events(
    events: &[String],
) -> Result<Vec<String>, (StatusCode, Json<WebhookErrorResponse>)> {
    let mut normalized = Vec::new();
    for event in events {
        let event = event.trim();
        if event.is_empty() || !ALLOWED_EVENTS.contains(&event) {
            return Err(api_error(
                WebhookApiError::InvalidEvents,
                "Events must contain only supported webhook event names",
            ));
        }
        if !normalized.iter().any(|existing| existing == event) {
            normalized.push(event.to_string());
        }
    }
    if normalized.is_empty() {
        return Err(api_error(
            WebhookApiError::InvalidEvents,
            "Events must contain at least one supported webhook event name",
        ));
    }
    Ok(normalized)
}

pub fn normalize_modified_fields(
    events: &[String],
    modified_fields: Option<&Vec<String>>,
) -> Result<Option<Vec<String>>, (StatusCode, Json<WebhookErrorResponse>)> {
    let has_modified = events
        .iter()
        .any(|event| event == "task.modified" || event == "task.*" || event == "*");
    if !has_modified {
        return Ok(None);
    }

    let Some(modified_fields) = modified_fields else {
        return Ok(None);
    };
    if modified_fields.is_empty() {
        return Ok(None);
    }

    let mut normalized = Vec::new();
    for field in modified_fields {
        let field = field.trim();
        // Field names must start with a letter and contain only alphanumeric,
        // underscores, and dots (for namespaced UDAs like "github.id").
        if field.is_empty()
            || !field
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            || !field.starts_with(|c: char| c.is_ascii_alphabetic())
            || field.ends_with('.')
        {
            return Err(api_error(
                WebhookApiError::InvalidModifiedFields,
                "modified_fields contains invalid field names",
            ));
        }
        if !normalized.iter().any(|existing| existing == field) {
            normalized.push(field.to_string());
        }
    }
    Ok(Some(normalized))
}

pub fn normalize_name(
    name: Option<&str>,
) -> Result<Option<String>, (StatusCode, Json<WebhookErrorResponse>)> {
    let Some(name) = name else {
        return Ok(None);
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.len() > 255 || trimmed.chars().any(char::is_control) {
        return Err(api_error(
            WebhookApiError::InvalidUrl,
            "Webhook name must be 255 characters or fewer and contain no control characters",
        ));
    }
    Ok(Some(trimmed.to_string()))
}

pub fn encrypt_secret(
    state: &AppState,
    secret: &str,
) -> Result<String, (StatusCode, Json<WebhookErrorResponse>)> {
    if secret.chars().count() < 32 {
        return Err(api_error(
            WebhookApiError::SecretTooShort,
            "Webhook secret must be at least 32 characters long",
        ));
    }
    let master_key =
        state.config.master_key.as_ref().ok_or_else(|| {
            api_error(WebhookApiError::MasterKeyRequired, "Master key is required")
        })?;
    let encrypted = crypto::encrypt_secret(secret.as_bytes(), master_key).map_err(|_| {
        api_error(
            WebhookApiError::Internal,
            "Failed to encrypt webhook secret",
        )
    })?;
    Ok(base64::engine::general_purpose::STANDARD.encode(encrypted))
}

pub fn map_webhook_response(webhook: WebhookRecord) -> WebhookResponse {
    WebhookResponse {
        id: webhook.id,
        url: webhook.url,
        events: webhook.events,
        modified_fields: webhook.modified_fields,
        name: webhook.name,
        enabled: webhook.enabled,
        created_at: webhook.created_at,
        consecutive_failures: webhook.consecutive_failures,
    }
}

pub fn map_admin_webhook_response(webhook: AdminWebhookRecord) -> WebhookResponse {
    WebhookResponse {
        id: webhook.id,
        url: webhook.url,
        events: webhook.events,
        modified_fields: webhook.modified_fields,
        name: webhook.name,
        enabled: webhook.enabled,
        created_at: webhook.created_at,
        consecutive_failures: webhook.consecutive_failures,
    }
}

pub fn map_delivery_response(delivery: WebhookDeliveryRecord) -> WebhookDeliveryResponse {
    WebhookDeliveryResponse {
        delivery_id: delivery.delivery_id,
        event_id: delivery.event_id,
        event: delivery.event,
        timestamp: delivery.timestamp,
        status: delivery.status,
        response_status: delivery.response_status,
        attempt: delivery.attempt,
        failure_reason: delivery.failure_reason,
    }
}

pub fn map_user_store_error(err: anyhow::Error) -> (StatusCode, Json<WebhookErrorResponse>) {
    let text = err.to_string();
    if text.contains("webhooks.user_id, webhooks.url") {
        return api_error(
            WebhookApiError::DuplicateUrl,
            "A webhook with this URL already exists for this user",
        );
    }
    internal_error(err)
}

pub fn map_admin_store_error(err: anyhow::Error) -> (StatusCode, Json<WebhookErrorResponse>) {
    let text = err.to_string();
    if text.contains("admin_webhooks.url") {
        return api_error(
            WebhookApiError::DuplicateUrl,
            "An admin webhook with this URL already exists",
        );
    }
    internal_error(err)
}

pub fn internal_error(err: anyhow::Error) -> (StatusCode, Json<WebhookErrorResponse>) {
    tracing::error!("Webhook request failed: {err}");
    api_error(WebhookApiError::Internal, "Internal error")
}

pub fn api_error(kind: WebhookApiError, message: &str) -> (StatusCode, Json<WebhookErrorResponse>) {
    (
        kind.status(),
        Json(WebhookErrorResponse {
            code: kind.code().to_string(),
            message: message.to_string(),
        }),
    )
}
