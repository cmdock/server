//! Prometheus metrics for observability.
//!
//! Exposes counters, histograms, and gauges on all critical paths.
//! Scraped via GET /metrics in Prometheus exposition format.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

#[derive(Clone, Debug, Default)]
struct DiskMetricPaths {
    data_dir: Option<PathBuf>,
    backup_dir: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FilesystemStats {
    total_bytes: u64,
    free_bytes: u64,
    available_bytes: u64,
    read_only: bool,
}

fn disk_metric_paths() -> &'static RwLock<DiskMetricPaths> {
    static PATHS: OnceLock<RwLock<DiskMetricPaths>> = OnceLock::new();
    PATHS.get_or_init(|| RwLock::new(DiskMetricPaths::default()))
}

/// Configure which filesystem paths should be exposed via scrape-time disk
/// space metrics.
pub fn configure_disk_metrics_paths(data_dir: &Path, backup_dir: Option<&Path>) {
    if let Ok(mut guard) = disk_metric_paths().write() {
        guard.data_dir = Some(data_dir.to_path_buf());
        guard.backup_dir = backup_dir.map(Path::to_path_buf);
    }
}

fn filesystem_stats_for_path(path: &Path) -> std::io::Result<FilesystemStats> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path contains interior NUL: {}", path.display()),
        )
    })?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    let fragment_size = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };

    Ok(FilesystemStats {
        total_bytes: stat.f_blocks.saturating_mul(fragment_size),
        free_bytes: stat.f_bfree.saturating_mul(fragment_size),
        available_bytes: stat.f_bavail.saturating_mul(fragment_size),
        read_only: (stat.f_flag & libc::ST_RDONLY as libc::c_ulong) != 0,
    })
}

fn collect_disk_metrics_for(scope: &'static str, path: &Path) {
    match filesystem_stats_for_path(path) {
        Ok(stats) => {
            gauge!("disk_total_bytes", "scope" => scope).set(stats.total_bytes as f64);
            gauge!("disk_free_bytes", "scope" => scope).set(stats.free_bytes as f64);
            gauge!("disk_available_bytes", "scope" => scope).set(stats.available_bytes as f64);
            gauge!("disk_read_only", "scope" => scope).set(if stats.read_only { 1.0 } else { 0.0 });
        }
        Err(_) => {
            counter!("disk_metric_collection_errors_total", "scope" => scope).increment(1);
        }
    }
}

fn collect_disk_metrics() {
    let paths = disk_metric_paths()
        .read()
        .ok()
        .map(|guard| guard.clone())
        .unwrap_or_default();

    if let Some(data_dir) = paths.data_dir.as_deref() {
        collect_disk_metrics_for("data_dir", data_dir);
    }
    if let Some(backup_dir) = paths.backup_dir.as_deref() {
        collect_disk_metrics_for("backup_dir", backup_dir);
    }
}

/// Initialise the Prometheus metrics recorder and return the handle
/// for serving the /metrics endpoint.
///
/// Safe to call multiple times (e.g. in tests) — returns the same
/// handle each time (OnceLock ensures single initialisation).
///
/// Configures histogram buckets tuned for:
/// - Sub-millisecond SQLite operations (config DB, replica open)
/// - Millisecond-range HTTP requests
/// - Second-range LLM calls
pub fn setup_metrics() -> PrometheusHandle {
    use std::sync::OnceLock;
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

    HANDLE
        .get_or_init(|| {
            let builder = PrometheusBuilder::new()
                // Config DB operations: mostly sub-millisecond
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix("config_db_".to_string()),
                    &[0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0],
                )
                .unwrap()
                // Replica operations: millisecond range with long tail
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix("replica_".to_string()),
                    &[0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0],
                )
                .unwrap()
                // Replica lock wait: same shape as replica ops, but allow slower tails.
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix("replica_lock_".to_string()),
                    &[
                        0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0,
                    ],
                )
                .unwrap()
                // Fine-grained task mutation steps: millisecond range with room for bad tails.
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix("task_mutation_".to_string()),
                    &[
                        0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0,
                    ],
                )
                .unwrap()
                // Filter evaluation: millisecond range
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix("filter_".to_string()),
                    &[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0],
                )
                .unwrap()
                // LLM calls: second range
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix("llm_".to_string()),
                    &[0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0],
                )
                .unwrap()
                // Outbound HTTP calls: second range with room for connect/TLS timeouts
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix("outbound_http_".to_string()),
                    &[0.01, 0.05, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0],
                )
                .unwrap()
                // Sync protocol operations: millisecond range (SQLite I/O)
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix(
                        "sync_operation_duration".to_string(),
                    ),
                    &[0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0],
                )
                .unwrap()
                // Sync payload sizes: byte range (segments ~50B–10KB, snapshots ~1KB–1MB)
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix("sync_body_size".to_string()),
                    &[
                        64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0,
                    ],
                )
                .unwrap()
                // HTTP requests: full range. The 0.6/0.7/0.8/0.9 buckets
                // between 0.5s and 1.0s tighten the interpolation resolution
                // around the p95 budget band. Without them, a p95 anywhere
                // in (0.5, 1.0]s collapses to the 1.0s bucket upper bound
                // after linear interpolation, which masks real regressions
                // and manufactures spurious budget misses for an actual p95
                // near 0.55s. See release issue #76 / Gate 6 investigation.
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix("http_".to_string()),
                    &[
                        0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 5.0,
                        10.0,
                    ],
                )
                .unwrap();

            let handle = builder
                .install_recorder()
                .expect("Failed to install Prometheus recorder");

            // Register process metrics (CPU, memory, open FDs)
            metrics_process::Collector::default().describe();

            handle
        })
        .clone()
}

