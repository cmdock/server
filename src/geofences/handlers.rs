use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::app_state::AppState;
use crate::audit;
use crate::auth::AuthUser;
use crate::store::models::GeofenceRecord;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeofenceConfig {
    pub id: String,
    pub label: String,
    pub latitude: f64,
    pub longitude: f64,
    pub radius: f64,
    #[serde(rename = "type")]
    pub geofence_type: String,
    pub context_id: Option<String>,
    pub view_id: Option<String>,
    pub store_tag: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema, Validate)]
#[serde(rename_all = "camelCase")]
pub struct UpsertGeofenceRequest {
    #[garde(
        length(min = 1, max = 128),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub label: String,
    #[garde(skip)]
    pub latitude: f64,
    #[garde(skip)]
    pub longitude: f64,
    /// Full-replacement upsert: omitted radius falls back to the server default.
    #[garde(skip)]
    pub radius: Option<f64>,
    /// Full-replacement upsert: omitted type falls back to the server default.
    #[serde(rename = "type")]
    #[garde(inner(
        length(min = 1, max = 32),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    ))]
    pub geofence_type: Option<String>,
    #[garde(inner(
        length(min = 1, max = 64),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars),
        custom(crate::validation::safe_resource_id)
    ))]
    pub context_id: Option<String>,
    #[garde(inner(
        length(min = 1, max = 64),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars),
        custom(crate::validation::safe_resource_id)
    ))]
    pub view_id: Option<String>,
    #[garde(inner(
        length(min = 1, max = 64),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    ))]
    pub store_tag: Option<String>,
}

impl From<GeofenceRecord> for GeofenceConfig {
    fn from(value: GeofenceRecord) -> Self {
        Self {
            id: value.id,
            label: value.label,
            latitude: value.latitude,
            longitude: value.longitude,
            radius: value.radius,
            geofence_type: value.geofence_type,
            context_id: value.context_id,
            view_id: value.view_id,
            store_tag: value.store_tag,
        }
    }
}

fn validate_geofence(id: &str, request: &UpsertGeofenceRequest) -> Result<(), StatusCode> {
    crate::validation::validate_resource_id(id)?;
    crate::validation::validate_or_bad_request(request, "Invalid geofence payload")?;
    if request.label.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if !(-90.0..=90.0).contains(&request.latitude) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if !(-180.0..=180.0).contains(&request.longitude) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let radius = request.radius.unwrap_or(200.0);
    if !radius.is_finite() || radius <= 0.0 {
        return Err(StatusCode::BAD_REQUEST);
    }

    if let Some(geofence_type) = &request.geofence_type {
        if geofence_type.trim().is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    Ok(())
}

#[utoipa::path(
    get,
    path = "/api/geofences",
    operation_id = "listGeofences",
    responses(
        (status = 200, description = "List geofences", body = Vec<GeofenceConfig>),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "geofences"
)]
pub async fn list_geofences(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<GeofenceConfig>>, StatusCode> {
    let geofences = state
        .store
        .list_geofences(&auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list geofences: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(geofences.into_iter().map(Into::into).collect()))
}

#[utoipa::path(
    put,
    path = "/api/geofences/{id}",
    operation_id = "upsertGeofence",
    params(("id" = String, Path, description = "Geofence ID")),
    request_body = UpsertGeofenceRequest,
    responses(
        (status = 200, description = "Geofence upserted (full replacement of the typed resource)"),
        (status = 400, description = "Invalid geofence payload"),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "geofences"
)]
pub async fn upsert_geofence(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<UpsertGeofenceRequest>,
) -> StatusCode {
    if let Err(status) = validate_geofence(&id, &body) {
        return status;
    }

    let record = GeofenceRecord {
        id,
        label: body.label,
        latitude: body.latitude,
        longitude: body.longitude,
        radius: body.radius.unwrap_or(200.0),
        geofence_type: body.geofence_type.unwrap_or_else(|| "home".to_string()),
        context_id: body.context_id,
        view_id: body.view_id,
        store_tag: body.store_tag,
    };

    match state.store.upsert_geofence(&auth.user_id, &record).await {
        Ok(_) => {
            tracing::info!(
                target: "audit",
                action = "config.geofence.upsert",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                geofence_id = %record.id,
            );
            StatusCode::OK
        }
        Err(e) => {
            tracing::error!("Failed to upsert geofence '{}': {e}", record.id);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[utoipa::path(
    delete,
    path = "/api/geofences/{id}",
    operation_id = "deleteGeofence",
    params(("id" = String, Path, description = "Geofence ID")),
    responses(
        (status = 204, description = "Geofence deleted"),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "geofences"
)]
pub async fn delete_geofence(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> StatusCode {
    if crate::validation::validate_resource_id(&id).is_err() {
        return StatusCode::BAD_REQUEST;
    }

    match state.store.delete_geofence(&auth.user_id, &id).await {
        Ok(_) => {
            tracing::info!(
                target: "audit",
                action = "config.geofence.delete",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                geofence_id = %id,
            );
            StatusCode::NO_CONTENT
        }
        Err(e) => {
            tracing::error!("Failed to delete geofence '{id}': {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
