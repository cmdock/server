use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use garde::Validate;
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::audit;
use crate::auth::AuthUser;
use crate::geofences::handlers::GeofenceConfig;
use utoipa::ToSchema;

use crate::store::models::{ContextRecord, PresetRecord, StoreRecord};

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppConfigResponse {
    pub contexts: Vec<ContextConfig>,
    pub views: Vec<ViewConfigFull>,
    pub presets: Vec<PresetConfig>,
    pub stores: Vec<StoreConfig>,
    pub shopping: Option<ShoppingConfig>,
    pub geofences: Vec<GeofenceConfig>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextConfig {
    pub id: String,
    pub label: String,
    pub project_prefixes: Vec<String>,
    pub color: Option<String>,
    pub icon: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ViewConfigFull {
    pub id: String,
    pub label: String,
    pub icon: String,
    pub filter: String,
    pub group_by: Option<String>,
    pub context_filtered: bool,
    pub display_mode: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PresetConfig {
    pub id: String,
    pub label: String,
    pub raw_suffix: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StoreConfig {
    pub id: String,
    pub label: String,
    pub tag: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema, Validate)]
#[serde(rename_all = "camelCase")]
pub struct ShoppingConfig {
    #[garde(
        length(min = 1, max = 128),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub project: String,
    #[garde(
        length(max = 32),
        inner(
            length(min = 1, max = 64),
            custom(crate::validation::trimmed_non_empty),
            custom(crate::validation::no_control_chars)
        )
    )]
    pub default_tags: Vec<String>,
}

/// Request body for creating/updating a context (id comes from path).
#[derive(Debug, Deserialize, ToSchema, Validate)]
#[serde(rename_all = "camelCase")]
pub struct UpsertContextRequest {
    #[garde(
        length(min = 1, max = 128),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub label: String,
    #[garde(
        length(max = 32),
        inner(
            length(min = 1, max = 64),
            custom(crate::validation::trimmed_non_empty),
            custom(crate::validation::no_control_chars)
        )
    )]
    pub project_prefixes: Vec<String>,
    #[garde(inner(
        length(min = 1, max = 32),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    ))]
    pub color: Option<String>,
    #[garde(inner(
        length(min = 1, max = 64),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    ))]
    pub icon: Option<String>,
}

/// Request body for creating/updating a store (id comes from path).
#[derive(Debug, Deserialize, ToSchema, Validate)]
pub struct UpsertStoreRequest {
    #[garde(
        length(min = 1, max = 128),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub label: String,
    #[garde(
        length(min = 1, max = 64),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub tag: String,
}

/// Request body for creating/updating a preset (id comes from path).
#[derive(Debug, Deserialize, ToSchema, Validate)]
#[serde(rename_all = "camelCase")]
pub struct UpsertPresetRequest {
    #[garde(
        length(min = 1, max = 128),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub label: String,
    #[garde(length(max = 512), custom(crate::validation::no_control_chars))]
    pub raw_suffix: String,
}

