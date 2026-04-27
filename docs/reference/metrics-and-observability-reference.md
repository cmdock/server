# Metrics and Observability Reference

This document describes the current observability model of the server.

For operator monitoring workflows, see
[Administration Guide](../manuals/administration-guide.md).

## 1. Purpose

Observability in this server is meant to answer:

- is the HTTP surface healthy?
- is SQLite healthy?
- is sync under pressure?
- are bridge and recovery paths behaving as expected?

## 2. Main Signals

The server currently uses a mix of:

- Prometheus-style metrics
- structured audit events
- application logs

Each serves a different purpose.

Cross-repo boundary events required by `cmdock/architecture` ADR-0008 and
`docs/observability-contract.md` are emitted as structured tracing events on
the normal application log stream with:

- `target = "boundary"`
- `event = "<logical boundary event name>"`

This repo does not use metrics as the primary transport for those contract
events. Metrics are supporting aggregate signals only.

## 3. Metrics Categories

Important categories include:

- HTTP request metrics
- disk capacity and filesystem writability metrics for `data_dir` and
  `backup_dir`
- connect-config onboarding success metrics
- outbound HTTP egress metrics
- auth metrics
- replica operation metrics
- sync operation metrics
- body size and conflict metrics
- quarantine/corruption metrics
- bridge scheduler metrics
- recovery assessment and transition metrics

## 4. Audit vs Metrics

Use metrics for:

- aggregate behaviour
- trend detection
- alert thresholds

Use audit logs for:

- specific operator actions
- security-relevant state changes
- who/what/when event trails

For security-sensitive deployments, assume those audit trails should be shipped
off the server for retention and incident review rather than treated as
host-local only.

Boundary-event note:

- contract boundary events use `target = "boundary"` on the normal structured
  log stream so they are emitted even when the dedicated audit sink is
  disabled
- audit records remain the durable trail for operator/security actions
- some flows deliberately emit both:
  - a boundary event for cross-repo diagnosis
  - an audit record for operator accountability

## 5. Cross-Repo Boundary Mapping

The server-side logical boundary names currently map to these implementation
seams:

| Logical event | Current implementation in `cmdock/server` |
|---------------|-------------------------------------------|
| `connect_config.token_issued` | `target="boundary"` from `admin connect-config create` in `src/admin/cli/connect_config.rs` |
| `connect_config.token_redeemed` | `target="boundary"` on first successful authenticated use of a connect-config token in `src/auth/middleware.rs` |
| `connect_config.token_exchange_rejected` | `target="boundary"` from bearer-auth rejection paths for connect-config tokens in `src/auth/middleware.rs` |
| `connection.established` | `target="boundary"` on first successful authenticated use of a connect-config token in `src/auth/middleware.rs` |
| `sync.request_received` | `target="boundary"` at each `/v1/client/*` handler entry in `src/tc_sync/handlers.rs` |
| `sync.complete` | `target="boundary"` on successful `/v1/client/*` completion in `src/tc_sync/handlers.rs` |
| `sync.failed` | `target="boundary"` on failed `/v1/client/*` completion in `src/tc_sync/handlers.rs` |
| `queue.mutation_accepted` | `target="boundary"` on successful task mutation handlers in `src/tasks/handlers.rs` when queue correlation headers are present |
| `queue.mutation_rejected` | `target="boundary"` on rejected task mutation handlers in `src/tasks/handlers.rs` when queue correlation headers are present |
| `queue.mutation_conflicted` | `target="boundary"` on `409` task mutation conflicts in `src/tasks/handlers.rs` when queue correlation headers are present |

Correlation fields currently used by the server:

- `correlation_id`
  - connect-config: non-secret `token_id`
  - sync: `X-Request-ID`
  - queue replay: `X-Session-ID`
- `request_id`
  - copied from `X-Request-ID` when present
- `session_id`
  - copied from `X-Session-ID` for queue replay events
- `mutation_id`
  - copied from `X-Mutation-ID` for queue replay events
- `credential_hash_prefix`
  - first 8 hex chars of the credential SHA-256 for compatibility fallback
    correlation when a client or payload does not yet surface `token_id`

## 5. Recovery-Relevant Signals

Operators and developers should pay special attention to:

- quarantine/offline blocking
- corruption detection
- bridge failures or repeated timeouts
- sync contention / busy signals
- startup recovery actions that automatically place users offline

These are the signals that tend to show:

- damaged files
- pathological load
- bad recovery sequencing
- mis-sized shared-device workloads

## 6. Performance-Relevant Signals

The most useful performance-oriented signals are currently:

- request latency
- remaining disk headroom on server-owned paths
- outbound provider connectivity and latency
- sync latency
- conflict counts
- body sizes
- bridge pressure / retries

These tell you more about system shape than raw request counts alone.

For storage-pressure detection specifically, prefer the server-owned scrape-time
gauges:

- `disk_available_bytes{scope="data_dir"}`
- `disk_available_bytes{scope="backup_dir"}`
- `disk_read_only{scope="data_dir|backup_dir"}`
- `disk_metric_collection_errors_total{scope="data_dir|backup_dir"}`

These answer slightly different questions:

- `data_dir`
  - can the server keep writing config, replica, WAL, and sync state?
- `backup_dir`
  - can backup and restore staging still proceed safely?

