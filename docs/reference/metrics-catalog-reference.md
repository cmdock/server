# Metrics Catalog Reference

This document is the server-local catalog of application metrics emitted by
`cmdock/server`.

Use it with:

- `GET /metrics` for the live Prometheus exposition surface
- [Metrics and Observability Reference](metrics-and-observability-reference.md)
  for interpretation and alerting guidance

This is the closest thing the server currently has to an OpenAPI-style metrics
contract. It is a human-maintained catalog derived from the code paths in
[`src/metrics.rs`](../../src/metrics.rs).

## 1. Discovery Model

Metrics are currently discoverable through:

- the live Prometheus endpoint at `GET /metrics`
- this reference catalog
- the helper definitions in [`src/metrics.rs`](../../src/metrics.rs)

The server does not currently expose a separate machine-readable metrics schema
or registry endpoint.

## 2. Scope

This catalog covers server-owned application metrics.

It does not attempt to fully catalog:

- generic process metrics emitted by `metrics_process`
- ingress-proxy metrics such as Caddy or load-balancer telemetry
- browser-side metrics from the operator console
- operator tooling metrics from shell scripts or deploy helpers

## 3. HTTP Metrics

### `http_requests_total`

- Type: counter
- Labels:
  - `method`
  - `path`
  - `status`
- Meaning:
  - total inbound HTTP requests handled by the server, excluding `/metrics`

### `http_request_duration_seconds`

- Type: histogram
- Labels:
  - `method`
  - `path`
- Meaning:
  - end-to-end request latency seen by the Axum middleware

### `http_requests_in_flight`

- Type: gauge
- Labels: none
- Meaning:
  - number of currently executing inbound HTTP requests

## 4. Config DB And Auth Metrics

### `config_db_queries_total`

- Type: counter
- Labels:
  - `operation`
- Meaning:
  - config-store query/update operations explicitly instrumented by server code

### `config_db_query_duration_seconds`

- Type: histogram
- Labels:
  - `operation`
- Meaning:
  - duration of instrumented config-store operations

### `auth_cache_total`

- Type: counter
- Labels:
  - `result`
- Meaning:
  - bearer-auth cache hits and misses

Current `result` values:

- `hit`
- `miss`

### `connect_config_consumes_total`

- Type: counter
- Labels:
  - `result`
- Meaning:
  - successful authenticated use of a short-lived connect-config token

Current `result` values include:

- `first_use`
- `repeat_use`

Operator note:

- the durable per-token troubleshooting state for connect-config lives in the
  config DB rather than a metric series
- `admin token list <user-id>` surfaces the token's `FIRST_USED`, `LAST_USED`,
  and `LAST_IP` fields for per-user diagnosis

## 5. Replica And SQLite Metrics

### `replica_operations_total`

- Type: counter
- Labels:
  - `operation`
  - `result`
- Meaning:
  - REST-side replica reads and writes performed through the task surface

Current `operation` values include:

- `all_tasks`
- `pending_tasks`
- `create_task`
- `complete_task`
- `undo_task`
- `delete_task`
- `modify_task`

Current `result` values:

- `ok`
- `error`

### `replica_operation_duration_seconds`

- Type: histogram
- Labels:
  - `operation`
  - `result`
- Meaning:
  - duration of the replica operations listed above

### `replica_open_duration_seconds`

- Type: histogram
- Labels: none
- Meaning:
  - time to open a user replica

### `replica_dirs_on_disk`

- Type: gauge
- Labels: none
- Meaning:
  - number of user replica directories on disk

### `replica_cached_count`

- Type: gauge
- Labels: none
- Meaning:
  - number of currently cached/open replicas

### `disk_total_bytes`

- Type: gauge
- Labels:
  - `scope`
- Meaning:
  - total filesystem capacity for a configured server-owned path such as
    `data_dir` or `backup_dir`

### `disk_free_bytes`

- Type: gauge
- Labels:
  - `scope`
- Meaning:
  - total free filesystem bytes for the configured path's underlying
    filesystem, including blocks only root may use

### `disk_available_bytes`