/// Axum middleware that records per-request HTTP metrics.
///
/// Tracks:
/// - `http_requests_total{method, path, status}` — counter
/// - `http_request_duration_seconds{method, path}` — histogram
/// - `http_requests_in_flight` — gauge
pub async fn metrics_middleware(request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let path = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        // Clamp unmatched paths to avoid high-cardinality label explosion
        .unwrap_or_else(|| "__unmatched__".to_string());

    // Don't track metrics endpoint itself (avoids self-inflating counters)
    if path == "/metrics" {
        return next.run(request).await;
    }

    gauge!("http_requests_in_flight").increment(1.0);
    let start = Instant::now();

    let response = next.run(request).await;

    let duration = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    counter!("http_requests_total", "method" => method.clone(), "path" => path.clone(), "status" => status).increment(1);
    histogram!("http_request_duration_seconds", "method" => method, "path" => path)
        .record(duration);
    gauge!("http_requests_in_flight").decrement(1.0);

    response
}

/// Axum handler for GET /metrics — returns Prometheus exposition format.
pub async fn metrics_handler(State(handle): State<PrometheusHandle>) -> impl IntoResponse {
    // Collect process metrics on each scrape
    metrics_process::Collector::default().collect();
    collect_disk_metrics();
    handle.render()
}

// --- Application-level metric helpers ---
// Call these from handlers and store implementations.

/// Record a config DB operation (query duration + counter).
pub fn record_config_db_op(operation: &'static str, duration_secs: f64) {
    counter!("config_db_queries_total", "operation" => operation).increment(1);
    histogram!("config_db_query_duration_seconds", "operation" => operation).record(duration_secs);
}

/// Record a replica operation (counter + duration).
///
/// `result` should be "ok" or "error". Duration is recorded for both
/// success and error cases. Error latency is also captured by the HTTP
/// request duration histogram at the middleware level.
pub fn record_replica_op(operation: &'static str, duration_secs: f64, result: &'static str) {
    counter!("replica_operations_total", "operation" => operation, "result" => result).increment(1);
    histogram!("replica_operation_duration_seconds", "operation" => operation, "result" => result)
        .record(duration_secs);
}

/// Record time to open a replica.
pub fn record_replica_open(duration_secs: f64) {
    histogram!("replica_open_duration_seconds").record(duration_secs);
}

/// Record time spent waiting to acquire a per-user replica mutex.
pub fn record_replica_lock_wait(operation: &'static str, duration_secs: f64) {
    histogram!("replica_lock_wait_duration_seconds", "operation" => operation)
        .record(duration_secs);
}

/// Record a fine-grained task mutation step duration.
pub fn record_task_mutation_step(operation: &'static str, step: &'static str, duration_secs: f64) {
    histogram!(
        "task_mutation_step_duration_seconds",
        "operation" => operation,
        "step" => step
    )
    .record(duration_secs);
}

/// Record a SQLite BUSY error with the operation that triggered it.
pub fn record_sqlite_busy(operation: &'static str) {
    counter!("sqlite_busy_errors_total", "operation" => operation).increment(1);
}

