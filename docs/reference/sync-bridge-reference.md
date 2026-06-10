# Sync Bridge Reference

> **Merged-gateway note:** `/v1/client/*` is now served by
> `MergedSyncGateway` and the gateway sync DB at
> `users/<user_id>/merged/sync.sqlite`. This reference documents the legacy
> bridge/scheduler primitives that may still exist as lower-level internals;
> it is not the operator-facing Taskwarrior sync runtime architecture.

This document explains the internal bridge model originally used to reconcile the
canonical per-user TaskChampion replica with the legacy shared per-user
TaskChampion sync DB while preserving per-device credentials at the protocol
edge.

Use this document when reasoning about:

- historical bridge behaviour and remaining lower-level primitives
- why REST and TaskChampion requests used to behave differently before the merged-gateway cutover
- why some legacy sync work was inline while other work was queued
- what the bridge scheduler was allowed to delay or coalesce
- where historical bridge scaling pressure came from

For the broader architecture, see
[Concepts Guide](../manuals/concepts-guide.md).

## 1. Historical Purpose

Before the merged-gateway cutover, the server maintained two related task-state
layers:

- the canonical per-user replica
- the legacy shared per-user TaskChampion sync DB

The bridge existed because those layers served different purposes:

- the canonical replica was the server's authoritative task state for REST
- the shared sync DB was the protocol-facing state for Taskwarrior /
  TaskChampion clients

The bridge kept those two layers close enough that:

- REST clients saw current canonical task state
- TaskChampion clients could keep syncing with their own device credentials

## 2. Canonical vs Shared Sync DB

### Canonical replica

The canonical replica lives at:

- `users/<user_id>/taskchampion.sqlite3`

It is the main server-side task database for:

- REST reads
- REST writes
- server-side task logic
- operator inspection of current task state

### Shared sync DB

The legacy shared sync DB lived at:

- `users/<user_id>/sync.sqlite`

The served gateway sync DB now lives at:

- `users/<user_id>/merged/sync.sqlite`

It stores the TaskChampion sync protocol state for that user:

- version chain
- snapshots
- opaque client payloads

These DBs are not the main server-side source of truth for REST.

## 3. Historical Scheduler Model

The bridge scheduler is an in-process legacy primitive.

It existed to keep bridge fan-out out of the normal REST hot path.

Historically:

- request handlers enqueued bridge work
- the scheduler coalesced work per user
- background execution performed the actual reconciliation

After the merged-gateway cutover, `/v1/client/*` traffic is served by
`MergedSyncGateway`; REST writes mark gateway devices stale for read-triggered
projection instead of relying on the scheduler as the serving runtime contract.

## 4. Historical Priorities

The scheduler used per-user coalescing with a priority model.

Conceptually:

- `high`
  - protocol-critical or near-critical reconciliation
  - typically device-targeted work after TaskChampion activity
- `normal`
  - canonical push work after REST mutations
- `low`
  - best-effort freshness work

The important invariant is:

- one user should not accumulate an unbounded queue of duplicate sync jobs

Instead, repeated work for one user collapses into:

- one in-flight job
- one remembered pending priority/reason if more work arrives while the first
  job is running

## 5. Freshness Tracking — Still Live

`BridgeFreshnessTracker` remains a live merged-gateway primitive even though the
legacy bridge scheduler is no longer the served `/v1/client/*` runtime. It
answers a targeted question:

- does this device need a read-triggered merged-gateway projection before the
  current TC read is served?

Freshness is cleared when relevant state changes, for example:

- canonical state changes and other devices become stale
- a user is quarantined/offlined
- caches are evicted during recovery

The freshness tracker is a performance optimisation, not a source of truth.

## 6. Request-Path Behaviour

### REST reads

REST reads operate on the canonical replica.

They do not synchronously pull the sync surface first.

Why:

- that made one REST read scale with device count
- it turned bridge fan-out into user-facing latency
- load tests showed this was the main bottleneck for multi-device users

### REST writes

REST writes commit to canonical state first.

Before gateway cutover, after a successful canonical mutation the runtime
scheduled bridge work instead of blocking the response on full device
reconciliation.

Gateway-era architectural boundary note:

- task CRUD should not need to know sync/projection policy directly
- the current narrow boundary is `RuntimeSyncCoordinator::note_canonical_change(...)`
- that boundary now marks devices stale for read-triggered gateway projection instead of warming the legacy `users/<user_id>/sync.sqlite` chain

### TaskChampion writes

TaskChampion writes arrive under the device's own credentials first.

The server validates the encrypted device envelope before accepting the write.

