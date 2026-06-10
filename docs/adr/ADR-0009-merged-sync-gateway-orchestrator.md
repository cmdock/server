---
created: 2026-05-09
status: accepted
tags: [architecture, sync, taskchampion, orchestrator]
---

# ADR-0009: MergedSyncGateway Runtime Orchestrator

## Status

**Accepted** for server#143.

This ADR is the repo-local companion to `cmdock/architecture` ADR-0012
(`Merged Sync Gateway As Day-One Runtime TW Sync Architecture`). ADR-0012
chooses the cross-repo product/runtime direction. This ADR defines the
`cmdock/server` internal boundary that keeps that direction compatible with
ADR-0002.

## Context

server#139 proved that a real Taskwarrior client can sync a single merged
TaskChampion chain when the server projects canonical source replicas into a
TW-visible chain and demultiplexes inbound TW operations back into source truth.

ADR-0012 changes the delivery posture: because beta has not opened and the only
known existing users are two dogfood users, `cmdock/server` will not ship a
personal-sync path first and later migrate to a merged gateway. Instead, the
runtime ships one TW sync architecture from beta onward:

```text
Taskwarrior client
  -> TC sync protocol edge
  -> MergedSyncGateway
  -> canonical source replica(s)
  -> durable merged TW projection chain
```

For beta/no-teams, the gateway still runs, but the visible Task Scope set has
one scope: the Runtime User's Personal Task Scope. Teams later add Team Task
Scopes and policy, not a second sync architecture.

The existing sync code has several load-bearing pieces that remain useful:

- sync client authentication and device lifecycle;
- TC encryption envelope and per-device payload translation;
- per-user canonical TaskChampion replicas;
- sync storage primitives;
- runtime recovery/cache eviction;
- bridge freshness/scheduling concepts;
- audit, metrics, and task event surfaces.

What changes is the ownership boundary. The TC HTTP handler and the existing
`sync_bridge.rs` must not become the home of account routing, operation decoding,
forward recovery, projection policy, and journal semantics. That would bless the
drift ADR-0002 already warns about.

## Decision

Introduce `MergedSyncGateway` as a named ADR-0002 designated orchestrator and
make it the sole owner of TW-sync-to-source-truth orchestration.

The gateway is responsible for accepting one inbound TW merged-chain version,
turning it into typed gateway operations, authorizing/routing those operations
against source truth, appending any projected/corrective operations to the
merged chain, and recovering forward after crashes.

The TC sync HTTP layer becomes a protocol adapter:

```text
HTTP request/auth/body limit/content type
  -> resolved sync identity + decrypted/translated payload facade
  -> MergedSyncGateway
  -> TC protocol response
```

`sync_bridge.rs` remains the existing REST/canonical-replica to TC-sync-storage
bridge while the replacement is under development, but the released beta runtime
must not expose both old personal sync and gateway sync as selectable product
paths. Transitional coexistence inside the feature branch is implementation
scaffolding only.

### Designated-orchestrator boundary

`MergedSyncGateway` may know:

- resolved sync identity (`user_id`, `client_id`/device id, request metadata);
- decrypted history-segment bytes through a crypto/payload facade;
- typed decoded TaskChampion wire operations through the gateway codec;
- the Runtime User's visible Task Scope set and permissions via narrow service
  traits;
- Task Scope write primitives for the Personal Task Scope now and Team Task
  Scopes later;
- task-key/task-scope correction primitives required for `cmdock_key` and
  `cmdock_task_scope` projection;
- durable merged-chain storage, snapshots, and projection cursors;
- inbound gateway journal/recovery state;
- audit, metrics, and task-event emission contracts.

It must not own:

- bearer-token auth;
- sync auth resolution or device lifecycle decisions;
- cryptographic key derivation/envelope internals;
- REST DTOs or `TaskItem` projection;
- view/filter resolution;
- billing/control-plane/team-management UX;
- raw SQLite details outside store/storage primitives;
- generic task CRUD validation unrelated to sync-originated operations.

### Module shape

The implementation should introduce a new module boundary rather than growing
existing handlers:

```text
src/merged_sync_gateway/
  mod.rs                  # public facade called by tc_sync adapter/runtime
  codec.rs                # sole owner of TC history-segment WireOp mirror
  journal.rs              # inbound version journal and recovery state values
  journal_ops.rs          # journal row transitions / quarantine helpers (as-built)
  planner.rs              # WireOps -> source/corrective plans
  projection.rs           # source truth -> merged-chain operations/snapshots
  inbound.rs              # inbound version apply/route orchestration (as-built)
  source.rs               # narrow traits for Task Scope writes and reads
  recovery.rs             # forward-only recovery driver
  recovery_acceptance.rs  # recovery acceptance gating (as-built)
  protocol.rs             # gateway protocol value types (as-built)
  storage.rs              # merged sync storage wrapper (as-built)
  sqlite_error.rs         # gateway sqlite error classifier (as-built)
  audit.rs                # gateway audit events (as-built; see Cross-cutting checklist)
```

The original ADR proposed the first seven boundaries; the `(as-built)` modules
are the realized supporting split and preserve those boundaries rather than
introducing new ones. The exact file names may change, but these boundaries
should remain explicit. Raw TaskChampion JSON/private wire shape must not escape
`codec.rs`.

### Task Scope terminology and transition invariants

This ADR uses the cross-repo vocabulary from `cmdock/architecture`:

- **Runtime User** — the server runtime principal for auth, devices, sync
  identities, and self-host identity issuance.
- **Hosted Account** — the control-plane managed identity record.
- **Organisation** — the managed tenancy / billing / commercial container.
- **Team** — a collaboration group with runtime task RBAC.
- **Task Scope** — the server-side task ownership, key namespace, event-log
  scope, canonical task storage, and sync-projection unit.

Load-bearing invariants for the server implementation:

1. `user_id` identifies the Runtime User for auth/device ownership. It is not
   the durable task namespace identity.
2. `task_scope_id` is the stable task ownership/key/event/projection identity.
3. `cmdock_task_scope` is the TW-visible prefix command/projection state. It is
   untrusted input; the server resolves the prefix to `task_scope_id` and then
   authorizes the write against local applied membership/RBAC state.
4. Server authorization on the sync hot path uses locally-applied Team/Task
   Scope membership state. It must not call the control plane to authorize an
   ordinary Taskwarrior sync request.
5. `MergedSyncGateway` projects only shard-local readable Task Scopes visible to
   the Runtime User.
6. Existing users are migrated to explicit Personal Task Scopes before beta:
   every Runtime User has exactly one Personal Task Scope, `kind = personal`,
   `owner_runtime_user_id = users.id`, and `key_prefix = users.prefix`.

The server/control-plane boundary follows `cmdock/architecture` ADR-0005 and
ADR-0012: cmdock/server owns Runtime Users, devices, Task Scopes, canonical task
storage, task-key allocation, merged sync projection, Team/membership/RBAC state
needed for runtime enforcement, event logs, and standalone self-host identity
issuance. The control plane owns Organisations, Hosted Accounts,
billing/subscription/plan, managed onboarding/invites/team UX, shard assignment,
and managed lifecycle orchestration. If the control plane is unavailable,
existing applied server runtime state continues to authorize; new managed
lifecycle changes wait until delivered and applied locally.

The current implementation is in transition: personal canonical files may still
live under `data/users/{user_id}/...`, but the logical owner is the Personal
Task Scope, not the Runtime User row itself. The pre-beta migration path is to
introduce a `task_scopes` table and populate one Personal Task Scope per
existing Runtime User, then attach task-key allocation, redirect/event-log scope
state, canonical storage ownership, and projection state to `task_scope_id`.

### Storage stance

The server keeps canonical source truth separate from the merged TW projection.
For personal-only beta:

```text
data/users/{user_id}/                    # existing personal canonical source replica
data/users/{user_id}/merged/             # durable TW-visible merged projection state
  replica/taskchampion.sqlite3           # merged plaintext projection replica/cache
  sync.sqlite                            # merged TC protocol version chain
```

