use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};

use crate::admin::handlers::{require_existing_user, validate_user_id};
use crate::admin::services::recovery::RecoveryCoordinator;
use crate::admin::users::{IntegrityMode, IntegrityResult, UserStats, UserStatsQuery};
use crate::app_state::AppState;
use crate::auth::OperatorAuth;

#[utoipa::path(
    get,
    path = "/admin/user/{user_id}/stats",
    operation_id = "getAdminUserStats",
    params(
        ("user_id" = String, Path, description = "User ID"),
        ("integrity" = Option<String>, Query, description = "Run integrity check: quick or full", example = "quick")
    ),
    responses(
        (status = 200, description = "Per-user diagnostics", body = UserStats),
        (status = 400, description = "Invalid user ID or integrity mode"),
        (status = 401, description = "Invalid operator token"),
        (status = 404, description = "User not found"),
        (status = 503, description = "Admin HTTP auth is not configured"),
    ),
    security(
        ("operatorBearer" = [])
    ),
    tag = "admin"
)]
pub async fn user_stats(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path(user_id): Path<String>,
    Query(query): Query<UserStatsQuery>,
) -> Result<Json<UserStats>, StatusCode> {
    validate_user_id(&user_id)?;
    let recovery = RecoveryCoordinator::for_running_state(&state);
    require_existing_user(&state, &user_id).await?;
    let replica_dir = state.user_replica_dir(&user_id);
    let dir_exists = replica_dir.exists();

    let dir_size = if dir_exists {
        std::fs::read_dir(&replica_dir).ok().map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.metadata().ok())
                .map(|metadata| metadata.len())
                .sum()
        })
    } else {
        None
    };

    let replica_cached = state.replica_manager.is_cached(&user_id);
    let quarantined = recovery.is_user_offline(&user_id);
    let recovery_assessment = recovery
        .assess_user_with_source(&user_id, "api")
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let (task_count, pending_count) = replica_task_counts(&state, &user_id, replica_cached).await;
    let integrity_check = match query.integrity {
        Some(mode) => Some(collect_integrity(mode, &replica_dir).await),
        None => None,
    };

    Ok(Json(UserStats {
        user_id,
        replica_cached,
        task_count,
        pending_count,
        replica_dir_exists: dir_exists,
        replica_dir_size_bytes: dir_size,
        quarantined,
        recovery_assessment,
        integrity_check,
    }))
}

async fn replica_task_counts(
    state: &AppState,
    user_id: &str,
    replica_cached: bool,
) -> (Option<usize>, Option<usize>) {
    if !replica_cached {
        return (None, None);
    }

    if let Ok(rep_arc) = state.replica_manager.get_replica(user_id).await {
        let mut replica = rep_arc.lock().await;
        let all = replica.all_tasks().await.ok().map(|tasks| tasks.len());
        let pending = replica.pending_tasks().await.ok().map(|tasks| tasks.len());
        (all, pending)
    } else {
        (None, None)
    }
}

async fn collect_integrity(mode: IntegrityMode, replica_dir: &std::path::Path) -> IntegrityResult {
    let pragma = match mode {
        IntegrityMode::Quick => "PRAGMA quick_check",
        IntegrityMode::Full => "PRAGMA integrity_check",
    };

    let pragma_str = pragma.to_string();
    let replica_result =
        run_integrity_for_path(replica_dir.join("taskchampion.sqlite3"), pragma_str.clone()).await;

    let mut sync_results = Vec::new();
    let shared_sync_db = replica_dir.join("sync.sqlite");
    if shared_sync_db.exists() {
        if let Some(result) = run_integrity_for_path(shared_sync_db, pragma_str).await {
            sync_results.push(format!("sync.sqlite: {result}"));
        }
    }

    IntegrityResult {
        replica: replica_result,
        sync: sync_results,
    }
}

async fn run_integrity_for_path(db_path: std::path::PathBuf, pragma: String) -> Option<String> {
    if !db_path.exists() {
        return None;
    }
    match tokio::task::spawn_blocking(move || run_integrity_check(&db_path, &pragma)).await {
        Ok(result) => result,
        Err(err) => Some(format!("task panicked: {err}")),
    }
}

fn run_integrity_check(db_path: &std::path::Path, pragma: &str) -> Option<String> {
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => return Some(format!("failed to open: {e}")),
    };
    let mut stmt = match conn.prepare(pragma) {
        Ok(s) => s,
        Err(e) => return Some(format!("pragma failed: {e}")),
    };
    let rows: Vec<String> = match stmt.query_map([], |row| row.get::<_, String>(0)) {
        Ok(iter) => iter
            .map(|r| r.unwrap_or_else(|e| format!("row error: {e}")))
            .collect(),
        Err(e) => return Some(format!("pragma query failed: {e}")),
    };
    drop(stmt);
    if rows.is_empty() {
        Some("no output".to_string())
    } else if rows.len() == 1 {
        Some(rows.into_iter().next().unwrap())
    } else {
        Some(rows.join("; "))
    }
}