/// Get all app configuration in one call (contexts, views, presets, stores, shopping, geofences).
#[utoipa::path(
    get,
    path = "/api/app-config",
    operation_id = "getAppConfig",
    responses(
        (status = 200, description = "Full app configuration", body = AppConfigResponse),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "app-config"
)]
pub async fn get_app_config(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<AppConfigResponse>, StatusCode> {
    if let Err(e) =
        crate::views::defaults::reconcile_default_views(state.store.as_ref(), &auth.user_id).await
    {
        tracing::warn!(
            "Failed to reconcile default views for {} during app-config load: {e}",
            auth.user_id
        );
    }

    let contexts: Vec<ContextConfig> = state
        .store
        .list_contexts(&auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list contexts: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .into_iter()
        .map(|r| ContextConfig {
            id: r.id,
            label: r.label,
            project_prefixes: r.project_prefixes,
            color: r.color,
            icon: r.icon,
        })
        .collect();

    let views: Vec<ViewConfigFull> = state
        .store
        .list_views(&auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list views: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .into_iter()
        .map(|r| ViewConfigFull {
            id: r.id,
            label: r.label,
            icon: r.icon,
            filter: r.filter,
            group_by: r.group_by,
            context_filtered: r.context_filtered,
            display_mode: r.display_mode,
        })
        .collect();

    let presets: Vec<PresetConfig> = state
        .store
        .list_presets(&auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list presets: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .into_iter()
        .map(|r| PresetConfig {
            id: r.id,
            label: r.label,
            raw_suffix: r.raw_suffix,
        })
        .collect();

    let stores: Vec<StoreConfig> = state
        .store
        .list_stores(&auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list stores: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .into_iter()
        .map(|r| StoreConfig {
            id: r.id,
            label: r.label,
            tag: r.tag,
        })
        .collect();

    let shopping = state
        .store
        .get_shopping_config(&auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get shopping config: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .map(|r| ShoppingConfig {
            project: r.project,
            default_tags: r.default_tags,
        });

    let geofences: Vec<GeofenceConfig> = state
        .store
        .list_geofences(&auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get geofences: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .into_iter()
        .map(Into::into)
        .collect();

    Ok(Json(AppConfigResponse {
        contexts,
        views,
        presets,
        stores,
        shopping,
        geofences,
    }))
}

/// Create or update shopping configuration.
#[utoipa::path(
    put,
    path = "/api/shopping-config",
    operation_id = "upsertShoppingConfig",
    request_body = ShoppingConfig,
    responses(
        (status = 200, description = "Shopping config upserted"),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "app-config"
)]
pub async fn upsert_shopping_config(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Json(body): Json<ShoppingConfig>,
) -> StatusCode {
    if crate::validation::validate_or_bad_request(&body, "Invalid shopping config").is_err() {
        return StatusCode::BAD_REQUEST;
    }

    let record = crate::store::models::ShoppingRecord {
        project: body.project,
        default_tags: body.default_tags,
    };

    match state
        .store
        .upsert_shopping_config(&auth.user_id, &record)
        .await
    {
        Ok(_) => {
            tracing::info!(
                target: "audit",
                action = "config.shopping.upsert",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
            );
            StatusCode::OK
        }
        Err(e) => {
            tracing::error!("Store operation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Delete shopping configuration.
#[utoipa::path(
    delete,
    path = "/api/shopping-config",
    operation_id = "deleteShoppingConfig",
    responses(
        (status = 204, description = "Shopping config deleted"),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "app-config"
)]
pub async fn delete_shopping_config(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
) -> StatusCode {
    match state.store.delete_shopping_config(&auth.user_id).await {
        Ok(true) => {
            tracing::info!(
                target: "audit",
                action = "config.shopping.delete",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
            );
            StatusCode::NO_CONTENT
        }
        Ok(false) => StatusCode::NO_CONTENT,
        Err(e) => {
            tracing::error!("Store operation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// List context definitions.
#[utoipa::path(
    get,
    path = "/api/contexts",
    operation_id = "listContexts",
    responses(
        (status = 200, description = "List of contexts", body = Vec<ContextConfig>),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "app-config"
)]
pub async fn list_contexts(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<ContextConfig>>, StatusCode> {
    let contexts = state
        .store
        .list_contexts(&auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list contexts: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .into_iter()
        .map(|r| ContextConfig {
            id: r.id,
            label: r.label,
            project_prefixes: r.project_prefixes,
            color: r.color,
            icon: r.icon,
        })
        .collect();

    Ok(Json(contexts))
}

/// Create or update a context definition.
#[utoipa::path(put, path = "/api/contexts/{id}", operation_id = "upsertContext",
    params(("id" = String, Path, description = "Context ID")),
    request_body = UpsertContextRequest,
    responses((status = 200, description = "Context upserted"), (status = 401, description = "Unauthorised"), (status = 500, description = "Internal server error")),
    tag = "app-config")]
pub async fn upsert_context(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<UpsertContextRequest>,
) -> StatusCode {
    if crate::validation::validate_resource_id(&id).is_err()
        || crate::validation::validate_or_bad_request(&body, "Invalid context payload").is_err()
    {
        return StatusCode::BAD_REQUEST;
    }

    let record = ContextRecord {
        id: id.clone(),
        label: body.label,
        project_prefixes: body.project_prefixes,
        color: body.color,
        icon: body.icon,
        sort_order: 0,
    };

    match state.store.upsert_context(&auth.user_id, &record).await {
        Ok(_) => {
            tracing::info!(
                target: "audit",
                action = "config.context.upsert",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                context_id = %id,
            );
            StatusCode::OK
        }
        Err(e) => {
            tracing::error!("Store operation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Delete a context definition.
#[utoipa::path(delete, path = "/api/contexts/{id}", operation_id = "deleteContext",
    params(("id" = String, Path, description = "Context ID")),
    responses((status = 204, description = "Context deleted"), (status = 401, description = "Unauthorised"), (status = 500, description = "Internal server error")),
    tag = "app-config")]
pub async fn delete_context(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> StatusCode {
    if crate::validation::validate_resource_id(&id).is_err() {
        return StatusCode::BAD_REQUEST;
    }

    match state.store.delete_context(&auth.user_id, &id).await {
        Ok(true) => {
            tracing::info!(
                target: "audit",
                action = "config.context.delete",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                context_id = %id,
            );
            StatusCode::NO_CONTENT
        }
        Ok(false) => StatusCode::NO_CONTENT,
        Err(e) => {
            tracing::error!("Store operation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// List store definitions.
#[utoipa::path(
    get,
    path = "/api/stores",
    operation_id = "listStores",
    responses(
        (status = 200, description = "List of stores", body = Vec<StoreConfig>),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "app-config"
)]
pub async fn list_stores(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<StoreConfig>>, StatusCode> {
    let stores = state
        .store
        .list_stores(&auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list stores: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .into_iter()
        .map(|r| StoreConfig {
            id: r.id,
            label: r.label,
            tag: r.tag,
        })
        .collect();

    Ok(Json(stores))
}

/// Create or update a store definition.
#[utoipa::path(put, path = "/api/stores/{id}", operation_id = "upsertStore",
    params(("id" = String, Path, description = "Store ID")),
    request_body = UpsertStoreRequest,
    responses((status = 200, description = "Store upserted"), (status = 401, description = "Unauthorised"), (status = 500, description = "Internal server error")),
    tag = "app-config")]
pub async fn upsert_store(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<UpsertStoreRequest>,
) -> StatusCode {
    if crate::validation::validate_resource_id(&id).is_err()
        || crate::validation::validate_or_bad_request(&body, "Invalid store payload").is_err()
    {
        return StatusCode::BAD_REQUEST;
    }

    let record = StoreRecord {
        id: id.clone(),
        label: body.label,
        tag: body.tag,
        sort_order: 0,
    };

    match state.store.upsert_store(&auth.user_id, &record).await {
        Ok(_) => {
            tracing::info!(
                target: "audit",
                action = "config.store.upsert",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                store_id = %id,
            );
            StatusCode::OK
        }
        Err(e) => {
            tracing::error!("Store operation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Delete a store definition.
#[utoipa::path(delete, path = "/api/stores/{id}", operation_id = "deleteStore",
    params(("id" = String, Path, description = "Store ID")),
    responses((status = 204, description = "Store deleted"), (status = 401, description = "Unauthorised"), (status = 500, description = "Internal server error")),
    tag = "app-config")]
pub async fn delete_store(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> StatusCode {
    if crate::validation::validate_resource_id(&id).is_err() {
        return StatusCode::BAD_REQUEST;
    }

    match state.store.delete_store(&auth.user_id, &id).await {
        Ok(true) => {
            tracing::info!(
                target: "audit",
                action = "config.store.delete",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                store_id = %id,
            );
            StatusCode::NO_CONTENT
        }
        Ok(false) => StatusCode::NO_CONTENT,
        Err(e) => {
            tracing::error!("Store operation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Create or update a quick-add preset.
#[utoipa::path(put, path = "/api/presets/{id}", operation_id = "upsertPreset",
    params(("id" = String, Path, description = "Preset ID")),
    request_body = UpsertPresetRequest,
    responses((status = 200, description = "Preset upserted"), (status = 401, description = "Unauthorised"), (status = 500, description = "Internal server error")),
    tag = "app-config")]
pub async fn upsert_preset(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<UpsertPresetRequest>,
) -> StatusCode {
    if crate::validation::validate_resource_id(&id).is_err()
        || crate::validation::validate_or_bad_request(&body, "Invalid preset payload").is_err()
    {
        return StatusCode::BAD_REQUEST;
    }

    let record = PresetRecord {
        id: id.clone(),
        label: body.label,
        raw_suffix: body.raw_suffix,
        sort_order: 0,
    };

    match state.store.upsert_preset(&auth.user_id, &record).await {
        Ok(_) => {
            tracing::info!(
                target: "audit",
                action = "config.preset.upsert",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                preset_id = %id,
            );
            StatusCode::OK
        }
        Err(e) => {
            tracing::error!("Store operation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Delete a quick-add preset.
#[utoipa::path(delete, path = "/api/presets/{id}", operation_id = "deletePreset",
    params(("id" = String, Path, description = "Preset ID")),
    responses((status = 204, description = "Preset deleted"), (status = 401, description = "Unauthorised"), (status = 500, description = "Internal server error")),
    tag = "app-config")]
pub async fn delete_preset(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> StatusCode {
    if crate::validation::validate_resource_id(&id).is_err() {
        return StatusCode::BAD_REQUEST;
    }

    match state.store.delete_preset(&auth.user_id, &id).await {
        Ok(true) => {
            tracing::info!(
                target: "audit",
                action = "config.preset.delete",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                preset_id = %id,
            );
            StatusCode::NO_CONTENT
        }
        Ok(false) => StatusCode::NO_CONTENT,
        Err(e) => {
            tracing::error!("Store operation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
