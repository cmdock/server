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
- task read-surface validation failures (#109)
- view/config/app-config mutations
- device lifecycle
- admin offline/online actions
- authentication failures
- corruption-triggered quarantine
- TaskChampion sync writes and device activity
- merged-sync gateway version lifecycle, source apply, correction, and quarantine

The required cross-repo boundary events are not primarily audit events in this
repo. They are structured `target="boundary"` events documented in
[Metrics and Observability Reference](metrics-and-observability-reference.md).

Representative examples:

- `user.create`
- `user.delete`
- `token.create`
- `account.prefix_set` — emitted after `users.prefix` is durably assigned by
  signup/bootstrap/operator/backfill flows. Carries `source`, `user_id`, and
  `prefix`.
- `account.task_scope_ensured` — emitted by prefix-assignment flows and
  boot-time Personal Task Scope backfill after the user's Personal Task Scope
  has been materialised. Carries `source`, `user_id`, `task_scope_id`, `prefix`,
  and `result` (`created` or `existing`); backfill also includes `username`.
  `result = "created"` is the first-materialisation discriminator; there is no
  separate created audit action.
- `auth.failure`
- `device.register` — emitted by `POST /admin/user/{id}/devices`. Carries
  `source`, `authority = "admin_http_token"`, `client_ip`, `request_id`,
  `user_id`, `sync_identity` (canonical sync identity), `client_id`,
  `device_name`, `outcome = "success"`, and `reason = "first_provisioning"`.
- `device.rotate` — emitted by `POST /admin/user/{id}/devices/{id}/rotate`. This
  is the ADR-0012 gate-6 lost-response **rotation recovery** audit entry, and it
  carries the full required field set:
  - `timestamp` — added by the JSON audit layer
  - `authority = "admin_http_token"` — operator authority identifier. The admin
    HTTP surface authenticates a single shared operator bearer token
    (`[admin].http_token` / `CMDOCK_ADMIN_TOKEN`) via `OperatorAuth`, which is
    structurally distinct from any user API token or the rotated/lost sync
    credential. Because the token is shared, the identifier is the authority
    *type*; `client_ip` carries the source address that distinguishes operators
    in practice.
  - `user_id` — Runtime User
  - `sync_identity` — the user's canonical sync identity (distinct from the
    per-device binding below)
  - `old_client_id` (revoked device) + `client_id` (new replacement) — device
    binding
  - `outcome = "success"`
  - `request_id` — correlation/request ID (from `X-Request-ID`; the request-id
    middleware guarantees a value in production)
  - `client_ip` — source address when available
  - `reason = "rotation_recovery"`

  This lets `cmdock-admin doctor` reconstruct the full identity transition —
  which authority authorised the swap, for which Runtime User and sync identity,
  from which old to new device — in a single audit record. The old credential is
  rejected on the TC sync endpoint (403 FORBIDDEN) immediately after rotation;
  the new credential authenticates successfully. Field presence is regression-
  locked by `rotation_recovery_revoked_device_forbidden_new_device_authenticates_gate6`
  in `tests/admin_integration/connect_and_devices.rs`.
- `device.revoke`
- `admin.user.quarantine`
- `admin.user.unquarantine`
- `replica.corruption_detected`
- `task.read.batch.invalid_uuid` — emitted on `GET /api/tasks?uuids=<csv>`
  validation failure (step 4 of the contract pipeline). Carries the standard
  `source` / `client_ip` / `user_id` / `request_id` envelope plus
  `invalid_index` (zero-based offset of the offending CSV segment). The
  index is recorded here for diagnostics rather than in the wire body per
  `task-read-contract.md` § Wire-body convention.
- `task.write.idempotent.first_execution` — emitted on `POST /api/tasks`
  or `POST /api/tasks/{uuid}/modify` when an `Idempotency-Key` is supplied
  and Phase 1 inserts a fresh `pending` dedup record. Carries the standard
  envelope plus `request_path`, `idempotency_key`, and `attempt_id` (the
  server-generated UUID that guards Phase 3 against stale finalizers).