/// Record a BUSY retry attempt.
pub fn record_busy_retry(operation: &'static str, attempt: usize) {
    counter!("sqlite_busy_retries_total", "operation" => operation).increment(1);
    histogram!("sqlite_busy_retry_attempt").record(attempt as f64);
}

/// Record filter evaluation metrics.
pub fn record_filter_eval(duration_secs: f64, tasks_scanned: usize, tasks_matched: usize) {
    histogram!("filter_evaluation_duration_seconds").record(duration_secs);
    counter!("filter_tasks_scanned_total").increment(tasks_scanned as u64);
    counter!("filter_tasks_matched_total").increment(tasks_matched as u64);
}

/// Record an LLM request.
pub fn record_llm_request(status: &'static str, duration_secs: f64) {
    counter!("llm_requests_total", "status" => status).increment(1);
    histogram!("llm_request_duration_seconds").record(duration_secs);
}

/// Record an LLM fallback to template.
pub fn record_llm_fallback() {
    counter!("llm_fallback_total").increment(1);
}

/// Record the outcome of a server-side outbound HTTP request.
///
/// `target` should stay low-cardinality (for example: "anthropic").
/// `result` should be a small fixed set such as "success", "transport_error",
/// "http_error", "decode_error", or "empty".
pub fn record_outbound_http_request(
    target: &'static str,
    result: &'static str,
    duration_secs: f64,
) {
    counter!("outbound_http_requests_total", "target" => target, "result" => result).increment(1);
    histogram!("outbound_http_request_duration_seconds", "target" => target, "result" => result)
        .record(duration_secs);
}

/// Record an outbound HTTP failure classification.
///
/// `class` should stay low-cardinality, for example:
/// "timeout", "connect", "request", "decode", "empty", "http_4xx", "http_5xx".
pub fn record_outbound_http_failure(target: &'static str, class: &'static str) {
    counter!("outbound_http_failures_total", "target" => target, "class" => class).increment(1);
}

/// Record the outcome of a webhook delivery attempt.
///
/// `status` should stay low-cardinality, for example:
/// "delivered", "http_error", "transport_error", or "ssrf_blocked".
pub fn record_webhook_delivery(event: &str, status: &str, duration_secs: f64) {
    counter!("webhook_deliveries_total", "event" => event.to_string(), "status" => status.to_string())
        .increment(1);
    histogram!(
        "webhook_delivery_duration_seconds",
        "event" => event.to_string(),
        "status" => status.to_string()
    )
    .record(duration_secs);
}

/// Record a webhook scheduler poll run.
pub fn record_webhook_scheduler_run(result: &str, duration_secs: f64) {
    counter!("webhook_scheduler_runs_total", "result" => result.to_string()).increment(1);
    histogram!("webhook_scheduler_run_duration_seconds", "result" => result.to_string())
        .record(duration_secs);
}

/// Record auth cache hit/miss.
pub fn record_auth_cache(result: &'static str) {
    counter!("auth_cache_total", "result" => result).increment(1);
}

/// Record successful authenticated consumption of a connect-config token.
pub fn record_connect_config_consume(result: &'static str) {
    counter!("connect_config_consumes_total", "result" => result).increment(1);
}

/// Record the number of user replica directories on disk.
pub fn set_replica_dirs_on_disk(count: usize) {
    gauge!("replica_dirs_on_disk").set(count as f64);
}

/// Record the number of currently cached (open) replicas.
pub fn set_replica_cached_count(count: usize) {
    gauge!("replica_cached_count").set(count as f64);
}

// --- Sync protocol metric helpers ---

/// Record a sync protocol operation (counter + duration).
///
/// `operation` should be one of: "add_version", "get_child_version",
/// "add_snapshot", "get_snapshot".
/// `result` should be "ok", "conflict", or "error".
pub fn record_sync_op(operation: &'static str, duration_secs: f64, result: &'static str) {
    counter!("sync_operations_total", "operation" => operation, "result" => result).increment(1);
    histogram!("sync_operation_duration_seconds", "operation" => operation, "result" => result)
        .record(duration_secs);
}

/// Record a sync version conflict (409 Conflict responses).
pub fn record_sync_conflict() {
    counter!("sync_conflicts_total").increment(1);
}

/// Record snapshot urgency level signalled to clients.
pub fn record_sync_snapshot_urgency(level: &'static str) {
    counter!("sync_snapshot_urgency_total", "level" => level).increment(1);
}

