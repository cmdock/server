# Sync Bridge Reference

This document explains the internal bridge model used to reconcile the
canonical per-user TaskChampion replica with the shared per-user TaskChampion
sync DB while preserving per-device credentials at the protocol edge.

Use this document when reasoning about:

- why REST and TaskChampion requests behave differently
- why some sync work is inline while other work is queued
- what the bridge scheduler is allowed to delay or coalesce
- where current scaling pressure comes from

For the broader architecture, see
[Concepts Guide](../manuals/concepts-guide.md).

## 1. Purpose

The server maintains two related task-state layers:

- the canonical per-user replica
- the shared per-user TaskChampion sync DB

The bridge exists because those layers serve different purposes:

- the canonical replica is the server's authoritative task state for REST
- the shared sync DB is the protocol-facing state for Taskwarrior /
  TaskChampion clients

The bridge keeps those two layers close enough that:

- REST clients see current canonical task state
- TaskChampion clients can keep syncing with their own device credentials

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

The shared sync DB lives at:

- `users/<user_id>/sync.sqlite`

It stores the TaskChampion sync protocol state for that user:

- version chain
- snapshots
- opaque client payloads

These DBs are not the main server-side source of truth for REST.

## 3. Scheduler Model

The bridge scheduler is in-process.

It exists to keep bridge fan-out out of the normal REST hot path.

At a high level:

- request handlers enqueue bridge work
- the scheduler coalesces work per user
- background execution performs the actual reconciliation

This means bridge work is treated as scheduled runtime work, not just as
synchronous helper logic inside request handlers.

## 4. Priorities

The scheduler uses per-user coalescing with a priority model.

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

## 5. Freshness Tracking

The bridge freshness tracker answers a targeted question:

- is the shared sync DB already known to be caught up with canonical state for this device context?

That allows the server to skip redundant targeted bridge work for TaskChampion
reads when a device is already fresh.

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

After a successful canonical mutation, the runtime schedules bridge work instead
of blocking the response on full device reconciliation.

Architectural boundary note:

- task CRUD should not need to know bridge queue policy directly
- the current narrow boundary is `RuntimeSyncCoordinator::note_canonical_change(...)`
- that keeps direct bridge scheduling policy out of `src/tasks/`

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
