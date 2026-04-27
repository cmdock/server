use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::app_state::AppState;
use crate::audit;
use crate::auth::AuthUser;
use crate::store::models::GenericConfigRecord;

/// Config response — matches iOS ConfigResponse model
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "version": "abc123",
    "items": [{"id": "geo-1", "label": "Home", "latitude": -33.8}],
    "legacy": false
}))]
pub struct ConfigResponse {
    pub version: Option<String>,
    pub items: Vec<Value>,
    pub legacy: Option<bool>,
}

/// Request body for upserting config.
#[derive(Debug, Deserialize, ToSchema, Validate)]
#[schema(example = json!({"version": "v1", "items": [{"id": "geo-1", "label": "Home"}]}))]
pub struct ConfigUpsertRequest {
    /// Version string for optimistic concurrency
    #[garde(inner(
        length(min = 1, max = 128),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    ))]
    pub version: Option<String>,
    /// Array of config items (structure depends on config_type)
    #[garde(skip)]
    pub items: Vec<Value>,
}

/// Get config by type (backwards-compatible generic config endpoint).
#[utoipa::path(
    get,
    path = "/api/config/{config_type}",
    operation_id = "getConfig",
    params(("config_type" = String, Path, description = "Config type (e.g. geofences)")),
    responses(
        (status = 200, description = "Config data", body = ConfigResponse),
        (status = 400, description = "Invalid config type"),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "config"
)]
pub async fn get_config(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(config_type): Path<String>,
) -> Result<Json<ConfigResponse>, StatusCode> {
    crate::validation::validate_resource_id(&config_type)?;
    let record = state
        .store
        .get_config(&auth.user_id, &config_type)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get config '{config_type}': {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    match record {
        Some(r) => {
            let items: Vec<Value> = serde_json::from_str(&r.items_json).unwrap_or_default();
            Ok(Json(ConfigResponse {
                version: r.version,
                items,
                legacy: Some(false),
            }))
        }
        None => Ok(Json(ConfigResponse {
            version: None,
            items: vec![],
            legacy: Some(false),
        })),
    }
}

/// Upsert config by type.
#[utoipa::path(
    post,
    path = "/api/config/{config_type}",
    operation_id = "upsertConfig",
    params(("config_type" = String, Path, description = "Config type")),
    request_body = ConfigUpsertRequest,
    responses(
        (status = 200, description = "Config saved"),
        (status = 400, description = "Invalid config type or payload"),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "config"
)]
pub async fn upsert_config(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(config_type): Path<String>,
    Json(body): Json<ConfigUpsertRequest>,
) -> StatusCode {
    if crate::validation::validate_resource_id(&config_type).is_err()
        || crate::validation::validate_or_bad_request(&body, "Invalid config payload").is_err()
    {
        return StatusCode::BAD_REQUEST;
    }

    let items_json = serde_json::to_string(&body.items).unwrap_or_default();
    let record = GenericConfigRecord {
        version: body.version,
        items_json,
    };

    match state
        .store
        .upsert_config(&auth.user_id, &config_type, &record)
        .await
    {
        Ok(_) => {
            tracing::info!(
                target: "audit",
                action = "config.generic.upsert",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                config_type = %config_type,
            );
            StatusCode::OK
        }
        Err(e) => {
            tracing::error!("Failed to upsert config '{config_type}': {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Delete a config item by ID.
#[utoipa::path(
    delete,
    path = "/api/config/{config_type}/{item_id}",
    operation_id = "deleteConfigItem",
    params(
        ("config_type" = String, Path, description = "Config type"),
        ("item_id" = String, Path, description = "Item ID to delete"),
    ),
    responses(
        (status = 204, description = "Item deleted"),
        (status = 400, description = "Invalid config type or item ID"),
        (status = 401, description = "Unauthorised"),
        (status = 404, description = "Item not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "config"
)]
pub async fn delete_config_item(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path((config_type, item_id)): Path<(String, String)>,
) -> StatusCode {
    if crate::validation::validate_resource_id(&config_type).is_err()
        || crate::validation::validate_resource_id(&item_id).is_err()
    {
        return StatusCode::BAD_REQUEST;
    }

    match state
        .store
        .delete_config_item(&auth.user_id, &config_type, &item_id)
        .await
    {
        Ok(true) => {
            tracing::info!(
                target: "audit",
                action = "config.generic.delete",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                config_type = %config_type,
                item_id = %item_id,
            );
            StatusCode::NO_CONTENT
        }
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("Failed to delete config item '{config_type}/{item_id}': {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