/// Record the size of a sync protocol payload (history segment or snapshot).
pub fn record_sync_body_size(operation: &'static str, bytes: usize) {
    histogram!("sync_body_size_bytes", "operation" => operation).record(bytes as f64);
}

/// Track sync operations currently in progress (not cached connections —
/// see `sync_storage_cached_count` for that).
pub fn sync_storage_in_flight_inc() {
    gauge!("sync_storage_in_flight").increment(1.0);
}

/// Decrement sync storage in-flight gauge.
pub fn sync_storage_in_flight_dec() {
    gauge!("sync_storage_in_flight").decrement(1.0);
}

/// Record the number of cached sync storage connections.
pub fn set_sync_storage_cached_count(count: usize) {
    gauge!("sync_storage_cached_count").set(count as f64);
}

/// Record a bridge sync job being scheduled.
pub fn record_bridge_sync_enqueue(source: &'static str, priority: &'static str) {
    counter!("bridge_sync_enqueued_total", "source" => source, "priority" => priority).increment(1);
}

/// Record a bridge sync request being coalesced into an already-pending job.
pub fn record_bridge_sync_coalesced(source: &'static str, priority: &'static str) {
    counter!("bridge_sync_coalesced_total", "source" => source, "priority" => priority)
        .increment(1);
}

/// Record a bridge sync run result and duration.
pub fn record_bridge_sync_run(
    source: &'static str,
    priority: &'static str,
    result: &'static str,
    duration_secs: f64,
) {
    counter!(
        "bridge_sync_runs_total",
        "source" => source,
        "priority" => priority,
        "result" => result
    )
    .increment(1);
    histogram!(
        "bridge_sync_run_duration_seconds",
        "source" => source,
        "priority" => priority,
        "result" => result
    )
    .record(duration_secs);
}

/// Record a request blocked by quarantine.
pub fn record_quarantine_blocked(source: &'static str) {
    counter!("quarantine_blocked_total", "source" => source).increment(1);
}

/// Record SQLite corruption detection.
///
/// Labels are `source` (e.g. "api", "sync") and `operation` (e.g. "open_replica",
/// "add_version"). User ID is intentionally excluded to prevent label cardinality
/// explosion — use audit logs for per-user attribution.
pub fn record_sqlite_corruption(source: &'static str, operation: &'static str) {
    counter!("sqlite_corruption_detected_total", "source" => source, "operation" => operation)
        .increment(1);
}

/// Record an operator or startup recovery transition.
pub fn record_recovery_transition(action: &'static str, source: &'static str, changed: bool) {
    counter!(
        "recovery_transitions_total",
        "action" => action,
        "source" => source,
        "changed" => if changed { "true" } else { "false" }
    )
    .increment(1);
}

/// Record a recovery assessment classification.
pub fn record_recovery_assessment(status: &'static str, source: &'static str) {
    counter!(
        "recovery_assessments_total",
        "status" => status,
        "source" => source
    )
    .increment(1);
}

/// Track the current number of offline/quarantined users.
pub fn set_quarantined_user_count(count: usize) {
    gauge!("recovery_quarantined_users").set(count as f64);
}

