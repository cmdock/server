pub mod defaults;
pub mod handlers;

use crate::app_state::AppState;
use crate::store::models::ViewRecord;
use crate::store::ConfigStore;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/views", axum::routing::get(handlers::list_views))
        .route("/api/views/{id}", axum::routing::put(handlers::upsert_view))
        .route(
            "/api/views/{id}",
            axum::routing::delete(handlers::delete_view),
        )
}

/// Resolve a view by id for view-scoped reads.
///
/// Reconciles default views (lazily seeds new builtins, updates
/// unmodified builtins to current template version), then looks up the
/// view by id. Returns `Ok(None)` if no view with that id exists for the
/// user (caller should map to 404).
///
/// This is the published views entry point used by callers outside
/// `src/views/` (currently only `GET /api/tasks?view=<id>`). It exists
/// so callers don't need to know about default reconciliation timing or
/// the underlying `list_views` query — see ADR-0002 review 2026-05-04
/// finding A / server#128 follow-up.
pub async fn resolve_view(
    store: &dyn ConfigStore,
    user_id: &str,
    view_id: &str,
) -> anyhow::Result<Option<ViewRecord>> {
    defaults::reconcile_default_views(store, user_id).await?;
    let views = store.list_views(user_id).await?;
    Ok(views.into_iter().find(|v| v.id == view_id))
}
