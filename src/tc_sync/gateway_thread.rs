//! Dedicated OS-thread adapters for `!Send` merged-gateway work.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;

use crate::app_state::AppState;
use crate::merged_sync_gateway::inbound::{
    add_personal_version, GatewayAddVersionOutcome, GatewayVersion,
};
use crate::merged_sync_gateway::projection::project_personal_now;

/// Timeout for merged gateway work dispatched to a dedicated OS thread.
///
/// Uses the same `CMDOCK_SYNC_TIMEOUT` contract as the legacy sync bridge: the
/// HTTP caller returns promptly while any orphaned OS thread finishes in the
/// background and releases its locks naturally.
fn merged_gateway_thread_timeout() -> Duration {
    static TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
        let secs = std::env::var("CMDOCK_SYNC_TIMEOUT")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(5)
            .max(1);
        Duration::from_secs(secs)
    });
    *TIMEOUT
}

// TaskChampion replica/gateway futures are `!Send`, so the Axum handler
// crosses into a dedicated current-thread runtime on a plain OS thread. Phase
// 12 Batch 2 review accepted this per-request thread boundary for day-one sync;
// revisit with a small per-user projection semaphore only if staging/load data
// shows multi-device read bursts creating material thread pressure.
async fn receive_gateway_thread_result<T>(
    rx: tokio::sync::oneshot::Receiver<Result<T>>,
    operation: &'static str,
) -> Result<T> {
    let timeout = merged_gateway_thread_timeout();
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => Err(anyhow::anyhow!(
            "merged gateway {operation} thread dropped result: {err}"
        )),
        Err(_) => {
            tracing::warn!(
                operation,
                timeout_secs = timeout.as_secs(),
                "merged gateway OS thread timed out; background worker will release locks on completion"
            );
            Err(anyhow::anyhow!(
                "merged gateway {operation} timed out after {}s",
                timeout.as_secs()
            ))
        }
    }
}

pub(super) async fn run_gateway_add_personal_version(
    state: AppState,
    version: GatewayVersion,
) -> Result<GatewayAddVersionOutcome> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| anyhow::anyhow!("start merged gateway runtime: {err}"))
            .and_then(|rt| rt.block_on(add_personal_version(&state, version)));
        let _ = tx.send(result);
    });
    receive_gateway_thread_result(rx, "add_personal_version").await
}

pub(super) async fn run_personal_projection(state: AppState, user_id: String) -> Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| anyhow::anyhow!("start merged projection runtime: {err}"))
            .and_then(|rt| {
                rt.block_on(project_personal_now(&state, &user_id))
                    .map(|_| ())
            });
        let _ = tx.send(result);
    });
    receive_gateway_thread_result(rx, "personal_projection").await
}