/// Track the latest startup recovery summary.
pub fn set_startup_recovery_summary(summary: &crate::recovery::StartupRecoverySummary) {
    gauge!("recovery_startup_users_total").set(summary.total_users as f64);
    gauge!("recovery_startup_users_healthy").set(summary.healthy_users as f64);
    gauge!("recovery_startup_users_rebuildable").set(summary.rebuildable_users as f64);
    gauge!("recovery_startup_users_needs_operator_attention")
        .set(summary.needs_operator_attention_users as f64);
    gauge!("recovery_startup_users_already_offline").set(summary.already_offline_users as f64);
    gauge!("recovery_startup_users_newly_offlined").set(summary.newly_offlined_users.len() as f64);
    gauge!("recovery_startup_orphan_user_dirs").set(summary.orphan_user_dirs.len() as f64);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: metrics crate uses a global recorder. These tests verify the
    // helper functions don't panic and produce correct metric names.
    // Integration tests verify the full /metrics endpoint output.

    #[test]
    fn test_record_config_db_op_does_not_panic() {
        record_config_db_op("auth_check", 0.005);
        record_config_db_op("list_views", 0.012);
    }

    #[test]
    fn test_record_replica_op_does_not_panic() {
        record_replica_op("create_task", 0.1, "ok");
        record_replica_op("complete_task", 0.05, "ok");
        record_replica_op("all_tasks", 0.2, "error");
    }

    #[test]
    fn test_record_replica_open_does_not_panic() {
        record_replica_open(0.085);
    }

    #[test]
    fn test_record_replica_lock_wait_does_not_panic() {
        record_replica_lock_wait("create_task", 0.02);
    }

    #[test]
    fn test_record_task_mutation_step_does_not_panic() {
        record_task_mutation_step("create_task", "commit", 0.03);
    }

    #[test]
    fn test_record_sqlite_busy_does_not_panic() {
        record_sqlite_busy("commit");
        record_sqlite_busy("get_task");
    }

    #[test]
    fn test_record_filter_eval_does_not_panic() {
        record_filter_eval(0.23, 553, 7);
    }

    #[test]
    fn test_record_llm_request_does_not_panic() {
        record_llm_request("success", 1.2);
        record_llm_request("error", 0.5);
        record_llm_request("timeout", 15.0);
    }

    #[test]
    fn test_record_llm_fallback_does_not_panic() {
        record_llm_fallback();
    }

    #[test]
    fn test_record_outbound_http_metrics_do_not_panic() {
        record_outbound_http_request("anthropic", "success", 0.8);
        record_outbound_http_request("anthropic", "transport_error", 1.5);
        record_outbound_http_failure("anthropic", "connect");
        record_outbound_http_failure("anthropic", "http_5xx");
    }

    #[test]
    fn test_record_connect_config_consume_does_not_panic() {
        record_connect_config_consume("first_use");
        record_connect_config_consume("repeat_use");
    }

    #[test]
    fn test_set_replica_counts_does_not_panic() {
        set_replica_dirs_on_disk(0);
        set_replica_dirs_on_disk(5);
        set_replica_cached_count(0);
        set_replica_cached_count(3);
    }

    #[test]
    fn test_record_sync_op_does_not_panic() {
        record_sync_op("add_version", 0.005, "ok");
        record_sync_op("add_version", 0.01, "conflict");
        record_sync_op("get_child_version", 0.002, "ok");
        record_sync_op("add_snapshot", 0.05, "error");
        record_sync_op("get_snapshot", 0.003, "ok");
    }

    #[test]
    fn test_record_sync_conflict_does_not_panic() {
        record_sync_conflict();
    }

    #[test]
    fn test_record_sync_snapshot_urgency_does_not_panic() {
        record_sync_snapshot_urgency("low");
        record_sync_snapshot_urgency("high");
    }

    #[test]
    fn test_record_sync_body_size_does_not_panic() {
        record_sync_body_size("add_version", 1024);
        record_sync_body_size("add_snapshot", 65536);
    }

    #[test]
    fn test_sync_storage_in_flight_does_not_panic() {
        sync_storage_in_flight_inc();
        sync_storage_in_flight_dec();
    }

    #[test]
    fn test_set_sync_storage_cached_count_does_not_panic() {
        set_sync_storage_cached_count(0);
        set_sync_storage_cached_count(5);
    }

    #[test]
    fn test_record_sqlite_corruption_does_not_panic() {
        record_sqlite_corruption("api", "open_replica");
        record_sqlite_corruption("sync", "add_version");
    }

    #[test]
    fn test_recovery_metric_helpers_do_not_panic() {
        record_recovery_transition("offline", "api", true);
        record_recovery_transition("online", "cli", false);
        record_recovery_assessment("healthy", "api");
        record_recovery_assessment("needs_operator_attention", "startup");
        set_quarantined_user_count(2);
        set_startup_recovery_summary(&crate::recovery::StartupRecoverySummary {
            total_users: 5,
            healthy_users: 3,
            rebuildable_users: 1,
            needs_operator_attention_users: 1,
            already_offline_users: 1,
            newly_offlined_users: vec!["user1".to_string()],
            orphan_user_dirs: vec![],
        });
    }

    #[test]
    fn test_filesystem_stats_for_path_returns_sane_values() {
        let tmp = tempfile::tempdir().unwrap();
        let stats = filesystem_stats_for_path(tmp.path()).unwrap();
        assert!(stats.total_bytes > 0);
        assert!(stats.free_bytes <= stats.total_bytes);
        assert!(stats.available_bytes <= stats.total_bytes);
    }
}
