use std::sync::Arc;

use axum::http::StatusCode;
use taskchampion::{Replica, SqliteStorage};
use tokio::sync::Mutex;

use crate::app_state::AppState;
use crate::metrics as m;
use crate::replica;

pub fn block_quarantined_user(
    state: &AppState,
    user_id: &str,
    source: &'static str,
) -> Result<(), StatusCode> {
    if state.is_user_quarantined(user_id) {
        m::record_quarantine_blocked(source);
        Err(StatusCode::SERVICE_UNAVAILABLE)
    } else {
        Ok(())
    }
}

pub async fn open_user_replica(
    state: &AppState,
    user_id: &str,
    source: &'static str,
) -> Result<Arc<Mutex<Replica<SqliteStorage>>>, StatusCode> {
    block_quarantined_user(state, user_id, source)?;
    state
        .replica_manager
        .get_replica(user_id)
        .await
        .map_err(|e| handle_replica_error(state, user_id, &e, "open_replica", source))
}

pub fn handle_replica_error(
    state: &AppState,
    user_id: &str,
    err: &impl std::fmt::Display,
    operation: &'static str,
    source: &'static str,
) -> StatusCode {
    if replica::is_sqlite_corruption(err) {
        tracing::error!("SQLite corruption detected for user {user_id} during {operation}: {err}");
        quarantine_for_corruption(
            state,
            user_id,
            operation,
            source,
            "replica.corruption_detected",
        );
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        replica::check_busy_error(err, operation);
        tracing::error!("Replica operation failed for user {user_id}: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

pub fn handle_sync_error(
    state: &AppState,
    user_id: &str,
    err: &anyhow::Error,
    operation: &'static str,
    source: &'static str,
) -> StatusCode {
    if replica::is_corruption_in_chain(err) {
        tracing::error!("SQLite corruption detected for user {user_id} during {operation}: {err}");
        quarantine_for_corruption(
            state,
            user_id,
            operation,
            source,
            "sync.corruption_detected",
        );
        StatusCode::SERVICE_UNAVAILABLE
    } else if replica::is_busy_in_chain(err) {
        replica::check_busy_error(err, operation);
        tracing::warn!("{operation} hit SQLite contention for user {user_id}: {err}");
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        tracing::error!("{operation} failed for user {user_id}: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

pub fn handle_sync_open_status(
    state: &AppState,
    user_id: &str,
    status: StatusCode,
    operation: &'static str,
    source: &'static str,
) -> StatusCode {
    if status == StatusCode::SERVICE_UNAVAILABLE {
        quarantine_for_corruption(
            state,
            user_id,
            operation,
            source,
            "sync.corruption_detected",
        );
    }
    status
}

fn quarantine_for_corruption(
    state: &AppState,
    user_id: &str,
    operation: &'static str,
    source: &'static str,
    audit_action: &'static str,
) {
    state.quarantine_user(user_id);
    m::record_sqlite_corruption(source, operation);
    tracing::warn!(
        target: "audit",
        action = audit_action,
        user_id = %user_id,
        operation = %operation,
        source = source,
    );
}