After a valid write:

- targeted reconciliation toward canonical state is attempted
- if the failure is operational rather than corruption, the runtime may degrade
  to queued high-priority bridge work instead of surfacing the pressure as a
  protocol-format error

### TaskChampion reads

TaskChampion reads use the shared sync DB, then re-encrypt responses for the requesting device.

If the device is already marked fresh:

- the bridge is skipped

If it is stale:

- targeted reconcile can run before the read completes

## 7. Inline vs Queued Work

The bridge does not use one single policy for all traffic.

### Inline work is used when:

- protocol correctness would otherwise be compromised
- the operation is tightly scoped to one user / one device-auth context

### Queued work is used when:

- the operation is fan-out style maintenance
- freshness can be eventual without breaking the API contract
- doing the work inline would push bridge cost into user-facing latency

This split is deliberate.

## 8. Execution Threads

The scheduler is in-process, but actual bridge execution still has to respect
TaskChampion / SQLite constraints.

In practice that means:

- scheduling state is async/in-process
- some bridge execution still uses dedicated blocking / OS-thread style work

This is an implementation constraint, not a separate distributed system.

## 9. Failure Semantics

The bridge distinguishes between:

- corruption
- operational contention / timeouts
- normal conflict and convergence behaviour

### Corruption

Corruption is a quarantine/offline event.

The user is taken out of normal service and cached runtime state is evicted.

### Operational pressure

Examples:

- SQLite contention
- bridge timeout
- transient scheduling pressure

These should degrade the system toward:

- retries
- queued reconcile
- eventual convergence

not silent corruption handling.

### Conflicts

Some sync conflicts are normal and expected in shared or concurrent sync
scenarios.

Those are not treated as corruption.

### Task-key drift

The `cmdock_key` UDA is replicated through TC sync as a normal task
attribute and can therefore be mutated by any device that holds the
encryption secret (a TW CLI user editing the task in `vimtask`, an
erroneous client, etc.). The server is the source of truth for task-key
assignment via `task_key_allocations`; device-side `cmdock_key`
mutations are not authoritative.

After every `replica.sync()` the bridge runs a post-canonical-apply
read-back pass (`src/task_keys/drift.rs::reconcile_drift`) that walks
`cmdock_key`-bearing tasks, batch-looks-up against the allocation
table, and applies a fixed decision table per task: emit a reverse-UDA
op for committed-row drift, finalise + reverse for pending-row drift,
finalise alone for pending-match. The decision table is documented in
the module header.

**Lock-acquisition discipline is the load-bearing implementation
invariant** — the reverse-op `replica.commit_operations(...)` MUST run
under the same `replica_arc.lock()` acquisition as the surrounding
`replica.sync(...)`. Releasing the guard between sync and reverse-op
opens a REST-observable window via the per-user replica mutex (REST
reads serialise on the same lock). The hook is wired inside
`do_sync`'s OS-thread closure for exactly this reason; see the
contract amendment at `cmdock/architecture` commit `7516969` and the
spike doc at
`docs/internal/implementation/server-130-phase5-spike.md`.

**Snapshot symmetry**: `add_snapshot` flows through the same
`replica.sync()` invocation as `add_version` (both feed inputs into
`replica.sync(...)`'s `Server` trait calls). The drift-recovery hook
runs after `replica.sync()` returns regardless of whether ops arrived
via segment append or snapshot apply, so no separate code path is
needed for snapshot drift recovery.

**No-allocation-row case**: the contract mandates SKIP — neither a
reverse op nor a `task.key.drift_recovered` audit entry is emitted.
The operational counter `task_keys_drift_skipped_no_row_total`
captures observability; the Phase 5e backfill orphan-reconciliation
pass handles eventual recovery (allocates fresh-N + emits reverse-UDA
op overwriting the foreign value).

## 10. Current Bottlenecks

The main remaining pressure points are:

- shared-user bridge fan-out
- multi-device same-user targeted reconciliation
- SQLite write contention under heavily concurrent device activity

The isolated single-user path is much healthier than the shared-device path.

## 11. Design Boundaries

The bridge is not:

- a byte-for-byte replica validator
- a full distributed job platform
- a replacement for backup/recovery

The bridge is:

- a reconciliation layer between canonical state and device-facing protocol
  state

## 12. Future Directions

Likely future work includes:

- further narrowing of fan-out behaviour for shared users
- richer queue metrics and visibility
- richer bridge-aware recovery diagnostics and repair hooks
- push-triggered targeted device sync as an optimisation on top of the bridge,
  not a replacement for it