The path names can be adjusted during implementation if a TaskChampion API
requires a directory boundary, but the separation is normative: the merged chain
is not the source of truth, and a source replica is not served directly to TW.
Future storage manifests and backup/restore flows must be Task Scope-aware:
Personal Task Scope canonical DBs, Team Task Scope canonical DBs, per-user merged
sync DBs, task-key allocation/redirect state, membership/RBAC metadata, and
event logs are distinct concerns. A user-scoped restore restores the Runtime
User's Personal Task Scope and runtime/device state and rebuilds that user's
merged projection; it must not mutate shared Team Task Scopes.

The merged chain is durable, not disposable. A TW client stores a base version;
rebuilding the chain underneath that client is a recovery event, not ordinary
cache eviction.

### Codec ownership

The gateway needs a codec for TaskChampion history segments. TaskChampion's
serialized operation shape is currently private (`pub(crate)` upstream), so the
server must choose and record one ownership path before beta:

1. own the de facto codec internally with a compatibility corpus and a
   TaskChampion-version bump gate;
2. use an upstream public history-segment/sync-op API if one exists in time;
3. vendor/fork the relevant codec;
4. reject the gateway before beta if none of the above is acceptable.

For server#143 Phase 2, option 1 is chosen: cmdock owns an internal de facto
codec in `merged_sync_gateway::codec`, guarded by a TaskChampion-generated
fixture corpus and a local/CI version-bump check. `tests/fixtures/tc_history/`
contains history segments captured from public TaskChampion APIs plus
hand-written fail-closed fixtures. `CODEC_REVIEW.toml` acknowledges the reviewed
TaskChampion crate version; `just codec-gate` fails if `Cargo.lock` resolves a
new `taskchampion` version before the corpus/review acknowledgement is updated.

Because this is a private upstream wire shape, `merged_sync_gateway::codec` is
the only module allowed to mirror or parse the raw JSON operation keys. The
minimum corpus covers: create, delete, update set, update clear, tags,
dependencies, annotations, UDA ops, multi-op same-task, multi-op multi-task, and
unknown/new variant policy.

### Snapshot and retention semantics

Phase 7 treats client-uploaded snapshots as durable merged-chain cache entries,
not as source truth. A snapshot is accepted only for an existing merged version
and only monotonically (same or newer chain sequence than the current snapshot).
The canonical source replicas remain authoritative; the snapshot is an
optimization/reset point for TaskChampion clients.

The latest accepted snapshot is the fresh-clone authority. A fresh client may
load that snapshot, then replay retained merged versions whose parent is the
snapshot version. Retention therefore never prunes versions after the latest
snapshot. Delete/corrective versions after a snapshot are retained until a newer
post-delete/post-correction snapshot exists.

Production GC is conservative: by default the server keeps at least 10,000
latest merged versions and has no age-based deletion path, which is stronger
than the beta requirement of 30 days or 10,000 versions, whichever retains more.
When history before the retained boundary has been pruned, `get-child-version`
for a stale parent returns `410 Gone`; clients recover by fetching the latest
snapshot rather than requiring operator DB reset.

### Personal concurrency semantics

For personal-only Phase 8, inbound TW `Update` operation timestamps are preserved
when applying source operations. If a rebased inbound update targets a property
whose source history already has a newer timestamp, the older update is skipped
for source truth while the accepted merged-chain version remains durable. Equal
timestamps are deterministic: the later rebased accepted version wins. Future
ordinary-property timestamps are honoured by the same rule. `cmdock_task_scope`
and `cmdock_key` remain server-owned/corrective inputs regardless of timestamp.
During the transition, the as-built personal-only UDA spelling may still be
`cmdock_account`; it is compatibility state for the prefix and must not be
reinterpreted as the control-plane Hosted Account or Organisation identity.

### Forward-only recovery

Returning success from `add-version` advances the TW client's base version. The
server cannot later un-accept that version. The gateway therefore uses a durable
forward-only journal.

The journal state machine is conceptually:

```text
received
  -> merged_version_accepted
  -> source_plan_applied
  -> projection_appended
  -> finalized
```

Failures after an accepted merged version must recover by replaying source work,
appending corrective merged-chain operations, or quarantining the user with
operator diagnostics. Rewriting client history is not an allowed recovery path.

The implementation may refine state names, but it must preserve these
properties:

- journal before any action whose outcome must survive restart;
- every accepted version has an operator-visible terminal state;
- stale finalizers are guarded by attempt identifiers or equivalent monotonic
  state checks;
- recovery is idempotent;
- fault injection covers every journal transition before beta.

### Personal-only beta gates

Because the gateway is day-one runtime architecture, these are beta-blocking
even with no teams:

1. codec ownership + corpus + TaskChampion-version bump gate;
2. inbound journal and forward-only recovery/quarantine diagnostics;
3. two-TW-client personal concurrency, REST-vs-TW races, offline queues,
   equal/future timestamp behavior, and 409/rebase behavior;
4. merged-chain snapshot/GC/fresh-clone/stale-client behavior;
5. audit/metrics/task-event correlation for gateway-originated source writes;
6. standalone identity issuance for self-host that produces the same
   client-facing sync identity shape as managed onboarding.

Teams-v1 gates remain deferred: RBAC rejection UX beyond personal ownership,
membership churn, cross-account move journal, and team fan-out/write
amplification.

## Consequences

### Positive

- The runtime has one TW sync architecture from beta onward.
- The hard gateway primitives are exercised on the simplest projection before
  teams add policy complexity.
- ADR-0002 boundaries remain explicit instead of turning `tc_sync::handlers` or
  `sync_bridge.rs` into mega-orchestrators.
- The codec and recovery risks are visible beta gates rather than hidden future
  migration work.

### Negative

- Pre-beta server scope grows materially.
- Personal-only users absorb gateway risk before seeing a teams feature.
- The implementation cannot be a small patch; it is a new sync subsystem.
- Existing bridge/runtime code must be carefully separated into reusable
  primitives versus old product path.

### Neutral / transitional

- Feature-branch scaffolding may temporarily run old and new code side by side
  for tests and cutover, but the released runtime should not expose both as
  operator-selectable TW sync modes.
- Dogfood migration is manual re-onboarding, not in-place chain rewriting.

## Rejected Alternatives

### Put the gateway inside `tc_sync::handlers.rs`

Rejected. The handler already owns HTTP/protocol adaptation. Adding operation
codec, journal, source routing, projection, recovery, audit, and event semantics
would make it the exact de facto orchestrator ADR-0002 says should shed
responsibilities.

### Extend `sync_bridge.rs` into sync bridge v2

Rejected. The sync bridge coordinates REST canonical replica changes with TC
sync storage. The gateway has a different responsibility: accepting TW-originated
merged-chain writes, authorizing/routing them to source truth, and recovering
forward after the TC ack boundary.

### Keep old personal sync as a supported self-host option

Rejected by ADR-0012. With no known installed base, carrying two TW sync
architectures into beta is more complex than cutting over the two dogfood users
and shipping one architecture.

## Cross-cutting checklist

- **Cross-repo contracts:** ADR-0012 governs the runtime decision. Task-write
  contract amendments are required before teams-era `cmdock_task_scope` move
  semantics expand beyond personal-only.
- **Audit/task event log:** gateway-originated source writes must emit the same
  task event/audit story as REST writes, with TW version/journal correlation.
- **Admin surfaces:** standalone self-host identity issuance is beta-blocking;
  operator diagnostics for journal/recovery state are required.
- **Documentation:** setup/operator manuals must stop describing the prior TW
  sync path once the gateway is the released path.
- **OpenAPI/Swagger:** admin/operator identity and diagnostics surfaces need
  schema coverage when introduced. TC sync protocol wire contract remains the
  TaskChampion protocol.
- **Metrics:** codec failures, journal states, recovery/quarantine outcomes,
  projection latency, snapshot urgency, and stale-client/GC failures need
  counters or histograms.
- **Testing:** corpus tests, fault-injection recovery tests, two-client
  concurrency harness, snapshot/GC tests, and dogfood cutover smoke are required
  before beta.

## References

- `cmdock/architecture` ADR-0012: Merged Sync Gateway As Day-One Runtime TW Sync Architecture
- server#143: MergedSyncGateway as day-one runtime TW sync architecture
- server#139: TW CLI single-auth merged-replica feasibility spike
- ADR-0001: Sync Bridge — Unifying REST API and TaskChampion Sync Protocol
- ADR-0002: Design Simplicity Principles