- `task.write.idempotent.replay` — emitted when a retry hits a `completed`
  dedup record with matching body fingerprint and the stored response is
  replayed. Standard envelope plus `request_path` and `idempotency_key`.
- `task.write.idempotent.conflict` — emitted on `409 IDEMPOTENCY_KEY_CONFLICT`
  (same key, different body fingerprint, regardless of dedup row state).
  Useful for surfacing client bugs that reuse a key across distinct logical
  operations.
- `task.write.idempotent.in_flight` — emitted on `503 IDEMPOTENCY_IN_FLIGHT`
  (pending row exists with matching fingerprint, original attempt still
  running or stranded by process death).
- `task.write.idempotent.stranded_reaped` — emitted by the background
  pruner (`cmdock_server::idempotency::start`) when stranded `pending` rows
  are deleted. `source = "system"`. Operational hygiene only — lookup-time
  expiry already handles correctness.
- `merged_sync.version_received`, `merged_sync.version_accepted`,
  `merged_sync.version_finalized`, and `merged_sync.version_rejected` — emitted
  by `MergedSyncGateway` for inbound TW versions. They carry `user_id`,
  `client_id`, `gateway_attempt_id`, `journal_id`, `parent_version_id`,
  `merged_version_id` when known, `request_id` when supplied by the HTTP edge,
  and `outcome`.
- `merged_sync.source_apply_succeeded` / `merged_sync.source_apply_failed` —
  correlate TW input with canonical source writes. Success entries include
  `source_account`, `task_uuid`, `operation_index`, and `request_id` when
  available so an operator can answer which TW operation caused a source
  mutation and correlate it with sync/webhook context.
- `merged_sync.task_scope_forbidden` — unauthorized/non-personal
  `cmdock_task_scope` command input rejected before merged-version acceptance.
  (Prior to TSKEY-007/SG-011 this also covered `cmdock_account` inputs; those
  are now cleared by corrective projection rather than rejected at the gate.)
- `merged_sync.cmdock_task_scope_corrected`,
  `merged_sync.cmdock_account_corrected`, and `merged_sync.cmdock_key_corrected`
  — accepted untrusted writes to server-owned fields that were restored through
  corrective projection. `cmdock_account_corrected` fires when a legacy TW client
  pushes a `cmdock_account` UDA that still exists in TC history; the corrective
  pass clears it. These carry `source_account`, `task_uuid`,
  `operation_index`, `property`, and `outcome`.
  Under ADR-0012 these are the **gateway's** drift/orphan correction signal and
  replace the legacy sync-bridge `task.key.drift_recovered` /
  `task.key.migration_recovery` events (which the gateway path does not emit). A
  TW client forging `cmdock_key` on an existing task, or syncing a new task with
  a foreign `cmdock_key`, surfaces here. **The event only fires when the
  corrective projection actually emitted an op** — i.e. the client's value
  differed from canonical and was overwritten; a write that already matched
  canonical produces no event (it is gated on the projection's `changed` flag,
  not merely on detecting the inbound reserved-UDA op). Operators should treat a
  rising `cmdock_key_corrected` rate as real client/UDA drift, not noise.
- `merged_sync.corrective_projection_failed` and
  `merged_sync.journal_quarantined` — recovery/quarantine diagnostics for an
  accepted version that could not safely finish forward processing.
- `merged_sync.recovery_started` and `merged_sync.recovery_finished` — bracket
  the forward-recovery of a journaled inbound version (e.g. after a crash mid
  apply). `recovery_started` carries `from_state` and `outcome = "started"`;
  `recovery_finished` carries `from_state`, the terminal `outcome` (and a
  `code` when the outcome is a rejection/quarantine). Both carry the standard
  `user_id`, `client_id`, `gateway_attempt_id`, `journal_id`,
  `parent_version_id`, and `merged_version_id` correlators so an operator can
  trace a single recovery attempt end-to-end. These are the audit records that
  make ADR-0012's deferral of durable event-log correlation (gate 5,
  post-beta) acceptable for beta: an operator can "trace a gateway write through
  diagnostics and audit records without a durable task event-log entry".

