use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{extract::State, Json};
use serde::Serialize;
use tokio::sync::RwLock;
use utoipa::ToSchema;

use crate::app_state::AppState;
use crate::metrics as m;

/// Cached health stats, refreshed at most every 30 seconds.
#[derive(Clone)]
pub struct HealthCache {
    inner: Arc<RwLock<CachedStats>>,
}

struct CachedStats {
    pending_tasks: usize,
    replica_count: usize,
    last_refresh: Instant,
}

impl Default for HealthCache {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(CachedStats {
                pending_tasks: 0,
                replica_count: 0,
                last_refresh: Instant::now() - Duration::from_secs(999), // force first refresh
            })),
        }
    }

    /// Get cached stats, refreshing if stale (older than 30s).
    async fn get_or_refresh(&self, state: &AppState) -> (usize, usize) {
        const TTL: Duration = Duration::from_secs(30);

        // Fast path: read lock, check if fresh
        {
            let cached = self.inner.read().await;
            if cached.last_refresh.elapsed() < TTL {
                return (cached.pending_tasks, cached.replica_count);
            }
        }

        // Slow path: write lock, refresh
        let mut cached = self.inner.write().await;

        // Double-check after acquiring write lock (another task may have refreshed)
        if cached.last_refresh.elapsed() < TTL {
            return (cached.pending_tasks, cached.replica_count);
        }

        let (pending, replicas) = count_pending_tasks(state).await;
        cached.pending_tasks = pending;
        cached.replica_count = replicas;
        cached.last_refresh = Instant::now();

        (pending, replicas)
    }
}

/// Count pending tasks from ONLY already-cached replicas.
///
/// Does NOT open replicas for inactive users — prevents FD/memory
/// growth from healthz polling. Disk user count is read with async I/O.
async fn count_pending_tasks(state: &AppState) -> (usize, usize) {
    let mut total_pending: usize = 0;

    // Async directory scan for replica count (no blocking I/O)
    let users_dir = state.data_dir.join("users");
    let replica_count = match tokio::fs::read_dir(&users_dir).await {
        Ok(mut entries) => {
            let mut count = 0;
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.file_type().await.is_ok_and(|ft| ft.is_dir()) {
                    count += 1;
                }
            }
            count
        }
        Err(_) => 0,
    };

    // Only count pending from cached replicas (active users)
    for user_id in state.replica_manager.cached_user_ids() {
        if let Ok(rep_arc) = state.replica_manager.get_replica(&user_id).await {
            let mut rep = rep_arc.lock().await;
            if let Ok(pending) = rep.pending_tasks().await {
                total_pending += pending.len();
            }
        }
    }

    (total_pending, replica_count)
}

#[derive(Serialize, ToSchema)]
#[schema(example = json!({"status": "ok", "pending_tasks": "553"}))]
pub struct HealthResponse {
    pub status: String,
    /// Returned as string for backwards compat with iOS decoder
    pub pending_tasks: String,
}

/// Health check endpoint. Returns server status and pending task count.
///
/// The pending task count is cached for 30 seconds to avoid opening every
/// user's replica on each call. Under load this makes healthz sub-millisecond
/// instead of scaling linearly with user count.
#[utoipa::path(
    get,
    path = "/healthz",
    operation_id = "healthCheck",
    responses(
        (status = 200, description = "Server is healthy", body = HealthResponse)
    ),
    security(()),
    tag = "health"
)]
pub async fn healthz(State(state): State<AppState>) -> Json<HealthResponse> {
    let (pending, replica_dirs) = state.health_cache.get_or_refresh(&state).await;
    m::set_replica_dirs_on_disk(replica_dirs);
    m::set_replica_cached_count(state.replica_manager.replica_count());

    Json(HealthResponse {
        status: "ok".to_string(),
        pending_tasks: pending.to_string(),
    })
}
