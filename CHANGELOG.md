# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Fixed
- **Webhook delivery no longer blocks the task-mutation HTTP response (#146/#149).**
  Webhook delivery retries run inline (1s/10s/60s), so a slow or dead endpoint
  previously stalled the mutation response for the full retry budget (~71s).
  Target lookup + delivery now dispatch asynchronously off the response path; a
  new `webhook_dispatch_in_flight` gauge surfaces in-flight/backed-up dispatch.
- **Merged-sync gateway: reserved-UDA drift on existing tasks now corrected in a
  single pass.** When a Taskwarrior client locally edited a server-owned UDA
  (`cmdock_key`) on an *existing* task and synced, the corrective projection
  compared canonical against the merged projection replica's own stale
  (already-canonical) state and emitted no counter-op — so the forged value
  persisted on the TW/merged surface until a *second* canonical-changing trigger
  (and never, on a quiet user). REST always stayed correct (reads the allocation
  table). `project_personal_now` now syncs/pulls the merged projection replica
  from storage **before** mirroring, so client-pushed drift is visible to the
  diff and corrected in the same pass the drift arrives (the client converges on
  its next pull). New tasks (orphan reconciliation) were unaffected. Verified on
  staging with a real Taskwarrior client (drift converges with zero extra
  triggers). Also: the `merged_sync.cmdock_key_corrected` audit event is now
  gated on the projection actually emitting a corrective op, so it no longer
  overclaims a correction when the client's value already matched canonical.

### Changed
- **Webhook creation cap (`MAX_WEBHOOKS_PER_USER`) is now enforced atomically (#155).**
  Both `POST /api/webhooks` (per-user) and `POST /admin/webhooks` (global) previously
  enforced the cap with a non-transactional count-then-insert precheck in the handler — a
  TOCTOU that could let concurrent creates briefly overshoot the cap. The count and insert
  now run together inside one writer transaction (`StoreError::WebhookLimitReached` →
  `422 LIMIT_REACHED`), so the stored count can never exceed the cap. **Behaviour change:**
  the cap check moved from *before* request normalization to *after* it, so a create that is
  both over-cap and malformed now returns the validation error (e.g. `INVALID_URL`) rather
  than `LIMIT_REACHED`, and over-cap requests do DNS resolution + secret encryption before
  being rejected. The 422 `LIMIT_REACHED` response is otherwise unchanged.
- **Async webhook dispatch is now bounded (#156).** The off-the-response-path
  dispatch introduced in #149 was an unbounded `tokio::spawn`; a burst of
  mutations to slow/dead endpoints could pile up tasks each sleeping the full
  retry budget. Dispatch is now capped by a semaphore (default **512**, override
  `CMDOCK_WEBHOOK_DISPATCH_MAX_INFLIGHT`). At capacity an event is **shed** — a
  permanent drop with no retry, counted by the new `webhook_dispatch_shed_total`
  counter and a warn log. Capacity is biased high so only pathological pile-up
  sheds, not a legitimate burst. The cap is global (noisy-neighbour limitation;
  per-user capping out of scope). The `webhook_dispatch_in_flight` gauge and the
  test-quiescence signal now derive from the same semaphore.
- **Config-DB write-path performance (#146).** Root-caused the
  release-qualification perf failures to serialization on the single shared
  `config.sqlite` connection (the fsync-free `get_user_prefix` read grew 368×
  from 1→50 users). Fixes: config reads now bypass the writer via a read pool
  (#147 — `get_user_prefix` flat across load, 35.9ms→0.11ms at 50 users); task
  creation does 2 config writes instead of 3 (#148 — reserve attaches the task
  UUID in one write); webhook target lookup + delivery dispatch off the response
  path (#149). The single writer connection remains the structural write-path
  ceiling (documented `ConfigStore`→Postgres path). `PRAGMA synchronous=NORMAL`
  was evaluated and **rejected** (#150/#151 — a power crash could lose committed
  allocation rows while the TC replica retains the task + `cmdock_key` UDA, with
  no recovery for migrated users; see `disaster-recovery-reference.md`).
- **`cmdock_account` suppressed from server output (TSKEY-007/SG-011).** The
  server no longer writes `cmdock_account` at any allocation site (REST create,
  Phase 4 backfill, drift reverse-op, merged-sync projection) and no longer
  emits it in `TaskItem` REST responses. This supersedes the #141 write-path
  added in the same Unreleased cycle. Migration 033 marks the transition.
  Pre-existing `cmdock_account` TC UDA values from prior deployments are
  suppressed in the REST `TaskItem` projection and actively cleared via
  corrective merged-sync projection so legacy TC UDA history converges forward
  without operator action.
  **Client migration:** any client reading `TaskItem.cmdock_account` must
  switch to `TaskItem.cmdock_task_scope` (same value, canonical field).
  Sending `cmdock_account` in request bodies continues to be accepted without
  error (tolerate-and-ignore); only `cmdock_task_scope` is validated
  (`400 INVALID_TASK_SCOPE` on mismatch).

### Added
- **Config write-path observability (#146/#152).** New `config_call_seconds{call}`
  histogram (caller-side `ConfigStore` timing on the task write path:
  `get_user_prefix`, `reserve`, `commit_task_key`) and `config_store_commit_seconds{call}`
  (inner `tx.commit()` timing, isolating fsync from queue-wait). Used to prove the
  write-path bottleneck is single-connection queue-wait (~94% at 50 users), not
  fsync (~5ms flat). New `webhook_dispatch_in_flight` gauge (#149). All documented
  in `metrics-catalog-reference.md`.
- **Secret-inclusive backup: webhook signing capability documented and signalled (BR-009).**
  The backup manifest previously left `webhook_secrets` empty — a dead field that misrepresented
  backup completeness. This field is replaced by `webhook_signing_via_encrypted_config_db: true`,
  which accurately signals that the `config.sqlite` file included in every secrets backup contains
  the AES-GCM-encrypted signing-secret rows for all webhooks. Plaintext secrets are intentionally
  never written to backup manifests. Restore instructions updated: operators must ensure
  `master_key` is configured after restore to decrypt webhook secrets; if the key was rotated
  since backup, webhooks must be re-registered.
- **X-Request-ID middleware (OBS-002/OBS-005).** Every response now carries
  `X-Request-ID`: if the client supplies the header, the value is echoed; otherwise
  a fresh UUID v4 is generated. The ID is injected into the request headers before
  any handler runs, so all downstream audit events that read `x-request-id` (TC sync
  source-apply, gateway audit, etc.) see a stable correlation value even without a
  client-provided header. The header is added to the CORS `allow_headers` and
  `expose_headers` lists so browser clients can send and read it. Implemented in
  `src/request_id.rs`.
- **Device rotation recovery audit fields (SG-010/ADMIN-008).** `device.rotate`
  audit events now carry the full ADR-0012 gate-6 required field set so
  `cmdock-admin doctor` can reconstruct the identity transition from a single
  record: `authority = "admin_http_token"` (operator authority identifier —
  distinct from the rotated/lost sync credential), `request_id`
  (correlation/request ID from `X-Request-ID`), `sync_identity` (canonical sync
  identity), `old_client_id` + `client_id` (device binding), `outcome = "success"`,
  `client_ip` (source address), `reason = "rotation_recovery"`, plus the layer-added
  timestamp. `device.register` events carry the same enriched envelope with
  `reason = "first_provisioning"`. The revoked device's credential is rejected by
  the TC sync endpoint (403 FORBIDDEN) immediately after rotation; the new credential
  authenticates on the first request. The gate-6 integration test asserts every
  required audit field is present (and that `sync_identity` is the canonical
  identity, distinct from both device bindings). Rotation now validates the
  public server URL **before** mutating credential state, so a configuration
  error fails fast with no revoke/provision and a committed swap always reaches
  its audit emit. (Known follow-up: a mid-rotation store failure between
  provision and revoke is not yet audited — tracked for a partial/failure-audit
  hardening pass.)
- **Gate 3 sequential replay test (SG-006).** `tc_sync_integration` now includes
  `stale_parent_conflict_retry_succeeds_gate3_sequential_replay`: submit v1 at NIL →
  submit v2 with stale NIL parent (409 + X-Parent-Version-Id = current tip) → retry
  v2 with corrected parent (200 OK).
- **Connect-config: OpenAPI registration and 250-byte QR budget (CC-002/OAPI-002).**
  `POST /admin/user/{user_id}/connect-config` is now registered in the OpenAPI spec
  (`ApiDoc`), making the endpoint visible in Swagger UI and generated clients.
  `MAX_CONNECT_URL_BYTES` is corrected from 300 to 250 per
  `connect-config-contract.md § Size Budget` (QR version 11 or below at ECC level M,
  251 byte-mode capacity). Connect URLs with long `name` fields that exceed 250 bytes
  are rejected with an explicit error; CLI callers should omit `--name` or use a short
  device label to stay within budget.
- **Admin CLI bootstrap command (#142).** `cmdock-server admin bootstrap
  user-device` provisions or idempotently replays a user + Taskwarrior
  device bootstrap credential via the same service as
  `/admin/bootstrap/user-device`. `--json` emits a single snake_case
  object for ops automation, including `client_id` as an alias of
  `device_client_id`; human output includes a Taskwarrior `.taskrc`
  snippet and sensitive-credential warning.
- **`cmdock_account` task-key UDA projection (#141).** The server now
  writes `cmdock_account` alongside `cmdock_key` at every allocation
  site (REST create, Phase 4 backfill empty/foreign paths,
  pending-attached reconcile, and Phase 5b drift reverse-op) per
  `cmdock/architecture@c92ca20`. v1 value is the allocation-row prefix.
  Reaper burn-with-UDA-clear intentionally preserves `cmdock_account`
  while clearing only `cmdock_key`. The UDA is filtered from REST
  `TaskItem` projection. Taskwarrior CLI users may see the new
  `cmdock_account` UDA after sync; declare
  `uda.cmdock_account.type=string` in `.taskrc` to avoid warnings, or
  ignore it if undeclared UDA warnings are acceptable. The staging seed
  scripts do not need changes because they never modify/read this
  internal UDA. New observability counter
  `task_keys_account_only_drift_observed_total` records cases where
  `cmdock_key` is canonical but `cmdock_account` is missing or wrong;
  #141 observes those cases but intentionally does not repair them.
- **Task keys foundation (Phase 1 of #130).** Per-user `<PREFIX>-N`
  task-key allocation infrastructure per `task-write-contract.md`
  § Task Keys (cmdock/architecture commits 1a7af9e + a69c647).
  Migrations 025 + 026 add `task_key_allocations` (three-state row
  model: `pending|committed|burned` — burned rows persist forever so
  rollback gaps cannot reuse `MAX(n)`) and a nullable `users.prefix`
  column. New `ConfigStore` primitives (`reserve_task_key_pending`,
  `attach_task_uuid_to_pending`, `commit_task_key`, `burn_task_key`,
  `select_stale_pending_task_keys`, `get_user_prefix`,
  `set_user_prefix`, `lookup_task_uuid_by_key`,
  `lookup_task_key_by_uuid`, `lookup_task_keys_by_uuids`,
  `users_without_prefix`).
  Reaper coordinator (`src/task_keys/reaper.rs`) wired into the
  existing 5-minute reaper tick; per-user mutation lock infra on
  `RuntimeRecoveryCoordinator` (`task_mutation_lock(user_id)`) reused
  by Phase 4 backfill.
  Admin CLI `user create --prefix=...` flag with `derive_prefix`
  fallback; new `admin user set-prefix <user_id> <prefix>` subcommand
  with `PREFIX_LOCKED` rejection rule (pre-allocation only).
  Startup routine assigns derived prefixes for existing users at boot
  (idempotent).
  Connect-config QR payload gains an additive `prefix: Option<String>`
  field per OQ6 sign-off (architecture#34) — older clients ignore it,
  no payload version bump.
  Wire-side surface (`TaskItem.key`, `TaskActionResponse.key`, key
  resolution on path params, drift recovery, lazy backfill) lands in
  Phase 2 onwards. (#130)
- **Task keys create-path wiring (Phase 2 of #130).** `POST /api/tasks`
  reserves a pending allocation row inside the existing idempotency
  Phase 2 closure, attaches the canonical `cmdock_key` UDA before TC
  commit, and finalises the allocation row to `committed` on commit
  success. `Idempotency-Key` retries return the original key without
  burning a slot (replay-no-burn regression lock pinned). The reaper
  scans pending rows older than the configurable timeout and either
  finalises them (if a matching TC task with `cmdock_key` UDA exists)
  or burns them (gap preserved). `TaskItem.key` and
  `TaskActionResponse.key` are projected from the committed allocation
  row (NOT the TC UDA) so brief mid-allocation UDA values can never
  leak to REST reads. List, view-scoped, batch, and singleton GET
  endpoints all populate `key` for committed allocations; pre-feature
  tasks have `key=None` until Phase 4 backfill runs. (#130)
- **Task keys per-account-lazy backfill (Phase 4 of #130).** Existing
  tasks (created before #130 shipped) get allocation rows + the
  `cmdock_key` UDA on first server access per user, idempotently and
  recoverably. Triggered from every task-CRUD entry point (`add_task`,
  `modify_task`, `complete_task`, `undo_task`, `delete_task`,
  `list_tasks`, `get_task_by_id`); fast-path is a `DashMap` lookup on
  `RuntimeRecoveryCoordinator::task_keys_migration_marked`. Slow-path
  acquires the per-user mutation lock (the same one used by
  `service::add_task` and the reaper — no separate migration lock),
  re-checks `users.task_keys_migrated_at` under the lock, and runs:
  Phase B writes the `cmdock_key` UDAs in one TC `Operations` batch
  (idempotent — skipped when the UDA already matches canonical), then
  Phase A+C atomically inserts every allocation row as
  `state='committed'` and updates `users.task_keys_migrated_at` in one
  `BEGIN IMMEDIATE` config-DB transaction. Migration `027` adds the
  `task_keys_migrated_at TEXT NULL` column. Three new `ConfigStore`
  primitives (`get_user_task_keys_migrated_at`,
  `mark_user_task_keys_migrated`, `max_n_for_user_prefix`,
  `commit_backfill_allocations_for_user`). Cache invalidation lives on
  `RuntimeRecoveryCoordinator::evict_user`, the single owner of the
  per-user runtime-cache eviction recipe per CLAUDE.md § Runtime cache
  eviction — restore / delete-user / offline-quarantine all funnel
  through it. New metrics `task_keys_migration_started_total`,
  `task_keys_migration_completed_total`,
  `task_keys_migration_recovery_total` (no high-cardinality labels).
  New audit events `task.key.migration_started`,
  `task.key.migration_completed`, `task.key.migration_recovery`.
  Deferred from Phase 4: `task_keys_migration_pending_users` gauge —
  startup-time initialisation requires a one-shot `users WHERE
  task_keys_migrated_at IS NULL` count, kept out of this PR's scope.
  Codex review iter1 fixes: (1) mutation handlers now run the gate
  inside `resolve_mutation_path_param_or_audit` BEFORE key resolution
  so first-access `POST /api/tasks/<PREFIX>-N/done` doesn't 404; (2)
  backfill reconciles pending+attached allocation rows under the
  per-user mutation lock before computing fresh candidates so the
  atomic Phase A+C insert can't collide on `idx_task_key_allocations_uuid`;
  (3) `commit_backfill_allocations_for_user` now takes `expected_max_n`
  and rejects with `BackfillMaxChanged` if `MAX(n)` shifted between
  Phase B and the commit; (4) the same primitive verifies the user
  row still exists inside the transaction and rejects with
  `BackfillUserMissing` on a concurrent `delete_user` race. (#130)
- **Task keys path-param resolution (Phase 3 of #130).** All five
  task-path endpoints (`GET /api/tasks/{uuid}`,
  `POST /api/tasks/{uuid}/{modify,done,undo,delete}`) accept a UUID
  **or** a task key in the form `<PREFIX>-N` (e.g. `WORK-15`). Prefix
  is case-insensitive on input (`work-15` resolves identically). UUID
  strictness preserved by endpoint: read singleton stays canonical-only
  per #109; mutation endpoints stay permissive (`Uuid::parse_str` —
  uppercase / simple / braced / URN forms accepted) per Decisions
  Locked In iter3 — no behaviour change for clients in the wild.
  Tightening mutation-endpoint UUID parsing to canonical-only is a
  separate, gated follow-up. Cross-account key probes return 404 with
  empty body (existence-leak parity with cross-account UUID).
  New metric `task_keys_resolution_total{form,outcome}` (server-wide,
  no high-cardinality labels). New audit reason `unknown_key`
  distinguishes resolved-but-not-allocated from malformed input
  (`invalid_uuid`). Webhook payloads continue to carry `key` on
  `task.created` / `task.modified` / `task.completed` events — pinned
  by regression test. (#130)
- New Prometheus metrics for the task-key foundation:
  `task_keys_allocated_total`,
  `task_keys_burned_total{reason}`,
  `task_keys_reaper_finalised_total`,
  `task_keys_reaper_pass_seconds`,
  `task_keys_reaper_lock_acquire_seconds`,
  `task_keys_prefix_assigned_total{source}`. No high-cardinality
  user-id labels (per iter3 finding). (#130)
- New audit events: `account.prefix_set` (signup / backfill / operator
  override), `task.key.reaper_pass`. (#130)
- **Task keys sync-bridge drift recovery (Phase 5b of #130).**
  Post-canonical-apply read-back inside `do_sync` reconciles the
  `cmdock_key` UDA on the canonical replica against the
  `task_key_allocations` table per `task-write-contract.md` §
  Drift recovery (cmdock/architecture commit `7516969`,
  Approach B). Four kinds: `value_mismatch` (committed row, UDA
  drifted → reverse-UDA op restores canonical),
  `pending_with_drift` (pending row, UDA drifted → reverse-UDA
  op + `commit_task_key`), `post_commit_finalize` (pending row,
  UDA matches → finalise), `no-row` (UDA without allocation row
  → SKIP, deferred to Phase 5e — operational metric only, no
  audit). Reverse-UDA commit holds the same replica lock as the
  preceding `replica.sync()` so REST readers cannot interleave.
  New audit event `task.key.drift_recovered` (per-row, with
  `kind` field). New metric
  `task_keys_drift_recovered_total{kind}` and operational
  counter `task_keys_drift_skipped_no_row_total`. (#130)
- **Task keys reaper Phase 3 retry + burn-with-UDA-clear (Phase 5c
  of #130).** Reaper now retries `commit_task_key` idempotently
  on uuid-attached pending rows whose TC task carries a matching
  `cmdock_key` UDA; success transitions the row `pending →
  committed`. On failure (rare; transient DB error or
  constraint), the reaper escalates to burn-with-UDA-clear:
  emits a reverse `cmdock_key` UDA op clearing the canonical
  value, commits it under the held replica lock, then calls
  `burn_task_key`. Both writes occur within the same replica-
  lock acquisition so concurrent TC sync/REST readers cannot
  observe an "row burned, UDA still set" half-applied state.
  Reaper now holds the per-user replica lock across the entire
  uuid-attached candidate batch (mutation lock → replica lock,
  symmetric with mutation handler ordering). New per-row audit
  events `task.key.reaper_phase3_retry_succeeded` and
  `task.key.reaper_burn_with_uda_clear`; new metrics
  `task_keys_reaper_phase3_retried_total{outcome}` and
  `task_keys_reaper_uda_cleared_total`. The
  `SkipUdaMismatch` policy is intentionally preserved — auto-
  clearing the UDA on canonical-vs-row mismatch would compound
  data-loss risk and is gated on a future contract amendment.
  (#130)
- **Task keys REST projection lookup-time expiry (Phase 5d of #130).**
  New `ConfigStore::lookup_task_keys_for_projection` primitive returns
  every `committed` row plus every `pending` row within the
  pending-timeout window per `task-write-contract.md` § REST projects
  from the allocation table. REST evaluates `created_at +
  pending_timeout > now` at lookup time, so a stranded pending row
  whose timeout has expired projects as `null` even if the reaper has
  not yet physically transitioned it to `burned` — REST correctness
  is decoupled from reaper scheduling. The same
  `task_write.idempotency_pending_timeout_seconds` config drives both.
  All five REST read paths (`GET /api/tasks`, `?view=`, `?uuids=`,
  `GET /api/tasks/{uuid}`) and the singleton path now use the
  projection primitive. The committed-only `lookup_task_keys_by_uuids`
  remains for backfill, which depends on its "absent from map = no
  allocation row" semantics. (#130)
- **Task keys backfill orphan reconciliation (Phase 5e of #130).**
  Backfill now distinguishes three classifications when visiting a
  candidate task per `task-write-contract.md` § Orphan reconciliation:
  *empty UDA* (steady-state first-time backfill — allocate next-N +
  write UDA), *matched UDA* (recovery from a prior crashed Phase B —
  re-stamp allocation row only), and *foreign UDA* (orphan — overwrite
  with fresh-N, never adopt the encoded N even if it happens to be
  unallocated). The fresh-N rule preserves "burned numbers never
  re-allocate": backfill always picks `MAX(n)+1`. New audit kind
  `task.key.migration_recovery` `kind="orphan_reconciled"` and counter
  `task_keys_orphans_reconciled_total` emit per-task AFTER both Phase
  B (UDA commit) and Phase A+C (allocation row commit) succeed
  (audit-after-success pattern, mirrors `reconcile_drift`). Drift
  recovery's existing operational counter
  `task_keys_drift_skipped_no_row_total` is the contract-aligned
  signal for orphans observed by the sync-bridge between backfill
  passes. (#130)
- `Idempotency-Key` header support on `POST /api/tasks` and
  `POST /api/tasks/{uuid}/modify` per `task-write-contract.md` §
  Idempotency (cmdock/architecture commit a3f242a). Optional ASCII
  string (1-64 chars). Retries with the same key + same body within
  the retention window (24h default) replay the original response
  byte-identically; same key + different body → `409
  IDEMPOTENCY_KEY_CONFLICT`; same key with the original still in
  flight → `503 IDEMPOTENCY_IN_FLIGHT` (with `Retry-After`). Header
  absent → at-least-once retry semantics (existing behaviour
  preserved). Lifecycle endpoints (`done`, `undo`, `delete`) tolerate
  the header forward-compat without engaging the dedup machinery.
  Implements server#114; closes the iOS#98 at-least-once exception
  for `addTask` once iOS adopts. (#114)
- `[task_write]` config section with three knobs:
  `idempotency_retention_hours` (default 24),
  `idempotency_pending_timeout_seconds` (default 300),
  `idempotency_retry_after_seconds` (default 5). Env overrides:
  `CMDOCK_TASK_WRITE_IDEMPOTENCY_RETENTION_HOURS`,
  `CMDOCK_TASK_WRITE_IDEMPOTENCY_PENDING_TIMEOUT_SECONDS`,
  `CMDOCK_TASK_WRITE_IDEMPOTENCY_RETRY_AFTER_SECONDS`. (#114)
- New audit events: `task.write.idempotent.first_execution`,
  `task.write.idempotent.replay`, `task.write.idempotent.conflict`,
  `task.write.idempotent.in_flight`,
  `task.write.idempotent.stranded_reaped`. Useful for debugging
  duplicate-creation incidents. (#114)
- New Prometheus metrics:
  `idempotency_phase2_duration_seconds{operation}` histogram (Phase 2
  mutation duration — operators monitor for tail-latency drift toward
  the configured pending-timeout) and
  `idempotency_outcomes_total{operation, outcome}` counter. (#114)
- New OpenAPI response codes documented on add/modify: `400
  INVALID_IDEMPOTENCY_KEY`, `409 IDEMPOTENCY_KEY_CONFLICT`, `503
  IDEMPOTENCY_IN_FLIGHT`. (#114)
- Migration `024_create_idempotency_records.sql` (config DB).
  Tuple-keyed dedup table with attempt-id-guarded Phase 3 update,
  state CHECK enum, fingerprint BLOB, response payload nullable
  while pending. Single-process; cross-instance dedup is out of
  scope per § Limitations and operational notes. (#114)

### Storage / atomicity note
- The contract-specified single-transaction atomicity claim was
  restructured during pre-implementation review (server#114 Q1):
  `cmdock-server` task-write effects span two SQLite databases
  (TaskChampion replica vs config DB), so true single-transaction
  atomicity would require a TC fork. The contract now specifies a
  three-phase write-ahead pattern with lookup-time expiry as the
  load-bearing residual-window bound. The reaper is operational
  hygiene only; correctness does not depend on it. See `CLAUDE.md`
  § Idempotency for the implementation gotchas.

- `GET /api/tasks/{uuid}` — singleton task lookup by UUID. Returns `200`
  with the `TaskItem` body for any task in the authenticated identity's
  replica (status pending/completed/deleted — visibility rule per
  `task-read-contract.md` § Visibility rule). Path-parameter validation
  runs before replica access; malformed UUIDs return `400 INVALID_UUID`
  (plain-text body). Unknown UUIDs and cross-account UUIDs both return
  `404` with **empty body** — indistinguishable per the existence-leak
  rule. Implements `task-read-contract.md` (cmdock/architecture). (#109)
- `GET /api/tasks?uuids=<csv>` — batched UUID lookup. Returns `200` with
  `{found: TaskItem[], missing: string[]}`; partial-success at the HTTP
  level. **Request-order preserved** in both arrays (not UUID-sorted).
  Validation pipeline mirrors the contract's normative step list:
  parse → `EMPTY_UUIDS` (empty after parse) → `TOO_MANY_UUIDS`
  (raw entries > cap, applied **before** dedupe) → `INVALID_UUID`
  (malformed entries, including empty CSV segments from leading/
  trailing/consecutive commas; offending index recorded in audit log
  + tracing, **not** in the wire body) → dedupe → resolve. Cap is
  `task_read.batch_max_uuids` (default 100, env override
  `CMDOCK_TASK_READ_BATCH_MAX_UUIDS`). Cross-account UUIDs surface in
  the `missing` array indistinguishable from unknown UUIDs. (#109)
- `[task_read]` config section with `batch_max_uuids` setting (default
  100). Env override `CMDOCK_TASK_READ_BATCH_MAX_UUIDS` (positive
  integer; non-positive or non-numeric values are ignored). Documented
  in `config.example.toml` and the 12-Factor Compliance table. (#109)
- `TaskBatchLookupResponse` schema in OpenAPI; new operation_ids
  `getTaskById` (singleton) and `listTasks` extended to cover the three
  request shapes (no-params, view, uuids). (#109)

### Fixed
- **Bootstrap path now assigns a task-key prefix at user creation
  (#137 / #130 follow-up).** `admin::services::bootstrap::ensure_user`
  was missing the `derive_prefix` + `apply_prefix` step that the
  in-container CLI (`admin::cli::user::run`) has done since Phase 1 of
  #130. The gap sat dormant until Phase 4 made the missing-prefix case
  fatal in `ensure_user_task_keys_migrated`, surfacing as `500` on
  `/api/tasks` reads for users created via `cmdock-admin user create`
  (HTTP bootstrap path). Audit `account.prefix_set` event now carries
  `source = "bootstrap"` to distinguish HTTP user creation from
  in-container CLI signup. (#137)

### Changed
- **Wire-breaking: `TaskItem.key` is now nullable on the wire (`string |
  null`) per `task-write-contract.md` § Wire exposure (Phase 5d of
  #130).** The field was previously omitted from the JSON when the task
  had no allocation row (`skip_serializing_if = "Option::is_none"`); it
  is now always present and emits explicit `null` for any of the four
  transient causes (pre-migration, burned, expired-pending, orphan).
  Clients still using `decodeIfPresent` against a key that is now
  always-present-but-nullable need updating — iOS#101 + obsidian#3
  carry the migration on the client side. Other JSON shapes
  unchanged. OpenAPI 3.1 schema reflects the change as
  `"type": ["string", "null"]`. (#130)
- **Internal:** Decoupled task-write date parsing from filter grammar.
  `parse_date_value` (and helpers) moved from `tasks::filter::dates` to
  `tasks::dates`; six task-write call sites
  (`tasks::handlers::validate_raw_recognised_dates`,
  `tasks::service::modify_task`, three sites in `replica::apply_task_parsed`)
  now import the new home. Filter parsing exposes a clock-injected
  `parse_filter_at(input, now)` so AST-resolved dates share a reference
  time with the evaluator instead of baking a hidden `Utc::now()` into the
  AST at parse time. The eval-time reparse fallback in `eval_date_attr` is
  removed — `parsed_date` is trusted, and an unparseable filter value
  (e.g. `due:notadate`) silently fails to match. Wire surface unchanged.
  ADR-0002 §Independence. (#127)
- **Internal:** Replaced caller-side SQLite-string substring matching
  with a typed `StoreError` enum at `src/store/error.rs`. Five sites
  (two in `webhooks/api.rs`, two in `admin/services/bootstrap.rs`,
  one in `admin/services/sync_identity.rs`) used to match raw SQLite
  error text like `"UNIQUE constraint failed: users.username"`;
  they now match `StoreError::Constraint(ConstraintKind::Unique {
  resource })` against stable labels declared in
  `store::error::resources`. Backend mapping lives in a single
  function (`src/store/sqlite.rs::rusqlite_unique_resource`) — adding
  a new resource label is a one-line edit there. Migration is
  incremental: only the trait methods with introspecting callers
  changed return type (`create_user`, `create_replica`,
  `create_bootstrap_device`, `create_webhook`, `create_admin_webhook`,
  `update_webhook`, `update_admin_webhook`); the rest stay on
  `anyhow::Result`. Substring helpers in bootstrap and sync_identity
  services removed. New `ProvisionDeviceError::BootstrapRequestConflict`
  variant carries the typed signal up to bootstrap.rs. No external
  behaviour change. Resource-label round-trip pinned by 5 new unit
  tests in `src/store/sqlite.rs`. Part of ADR-0002 § P4 sub-fix 3
  — closes the umbrella alongside sub-fixes 1, 2 and 4. (#124)

- **Internal:** Locked the webhook auto-disable threshold boundary
  with explicit unit tests for both user and admin webhooks. The
  threshold (`DISABLE_AFTER_FAILURES = 10` in
  `src/webhooks/delivery.rs`) was already passed as a SQL parameter
  rather than hard-coded — sub-fix 2 audit confirmed nothing
  structural needed lifting. The new boundary tests pin the
  transition (failures == threshold − 1 → enabled; failures ==
  threshold → disabled; failures > threshold → stays disabled) so
  future changes to the SQL primitives can't silently regress the
  contract. Part of ADR-0002 § P4 sub-fix 2. (#124)

- **Internal:** Moved file-level maintenance ops (checkpoint, backup,
  restore) off the `ConfigStore` trait onto a new
  `OperatorMaintenanceBackend` trait at `src/store/maintenance.rs`.
  These ops can't be honestly implemented by a non-file backend like
  Postgres, so they no longer pretend to be portable. `AppState` now
  carries `store: Arc<dyn ConfigStore>` and
  `maintenance: Arc<dyn OperatorMaintenanceBackend>` separately; in
  production both views derive from the same `Arc<SqliteConfigStore>`
  so there's still only one DB handle. No external behaviour change.
  Part of ADR-0002 § P4 sub-fix 1. (#124)

### Changed
- `GET /api/tasks` is now **strict-recognise** on query parameters per
  `task-read-contract.md` § GET /api/tasks request shapes. Three valid
  shapes: no params (pending list — preserved), `?view=<id>`
  (view-scoped — preserved), `?uuids=<csv>` (new batch — see above).
  Mutual exclusion between `view` and `uuids` (both supplied →
  `400 INVALID_QUERY_PARAM`). Repeated keys, unknown keys, and
  `view` + `uuids` combined return `400 INVALID_QUERY_PARAM` (plain-text
  body). **Behaviour change:** clients that previously sent unknown
  query parameters (e.g. forward-compatibility probes, typo'd keys)
  alongside `?view=<id>` got `200`; they now get `400`. The no-params
  case (`GET /api/tasks` with no query string) is **unchanged** —
  still returns the pending list. (#109)

- Default contexts (`personal`, `work`, `health`) are now seeded for every user
  on first access to `/api/contexts` or `/api/app-config`. The IDs match the
  `context_id` references on the project-scoped default views, so context
  auto-scoping now works out of the box. Seeded `projectPrefixes` are
  uppercase (`PERSONAL` / `WORK` / `HEALTH`) for parity with the v4 hardcoded
  filters. User edits and deletions are preserved across reconciliation
  via `user_modified` / `hidden` flags on the `contexts` table. (#97)

### Changed
- `PUT /api/views/{id}` now validates `context_id` against the user's
  existing contexts and rejects dangling references with `400 INVALID_CONTEXT_ID`.
  Previously the field was silently dropped on user-created views; clients
  that have been sending `contextId` and getting `200` will now get `400` if
  the referenced context does not exist. Custom views can now persist a valid
  `context_id`. (#97)
- `GET /api/tasks` now returns tasks in deterministic UUID-ascending order on
  both the filtered-view path and the no-filter (pending-only) path. Identical
  requests over identical data produce identically-ordered task arrays.
  Previously the filtered path surfaced Rust `HashMap` iteration order to the
  wire, varying per-process. Clients that only sorted to compensate for
  nondeterministic server order can drop that workaround; clients still need
  their own UX sort. (Note: at the per-task level, UDA pass-through fields are
  still serialised from a `HashMap` — full byte-identical bodies for tasks
  with multiple UDAs is tracked separately.) (#102)
- `GET /api/summary` now sorts the matching task list by UUID before passing
  it to the LLM prompt (same `HashMap.values().collect()` pattern as `/api/tasks`).
  Improves Anthropic prompt-cache hit rate for repeat summary calls over
  unchanged data. The template-fallback path doesn't read the task list, so
  this is a no-op there. No client-visible change — the summary string
  output is still LLM-generated.

### Added
- REST `POST /api/tasks` `parse_raw` recognises `wait:VALUE` and
  `scheduled:VALUE` tokens. Same broad date parser as `due:` (named dates,
  ISO, relative durations, canonical TW format). Tokens like `wait:7d` and
  `scheduled:tomorrow` now set the wait/scheduled date instead of falling
  through to the description. Implemented per `task-write-contract.md`
  § Recognised raw-syntax attributes; ADR-0011 § Per-Attribute-Family
  Evolution. Other unrecognised `name:value` tokens (`recur:`, `until:`,
  etc.) continue to fall through to the description (lenient-drop deviation
  preserved). (#100)
- `POST /api/tasks/{uuid}/modify` accepts `wait` and `scheduled` fields
  (canonical `YYYYMMDDTHHmmssZ` only — broader parser does NOT apply on
  modify). Both fields support JSON-Merge-Patch-style clear semantics:
  explicit `null` clears the value; omission leaves unchanged. Implemented
  via `Option<Option<String>>` + custom serde to distinguish absent from
  null. (#100)

### Changed
- `POST /api/tasks` and `POST /api/tasks/{uuid}/modify` request bodies are
  now strict-recognise per ADR-0011: unknown top-level fields are rejected
  with `400 INVALID_FIELD` (plain-text body). Non-canonical wait/scheduled
  values return `400 INVALID_DATE`. **Behaviour change:** clients sending
  forward-compatibility probe fields or extra typo'd keys will now get
  `400` where they previously got `200` with the field silently ignored.
  iOS and obsidian-plugin smoke checks before prod merge. (#100)

- `POST /api/tasks/{uuid}/modify` now honours JSON-Merge-Patch null-clears
  semantics on `project`, `priority`, and `due` — explicit JSON `null`
  clears the field; omission leaves unchanged. Closes the retrofit gap
  documented in `task-write-contract.md` § Known retrofit gap; matches the
  pattern established by `wait`/`scheduled` in #100. **Behaviour change:**
  clients today that send `{"project": null}` (or `priority` / `due`)
  expecting a no-op will see the field cleared. Verified no current client
  relies on the old no-op behaviour — clients omit fields they don't want
  to change rather than supplying explicit null. `due` continues to accept
  the broad date parser on set (named dates, ISO, etc.) per the contract
  asymmetry. (#105)

### Known asymmetries
- `due` modify accepts the broad date parser (named dates, ISO, etc.)
  on set; `wait` / `scheduled` modify accept canonical
  `YYYYMMDDTHHmmssZ` only. Documented in `task-write-contract.md`
  § Date format on modify; not a retrofit, an intentional asymmetry per
  ADR-0011's retrofit rule (existing field behaviour preserved on set).

## [0.2.0] - 2026-04-27

### Added
- `scripts/seed-udas.sh` — applies UDAs to harness-seeded tasks on staging via
  TW CLI. Covers estimate (S/M/L), energy (low/medium/high), and area fields
  across 14 of 16 seeded tasks. Idempotent, deterministic assignment.
- `depends` field on `TaskItem` API response listing UUIDs of pending
  dependencies. Enables clients to show what a task is blocked by, not just
  that it is blocked. Sorted deterministically. On list/summary responses
  `blocked == (depends.len > 0)`. On mutation webhook payloads `depends` may
  be empty when blocked — treat `blocked` as authoritative. (#79)
- User-defined attributes (UDAs) are now emitted as top-level string keys on
  `TaskItem` API responses. All TaskChampion user-defined properties pass
  through automatically, matching the Taskwarrior JSON format. (#81)
- Optional `context_id` field on view objects in `GET /api/views` and
  `GET /api/app-config`. Project-scoped named views (personal, work, health)
  now use `context_filtered=true` with an explicit `context_id` binding;
  clients auto-apply the bound context's `projectPrefixes`. (#84)
- `--scheme` flag on `staging-qr.sh` for staging-specific connect URLs
  (`cmdock-staging://`). (#80)

### Changed
- `set_startup_recovery_summary` now accepts `&StartupRecoverySummary` struct
  instead of 7 positional parameters (ADR-0002 HC-1 compliance). (#78)
- Urgency calculation now matches stock Taskwarrior defaults. Due dates no
  longer produce negative urgency. Tags use stepped scaling (max 1.0, was 3.0).
  New factors: annotations, age, active, blocking, blocked, scheduled, waiting.
  Sort order of task lists will change for existing users. (#77)
- Switched the global allocator to `tikv-jemallocator` on Linux + glibc with
  tuned `malloc_conf` (`background_thread:true`, `dirty_decay_ms:1000`,
  `muzzy_decay_ms:0`, `narenas:2`). Reduces sustained-load RSS by ~40 % and
  stabilises the 1-hour endurance profile well inside its memory budgets.
  Operators can override any setting via the `MALLOC_CONF` env var at
  startup. See `docs/reference/release-qualification-reference.md §2.1`.
- Reworked the root documentation set to match the shared documentation
  standards.
- Added a root contribution guide and clarified the README landing-page
  structure.
- Split the public container image from the internal runtime image so the
  published self-host image no longer bakes in internal CA trust material.
- Simplified the public Docker Compose deploy path to one self-host variant
  using the stock Caddy image and generic TLS modes.
- Added a `cargo-deny` policy with a commercial-ready licence allowlist,
  advisory checking, and source gating. Exposed as `just deny`; wired into
  the internal Woodpecker security pipeline alongside `cargo-audit`.
- Added tracked git hooks (`pre-commit` = fmt+clippy, `pre-push` = full
  `just check`) installable via `just install-hooks`. These are local-only;
  no CI change.

### Fixed
- `scripts/load_test_summary.py::histogram_quantile_ms` now linearly
  interpolates within the bucket that crosses the quantile threshold,
  matching Prometheus's `histogram_quantile()` semantics. Previously it
  returned the bucket upper bound without interpolating, producing clamped
  p95 values like exactly 1000 ms for any observation in the (0.5, 1.0 s]
  HTTP bucket.
- `config::tests::test_env_overrides` and the four sibling `ServerConfig`
  load tests are now serialised through a module-local mutex so the
  parallel-test race on process-global `std::env::set_var` / `remove_var`
  no longer intermittently fails `just check` / `cargo test`.
- Patched a security advisory: bumped `rand` 0.9.2 → 0.9.3 and 0.10.0 →
  0.10.1 to resolve RUSTSEC-2026-0097 (unsound stacked-borrows in
  `ThreadRng` reseed path under custom loggers). Surfaced by the new
  `cargo-deny` gate.
- Bumped `rustls-webpki` 0.103.10 → 0.103.13 to resolve RUSTSEC-2026-0098
  (URI name constraints incorrectly accepted), RUSTSEC-2026-0099 (name
  constraints accepted for wildcard certs), and RUSTSEC-2026-0104 (reachable
  panic parsing CRLs). SemVer-compatible patch bump within the 0.103.x line.

### Removed
- Dropped the unused `jsonwebtoken` and `argon2` crate dependencies —
  scaffolded for Phase 2 but never wired up.

## [0.1.0] - 2026-04-06

### Added
- Initial open-source release of `cmdock-server`.
- Bearer-token REST API for task CRUD, views, config, and summaries.
- TaskChampion-compatible sync surface for Taskwarrior-class clients.
- Local admin CLI for user, token, sync identity, device, and maintenance workflows.
- Standalone documentation library under `docs/manuals`, `docs/reference`, `docs/adr`, and `docs/implementation`.