For alerting, `disk_available_bytes` is usually the best primary signal because
it reflects bytes available to the running process rather than raw filesystem
free blocks.

## 7. What Is Intentionally Low-Volume

Some surfaces do not need deep metric expansion unless the product model
changes:

- device registry CRUD
- admin CLI actions
- low-frequency operator lifecycle actions

Those are better captured primarily via audit and integration tests unless they
become a hot operator path later.

## 8. Where to Extend Metrics Next

The most likely future observability extensions are:

- richer bridge scheduler visibility
- clearer breakdown of sync auth failures
- deeper restore/repair progress metrics

Current outbound seam note:

- the server has only one runtime outbound HTTP dependency today: the Anthropic
  summary client
- browser-side operator-console fetches are not server-process egress
- Taskwarrior sync is an inbound protocol surface, not server egress
- ACME/TLS traffic belongs to Caddy or the external ingress layer, not this
  server process

The Anthropic seam now exposes:

- `outbound_http_requests_total{target,result}`
- `outbound_http_request_duration_seconds{target,result}`
- `outbound_http_failures_total{target,class}`

Use those metrics to answer:

- can the server reach the provider at all?
- are outbound calls timing out or failing to connect?
- are failures transport-level or HTTP response-level?
- is fallback usage driven by provider reachability or by server-side policy?

Connect-config onboarding now exposes a small troubleshooting signal:

- `connect_config_consumes_total{result=...}`
  - the important value is `result="first_use"`, which means a short-lived
    connect-config token completed at least one successful authenticated API
    request
- the token row itself also records:
  - `first_used_at`
  - `last_used_at`
  - `last_used_ip`

Use that split deliberately:

- metrics answer "is QR/deep-link onboarding working at all?"
- token usage state answers "did this specific user/token ever succeed?"

Already implemented recovery-facing metrics now include:

- `recovery_transitions_total`
  - labelled by `action=offline|online`
  - labelled by `source=cli|api|startup`
  - includes whether the transition actually changed runtime state
- `recovery_assessments_total`
  - labelled by recovery classification
  - currently emitted from the recovery service boundary
- `recovery_quarantined_users`
  - current in-memory offline/quarantine count
- `recovery_startup_users_*`
  - latest startup assessment summary gauges

## 9. Alerting Guidance

Recovery and recovery-adjacent signals should not all be treated the same.

### Page-worthy signals

These should normally page because they indicate service loss, likely data
damage, or unexpected security-relevant state changes:

- `recovery_transitions_total{action="offline",source="startup",changed="true"}`
  - unexpected startup auto-offline
- `recovery_quarantined_users > 0`
  - only page when not in a planned maintenance/recovery window
- `sqlite_corruption_detected_total`
  - page on any increase
- repeated `quarantine_blocked_total`
  - page when traffic is being actively rejected because a user stayed offline
- `disk_read_only{scope="data_dir"} == 1`
  - page immediately; normal writes cannot succeed safely
- `disk_available_bytes{scope="data_dir"}` below emergency write headroom
  - page before runtime write failures cascade

### Warning-only signals

These usually indicate operator follow-up or capacity tuning, but not an
immediate emergency by themselves:

- `recovery_assessments_total{status="rebuildable"}`
  - working state is degraded but not necessarily unsafe
- `recovery_startup_users_rebuildable > 0`
  - the server came up in a recoverable but degraded state
- `sqlite_busy_errors_total`
  - warning first; page only if sustained and user-facing latency is also bad
- bridge retry / timeout growth without quarantine or corruption
- `disk_available_bytes{scope="backup_dir"}` below planned backup/restore
  staging headroom
- `disk_metric_collection_errors_total`
  - warning first; the monitoring signal itself is degraded

### Suppress or downgrade during planned recovery work

During an intentional selective restore, operator-driven offline window, or
maintenance exercise, suppress or downgrade:

- `recovery_quarantined_users`
- `recovery_transitions_total{action="offline"}`
- `quarantine_blocked_total`

Those signals are still useful for dashboards and post-incident review, but
they should not create noisy pages when the operator explicitly caused them.

### Use metrics and audit together

Recommended split:

- metrics answer "is recovery state unhealthy right now?"
- audit answers "which user was affected, by whom, and why?"

For cybersecurity-sensitive deployments, a startup auto-offline should be
treated as both:

- an operational page candidate
- a security-relevant audit event for off-host retention

## 10. Boundary Rule For Refactors

Observability should not become the next coupling hub.

Default rule:

- domain services should return meaningful results and domain errors
- orchestrators and surfaces should attach most audit/metrics emission
- only keep observability inside a service when the metric or audit event is
  genuinely part of that service's domain contract

Recovery is a deliberate exception now:

- recovery transition audit and recovery metrics are emitted from the
  `RecoveryCoordinator`
- the runtime recovery layer maintains only the current quarantine gauge,
  because it owns the authoritative in-memory state

This matters especially for:

- admin/recovery refactors
- sync/runtime coordination
- future remote admin and operator tooling work

## 11. Related Docs

- [Metrics Catalog Reference](metrics-catalog-reference.md)
- [Performance and Scaling Guide](../manuals/performance-and-scaling-guide.md)
- [Administration Guide](../manuals/administration-guide.md)
- [Recovery Reference](recovery-reference.md)
- [Sync Bridge Reference](sync-bridge-reference.md)