- Type: gauge
- Labels:
  - `scope`
- Meaning:
  - filesystem bytes available to the current process for the configured path's
    underlying filesystem

This is the most useful metric for low-space alerting on self-hosted systems.

### `disk_read_only`

- Type: gauge
- Labels:
  - `scope`
- Meaning:
  - `1` when the configured path's underlying filesystem is mounted read-only,
    otherwise `0`

### `disk_metric_collection_errors_total`

- Type: counter
- Labels:
  - `scope`
- Meaning:
  - scrape-time failures while collecting filesystem capacity metrics for a
    configured server-owned path

### `sqlite_busy_errors_total`

- Type: counter
- Labels:
  - `operation`
- Meaning:
  - SQLite `BUSY` errors encountered by instrumented retry paths

### `sqlite_busy_retries_total`

- Type: counter
- Labels:
  - `operation`
- Meaning:
  - count of retry attempts after `BUSY`

### `sqlite_busy_retry_attempt`

- Type: histogram
- Labels: none
- Meaning:
  - retry-attempt number recorded for `BUSY` retry loops

### `sqlite_corruption_detected_total`

- Type: counter
- Labels:
  - `source`
  - `operation`
- Meaning:
  - corruption or not-a-database detection at server-owned runtime seams

## 6. Filter Metrics

### `filter_evaluation_duration_seconds`

- Type: histogram
- Labels: none
- Meaning:
  - task-filter evaluation time for the native filter engine

### `filter_tasks_scanned_total`

- Type: counter
- Labels: none
- Meaning:
  - total tasks scanned during instrumented filter evaluations

### `filter_tasks_matched_total`

- Type: counter
- Labels: none
- Meaning:
  - total tasks matched during instrumented filter evaluations

## 7. Summary And Outbound HTTP Metrics

### `llm_requests_total`

- Type: counter
- Labels:
  - `status`
- Meaning:
  - high-level result of LLM summary attempts

Current `status` values include:

- `success`
- `error`
- `empty`

### `llm_request_duration_seconds`

- Type: histogram
- Labels: none
- Meaning:
  - total duration of an LLM summary attempt

### `llm_fallback_total`

- Type: counter
- Labels: none
- Meaning:
  - number of times summary generation fell back to the template path

### `outbound_http_requests_total`

- Type: counter
- Labels:
  - `target`
  - `result`
- Meaning:
  - result of server-owned outbound HTTP calls

Current `target` values:

- `anthropic`

Current `result` values include:

- `success`
- `transport_error`
- `http_error`
- `decode_error`
- `empty`

### `outbound_http_request_duration_seconds`

- Type: histogram
- Labels:
  - `target`
  - `result`
- Meaning:
  - duration of server-owned outbound HTTP calls

### `outbound_http_failures_total`

- Type: counter
- Labels:
  - `target`
  - `class`
- Meaning:
  - failure classification for server-owned outbound HTTP calls

Current `class` values include:

- `timeout`
- `connect`
- `decode`
- `body`
- `redirect`
- `builder`
- `request`
- `transport`
- `http_4xx`
- `http_5xx`
- `http_other`
- `empty`

### `webhook_deliveries_total`

- Type: counter
- Labels:
  - `event`
  - `status`
- Meaning:
  - outcome of webhook delivery attempts, including retries and SSRF-blocked attempts

Current `status` values include:

- `delivered`
- `http_error`
- `transport_error`
- `ssrf_blocked`

### `webhook_delivery_duration_seconds`

- Type: histogram
- Labels:
  - `event`
  - `status`
- Meaning:
  - duration of webhook delivery attempts

Notes:

- `ssrf_blocked` attempts record zero duration because they are rejected before any outbound connection is attempted.
- `event` uses the webhook event name such as `task.created`, `task.modified`, `task.due`, `task.overdue`, or `webhook.test`.

### `webhook_scheduler_runs_total`

- Type: counter
- Labels:
  - `result`
- Meaning:
  - outcome of webhook scheduler poll runs

Current `result` values include:

- `ok`
- `error`

### `webhook_scheduler_run_duration_seconds`

