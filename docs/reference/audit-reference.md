# Audit Reference

This document explains the server's structured audit logging model and how it
relates to the separate cross-repo boundary-event log stream.

It is not a general logging guide. Use it when reasoning about:

- which actions produce audit events
- what fields those events carry
- the distinction between audit, metrics, and regular logs
- current audit gaps and future extension points

## 1. Audit Layer Model

Audit events are ordinary `tracing` events emitted with:

- `target = "audit"`

The runtime installs a dedicated subscriber layer that captures only that
target and writes line-delimited JSON.

Cross-repo contract events are different:

- they use `target = "boundary"`
- they stay on the normal application log stream
- they are not filtered through the dedicated audit sink

Current output modes:

- `stderr`
- `stdout` (legacy alias, still written to stderr internally)
- append-only file path

When audit is enabled and a file path is used, the file is opened with
restrictive `0600` permissions on Unix.

## 2. Why Audit Is Separate

The server uses three different observability channels for different purposes:

- regular application logs
  - debugging, warnings, runtime detail
- boundary logs
  - cross-repo flow diagnosis with correlation IDs
- metrics
  - aggregation, alerting, latency/volume trends
- audit events
  - who did what, through which surface, against which user/device

Audit exists for accountability and operator traceability, not for performance
analysis.

In practice, that also means audit should be suitable for off-host retention in
SIEM/log-management systems. Security-relevant state changes should not depend
solely on local server logs that may be rotated away or lost with the host.

Boundary logs exist for a different reason:

- reconstructing cross-repo flows with grep-able correlation IDs
- matching client and server sides of a boundary crossing
- satisfying the architecture-level observability contract

For the connect-config flow specifically:

- the primary correlation ID is now the non-secret payload `token_id`
- the truncated credential hash prefix remains a compatibility fallback during
  rollout, not the preferred correlation key

## 3. Event Shape

Audit events are JSON records emitted by `tracing_subscriber`'s JSON formatter.
There is no separate bespoke audit schema object yet, but common fields recur
throughout the codebase.

Common fields:

- `action`
- `source`
- `user_id`
- `client_ip`

Common optional fields:

- `client_id`
- `username`
- `label`
- `device_name`
- `operation`
- `reason`
- `was_quarantined`

The meaning of the important common fields is:

- `action`
  - the semantic action name, for example `user.create`,
    `device.register`, or `admin.user.quarantine`
- `source`
  - where the action originated, currently usually `api` or `cli`
- `client_ip`
  - remote address derived from forwarding headers for HTTP calls, or `local`
    for CLI-originated actions

## 4. Implemented Audit Categories

Current audit coverage is strongest in these areas:

- user and token lifecycle
- task mutation surfaces
- view/config/app-config mutations
- device lifecycle
- admin offline/online actions
- authentication failures
- corruption-triggered quarantine
- TaskChampion sync writes and device activity

The required cross-repo boundary events are not primarily audit events in this
repo. They are structured `target="boundary"` events documented in
[Metrics and Observability Reference](metrics-and-observability-reference.md).

Representative examples:

- `user.create`
- `user.delete`
- `token.create`
- `auth.failure`
- `device.register`
- `device.revoke`
- `admin.user.quarantine`
- `admin.user.unquarantine`
- `replica.corruption_detected`

## 5. Source Semantics

`source` matters because the same logical action may be available through more
than one control surface.

Current common values:

- `api`
  - request came through an HTTP endpoint
- `cli`
  - request came from the local admin CLI
- `startup`
  - action was triggered automatically during server boot-time recovery

That distinction is important for:

- operator attribution
- understanding whether an action happened online or offline
- future admin UI or operator tooling

## 6. Recovery and Audit

Recovery-related auditing currently has two layers:

- explicit audit events for recovery transitions emitted by the recovery service
- structured application logs for startup assessment summaries

Implemented recovery/offline audit behaviour:

- `admin user offline` emits an audit event
- `admin user online` emits an audit event
- corruption-triggered quarantine emits an audit event
- startup recovery auto-offlining now emits an audit event with:
  - `action = "admin.user.quarantine"`
  - `source = "startup"`
  - `reason = "startup_recovery_assessment"`
  - `changed = true`

That keeps the semantic action aligned with manual offline, while still making
automatic startup-driven quarantine distinguishable in the audit stream.

This is especially important for cybersecurity and incident review:

- a host restart should not hide the fact that the server came up and
  immediately isolated a user
- exported audit streams should preserve these events even if local logs are
  unavailable later

## 7. What Is Intentionally Not Audited

Not everything should become an audit record.

Examples that belong primarily in logs and metrics instead:

- high-frequency bridge scheduling internals
- cache hits and misses
- load-test traffic volume
- routine health checks
- low-level SQLite busy retries

Audit should stay focused on semantically meaningful state changes and security
relevant actions.

## 8. Audit vs Metrics

Use audit when you need:

- which user was affected
- which device was affected
- whether the CLI or API performed the action
- a durable trail of operator/security events

Use metrics when you need:

- how often something happens
- latency distributions
- error rates
- queue depth and contention trends

In practice:

- `device.revoke` belongs in audit
- `sync_auth_failures_total` would belong in metrics

## 9. Operational Notes

If audit is enabled but the configured output cannot be opened, the server
fails fast at startup rather than silently running without the requested audit
sink.

CLI audit works through the same subscriber setup as the server process, so
local admin actions can be captured consistently when audit is enabled.

For stronger security posture, treat local audit output as a short-term source
only. Ship audit off the server for retention and correlation.

## 10. Future Work

Likely next improvements:

- a more formal event taxonomy table
- clearer event expectations for future remote admin APIs
- documentation of any retention/export guidance once deployment patterns
  stabilize
