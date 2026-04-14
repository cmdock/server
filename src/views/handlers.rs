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
use crate::store::models::ViewRecord;

/// View definition as returned by the API — matches iOS TaskViewConfig.
///
/// New users receive 6 built-in default views (duesoon, action, personal,
/// work, health, shopping). Users can customise or delete these via PUT/DELETE.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "duesoon",
    "label": "Due Soon",
    "icon": "clock",
    "filter": "status:pending -BLOCKED -WAITING due.before:7d",
    "group": null,
    "displayMode": "list"
}))]
pub struct ViewConfig {
    pub id: String,
    pub label: String,
    /// SF Symbol name for iOS
    pub icon: String,
    /// Taskwarrior filter expression
    pub filter: String,
    /// Grouping: null = flat list, "tags" or "project" = grouped
    pub group: Option<String>,
    /// Display mode: "list" (flat), "grouped" (grouped by project/tags)
    #[serde(rename = "displayMode")]
    pub display_mode: String,
}

/// Request body for creating/updating a view (id comes from the path).
#[derive(Debug, Deserialize, ToSchema, Validate)]
#[schema(example = json!({
    "label": "Due Soon",
    "icon": "clock",
    "filter": "status:pending -BLOCKED -WAITING due.before:7d",
    "group": null,
    "displayMode": "list"
}))]
pub struct UpsertViewRequest {
    #[garde(
        length(min = 1, max = 128),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub label: String,
    /// SF Symbol name for iOS
    #[garde(
        length(min = 1, max = 64),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub icon: String,
    /// Taskwarrior filter expression
    #[garde(length(max = 512), custom(crate::validation::no_control_chars))]
    pub filter: String,
    /// Grouping: null = flat list, "tags" or "project" = grouped
    #[garde(inner(
        length(min = 1, max = 32),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    ))]
    pub group: Option<String>,
    /// Display mode: "list" (flat), "grouped" (grouped by project/tags).
    /// Defaults to "list" if omitted.
    #[serde(rename = "displayMode", default = "default_display_mode")]
    #[garde(
        length(min = 1, max = 32),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub display_mode: String,
}

fn default_display_mode() -> String {
    "list".to_string()
}

impl From<ViewRecord> for ViewConfig {
    fn from(r: ViewRecord) -> Self {
        Self {
            id: r.id,
            label: r.label,
            icon: r.icon,
            filter: r.filter,
            group: r.group_by,
            display_mode: r.display_mode,
        }
    }
}

/// List all view definitions for the authenticated user.
///
/// New users automatically receive 6 built-in default views on first access.
/// Users can customise these via PUT or hide them via DELETE.
#[utoipa::path(
    get,
    path = "/api/views",
    operation_id = "listViews",
    responses(
        (status = 200, description = "List of views", body = Vec<ViewConfig>),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "views"
)]
pub async fn list_views(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<ViewConfig>>, StatusCode> {
    // Lazy reconcile: ensure default views exist for this user.
    // This is idempotent and fast (skips if all defaults present).
    if let Err(e) =
        super::defaults::reconcile_default_views(state.store.as_ref(), &auth.user_id).await
    {
        tracing::warn!(
            "Failed to reconcile default views for {}: {e}",
            auth.user_id
        );
        // Continue — user may still have views from a previous seed
    }

    let views = state
        .store
        .list_views(&auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list views: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .into_iter()
        .map(ViewConfig::from)
        .collect();

    Ok(Json(views))
}

/// Create or update a view definition.
#[utoipa::path(
    put,
    path = "/api/views/{id}",
    operation_id = "upsertView",
    params(("id" = String, Path, description = "View ID")),
    request_body = UpsertViewRequest,
    responses(
        (status = 200, description = "View upserted"),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "views"
)]
pub async fn upsert_view(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<UpsertViewRequest>,
) -> StatusCode {
    if crate::validation::validate_resource_id(&id).is_err()
        || crate::validation::validate_or_bad_request(&body, "Invalid view payload").is_err()
    {
        return StatusCode::BAD_REQUEST;
    }

    // Check if this is a user modification of a builtin view
    let existing = state
        .store
        .list_views_all(&auth.user_id)
        .await
        .unwrap_or_default();
    let existing_view = existing.iter().find(|v| v.id == id);
    let is_builtin_edit = existing_view.is_some_and(|v| v.origin == "builtin");
    let context_filtered = if is_builtin_edit {
        crate::views::defaults::builtin_view(&id)
            .map(|v| v.context_filtered)
            .unwrap_or(false)
    } else {
        false
    };

    let record = ViewRecord {
        id: id.clone(),
        label: body.label,
        icon: body.icon,
        filter: body.filter,
        group_by: body.group,
        context_filtered,
        display_mode: body.display_mode,
        sort_order: existing_view.map(|v| v.sort_order).unwrap_or(0),
        origin: if is_builtin_edit {
            "builtin".to_string()
        } else {
            "user".to_string()
        },
        // Mark as user_modified only if the user is changing a builtin's content.
        // Re-creating a hidden builtin (un-hiding via PUT) also counts as a modification
        // since the user is actively choosing to restore it with specific values.
        user_modified: is_builtin_edit,
        hidden: false, // Unhide if user is re-creating a deleted builtin
        template_version: existing_view.map(|v| v.template_version).unwrap_or(0),
    };

    match state.store.upsert_view(&auth.user_id, &record).await {
        Ok(_) => {
            tracing::info!(
                target: "audit",
                action = "config.view.upsert",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                view_id = %id,
            );
            StatusCode::OK
        }
        Err(e) => {
            tracing::error!("Failed to upsert view: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Delete a view definition.
#[utoipa::path(
    delete,
    path = "/api/views/{id}",
    operation_id = "deleteView",
    params(("id" = String, Path, description = "View ID")),
    responses(
        (status = 204, description = "View deleted"),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "views"
)]
pub async fn delete_view(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> StatusCode {
    if crate::validation::validate_resource_id(&id).is_err() {
        return StatusCode::BAD_REQUEST;
    }

    match state.store.delete_view(&auth.user_id, &id).await {
        Ok(true) => {
            tracing::info!(
                target: "audit",
                action = "config.view.delete",
                source = "api",
                user_id = %auth.user_id,
                client_ip = %audit::client_ip(&headers, state.config.server.trust_forwarded_headers),
                view_id = %id,
            );
            StatusCode::NO_CONTENT
        }
        Ok(false) => StatusCode::NO_CONTENT,
        Err(e) => {
            tracing::error!("Failed to delete view: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