The `Idempotency-Key` audit events deliberately omit the body fingerprint
(privacy) and the response payload (size). The `attempt_id` lets operators
correlate first_execution with subsequent replay/conflict/in_flight events
for the same logical request. See `task-write-contract.md` § Idempotency
for the full state-machine semantics.

- `task.key.drift_recovered` — emitted by the sync-bridge drift-recovery
  pass (`src/task_keys/drift.rs::reconcile_drift`) when a task's
  `cmdock_key` UDA on the canonical replica was reconciled against the
  `task_key_allocations` row. Standard envelope plus `task_uuid`,
  `kind`, `canonical_key`, and (when meaningful) `drift_value`. Three
  kinds:
  - `value_mismatch` — committed allocation row, UDA on canonical
    differed; reverse-UDA op restored canonical.
  - `post_commit_finalize` — pending allocation row whose UDA matches;
    `commit_task_key` finalised the row (Phase 2 ambiguous-recovery
    case).
  - `pending_with_drift` — pending allocation row whose UDA differs;
    reverse-UDA op + `commit_task_key` finalisation in the same pass.
  
  The contract-mandated **no-allocation-row** case is intentionally NOT
  audited (`task-write-contract.md` § Drift recovery, commit
  `7516969`): operator visibility uses the
  `task_keys_drift_skipped_no_row_total` operational counter instead,
  and the Phase 5e backfill orphan-reconciliation pass picks up the
  recovery. Audit silence on no-row is a contract-level assertion, not
  an oversight.

- `task.key.reaper_pass` — per-pass summary emitted by
  `src/task_keys/reaper.rs::run_reaper_pass` whenever any rows were
  burned, finalised, skipped, or escalated. Standard envelope plus
  `burned`, `finalised`, `skipped_uuid_attached`, `skipped_lock_busy`,
  `skipped_uda_mismatch`, `phase3_retry_failed`, `uda_cleared`. Source
  is always `"system"` (reaper is the only emitter today).
- `task.key.reaper_uda_mismatch` — per-row event emitted when a
  uuid-attached pending row's TC `cmdock_key` UDA does not match the
  allocation row's canonical key. The reaper LEAVES the row pending
  for operator review rather than auto-burning, mirroring Phase 4's
  `reconcile_pending_attached_rows` mismatch-bail policy. Carries
  `user_id`, `prefix`, `n`, `task_uuid`, `expected` (canonical
  `<PREFIX>-N` form), and `observed` (the actual UDA value present on
  the canonical replica).
- `task.key.reaper_phase3_retry_succeeded` — per-row event emitted
  when the reaper's Phase 3 retry of `commit_task_key` succeeds for a
  uuid-attached pending row whose TC task carries a matching
  `cmdock_key` UDA. Row transitions `pending → committed`. Carries
  `user_id`, `prefix`, `n`.
- `task.key.reaper_burn_with_uda_clear` — per-row event emitted when
  Phase 3 retry fails (`commit_task_key` returns Err) and the reaper
  escalates to burn-with-UDA-clear: emit a reverse `cmdock_key` UDA
  op, commit it under the held replica lock, then `burn_task_key`.
  Both writes occur under the same replica-lock acquisition so a
  concurrent TC sync/read cannot observe a half-applied "row burned,
  UDA still set" state. Carries `user_id`, `prefix`, `n`, `task_uuid`,
  and `uda_cleared` (`true` on the normal path; `false` only on the
  defensive no-op branch where the task or UDA is already absent
  despite Finalise classification — should be unreachable under the
  current same-lock classification/emit discipline since the reaper
  holds the replica lock continuously from index build through emit).
- `task.key.reaper_burn_after_uda_clear_failed` — per-row event
  emitted when the reverse `cmdock_key` UDA op committed successfully
  but the follow-on `burn_task_key` failed. Resulting transient
  state: row stays pending, canonical UDA already cleared. The
  contract-forbidden `row=burned, UDA still set` state is explicitly
  avoided by the UDA-first ordering in `burn_with_uda_clear`. The
  next reaper pass self-heals: TC scan finds the task with no
  `cmdock_key`, classifies as Burn, `burn_plain` transitions the row
  to burned. Carries `user_id`, `prefix`, `n`, `task_uuid`,
  `uda_cleared`, `error`.

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