- Type: histogram
- Labels:
  - `result`
- Meaning:
  - duration of webhook scheduler poll runs

## 8. Sync Protocol Metrics

### `sync_operations_total`

- Type: counter
- Labels:
  - `operation`
  - `result`
- Meaning:
  - TaskChampion sync endpoint operations

Current `operation` values:

- `add_version`
- `get_child_version`
- `add_snapshot`
- `get_snapshot`

Current `result` values include:

- `ok`
- `conflict`
- `error`

### `sync_operation_duration_seconds`

- Type: histogram
- Labels:
  - `operation`
  - `result`
- Meaning:
  - latency of sync protocol operations

### `sync_conflicts_total`

- Type: counter
- Labels: none
- Meaning:
  - total sync conflicts returned to clients

### `sync_snapshot_urgency_total`

- Type: counter
- Labels:
  - `level`
- Meaning:
  - urgency levels signalled to sync clients

### `sync_body_size_bytes`

- Type: histogram
- Labels:
  - `operation`
- Meaning:
  - payload size for sync request/response bodies

### `sync_storage_in_flight`

- Type: gauge
- Labels: none
- Meaning:
  - sync storage operations currently in progress

### `sync_storage_cached_count`

- Type: gauge
- Labels: none
- Meaning:
  - cached sync storage handles/connections

## 9. Sync Bridge Metrics

### `bridge_sync_enqueued_total`

- Type: counter
- Labels:
  - `source`
  - `priority`
- Meaning:
  - bridge sync jobs scheduled for later execution

### `bridge_sync_coalesced_total`

- Type: counter
- Labels:
  - `source`
  - `priority`
- Meaning:
  - bridge sync requests folded into an already-pending job

### `bridge_sync_runs_total`

- Type: counter
- Labels:
  - `source`
  - `priority`
  - `result`
- Meaning:
  - completed bridge sync runs by outcome

### `bridge_sync_run_duration_seconds`

- Type: histogram
- Labels:
  - `source`
  - `priority`
  - `result`
- Meaning:
  - duration of bridge sync runs

## 10. Quarantine And Recovery Metrics

### `quarantine_blocked_total`

- Type: counter
- Labels:
  - `source`
- Meaning:
  - requests blocked because a user is quarantined/offline

### `recovery_transitions_total`

- Type: counter
- Labels:
  - `action`
  - `source`
  - `changed`
- Meaning:
  - operator or startup transitions into or out of offline/quarantine state

### `recovery_assessments_total`

- Type: counter
- Labels:
  - `status`
  - `source`
- Meaning:
  - recovery assessment classifications emitted by the recovery service

### `recovery_quarantined_users`

- Type: gauge
- Labels: none
- Meaning:
  - current in-memory quarantined/offline user count

### `recovery_startup_users_total`

- Type: gauge
- Labels: none
- Meaning:
  - total users examined during the latest startup recovery assessment

### `recovery_startup_users_healthy`

- Type: gauge
- Labels: none
- Meaning:
  - healthy users in the latest startup recovery assessment

### `recovery_startup_users_rebuildable`

- Type: gauge
- Labels: none
- Meaning:
  - rebuildable users in the latest startup recovery assessment

### `recovery_startup_users_needs_operator_attention`

- Type: gauge
- Labels: none
- Meaning:
  - users requiring operator attention in the latest startup assessment

### `recovery_startup_users_already_offline`

- Type: gauge
- Labels: none
- Meaning:
  - users already offline before the latest startup assessment

### `recovery_startup_users_newly_offlined`

- Type: gauge
- Labels: none
- Meaning:
  - users newly placed offline during the latest startup assessment

### `recovery_startup_orphan_user_dirs`

- Type: gauge
- Labels: none
- Meaning:
  - orphan user directories found during the latest startup assessment

## 11. Process Metrics

The Prometheus endpoint also includes generic process metrics from
`metrics_process`, for example:

- `process_cpu_seconds_total`
- `process_resident_memory_bytes`
- `process_threads`

Treat those as runtime/environment metrics rather than application-contract
metrics owned by this catalog.
