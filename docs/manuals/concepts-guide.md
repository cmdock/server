# Concepts Guide

This guide explains how `cmdock-server` is put together and how its main runtime pieces relate to each other.

It is meant to answer questions like:

- What data lives where?
- Why are there both REST and TaskChampion sync endpoints?
- What is the canonical replica?
- Why does each device get its own `client_id`?
- What does the bridge scheduler actually do?

For install and deployment steps, see the [Installation and Setup Guide](installation-and-setup-guide.md). For operator tasks, see the [Administration Guide](administration-guide.md). For backup and restore procedures, see the [Backup and Recovery Guide](backup-and-recovery-guide.md). For the full manual set, see [Documentation Library](index.md).

## Glossary

The glossary below is the authoritative server-local source for runtime and
product terminology used by `cmdock/server`.

- **Admin surface**
  The server-facing operator interface, currently the admin CLI and admin HTTP endpoints.

- **API token**
  A bearer token used to authenticate a REST client to the server. The server stores only a hashed form in the config database.

- **AppState**
  The shared runtime state object passed through the Axum application. It holds references to core server services such as the config store, replica manager, bridge scheduler, caches, and configuration.

- **Auth cache**
  An in-memory cache used to reduce repeated authentication lookups against the config database for bearer-token-authenticated REST traffic.

- **Backup staging directory**
  The local filesystem directory configured by `backup_dir` where the server writes timestamped backup snapshots for later off-host copying and restore.

- **Bridge freshness**
  The server’s internal view of whether the shared TaskChampion sync DB is already caught up with the current canonical replica state for a given user/device context.

- **Bridge scheduler**
  The in-process background scheduler that queues and coalesces bridge reconciliation work instead of doing all bridge sync inline in request handlers.

- **Canonical replica**
  The per-user plaintext TaskChampion replica stored as `taskchampion.sqlite3` and used as the primary source of truth for the REST API.

- **Canonical sync identity**
  The user-level sync identity created by `admin sync create`. It is distinct from individual device credentials.

- **Client ID**
  The unique identifier assigned to a sync client. In the current architecture, each registered device gets its own `client_id`.

- **Config database**
  The server metadata database stored in `config.sqlite`. It contains users, tokens, views, contexts, presets, device rows, and other configuration data.

- **ConfigStore**
  The storage abstraction trait for server metadata. The current implementation uses SQLite, but handlers depend on the trait boundary rather than directly on one database backend.

- **Device**
  A physical or logical sync client registered for a user, such as an iPhone, laptop, or Taskwarrior profile. Each device has its own `client_id`, status, and derived sync secret.

- **Device chain**
  Historical shorthand for a per-device TaskChampion sync SQLite database under `users/<user-id>/sync/<client-id>.sqlite`. The current runtime keeps per-device identity and crypto, but not one live server-side sync DB per device.
  Normal registration and sync do not create or rely on these files.

- **Device registry**
  The metadata model and operational workflow that tracks which devices belong to a user and whether they are active or revoked.

- **Device secret**
  The per-device sync secret derived from the user’s canonical secret and used to encrypt that device’s TaskChampion sync traffic.

- **Delivery history**
  The recent persisted record of webhook delivery attempts, including retries
  and failure/success outcome for one webhook target.

- **Master key**
  The server-side key used to encrypt sync secrets at rest so the server can escrow and later decrypt them for bridge operations.

- **Operator webhook**
  An admin/per-server webhook configured under `/admin/webhooks` for
  environment-wide event delivery rather than a single authenticated user's
  event stream.

- **Operator token**
  The bearer token used to authenticate operator requests to the admin HTTP surface. This is distinct from per-user API tokens used by runtime REST clients.

- **Per-device auth**
  The TaskChampion sync auth model where `X-Client-Id` is resolved through the device registry rather than a shared per-user sync identity.

- **ReplicaManager**
  The server component that opens, caches, and evicts canonical TaskChampion replicas for REST-side task operations.

- **Replica**
  In general, a TaskChampion task database. In this server there are two important kinds: the canonical per-user replica and the shared per-user TaskChampion sync database.

- **Quarantine**
  A protective state entered when corruption is detected for a user. Requests for that user fail fast and cached state is evicted until recovery action is taken.

- **REST surface**
  The bearer-token-authenticated HTTP API used by the iOS app and other first-party REST clients.

- **Safety snapshot**
  An automatic `pre-restore-*` backup snapshot created immediately before a full snapshot restore so the server can roll back to the pre-restore live state if restore fails partway through.

- **Sync bridge**
  The translation layer that reconciles canonical replica state with the shared per-user TaskChampion sync database while translating payloads for each device’s credentials.

- **Sync database**
  A SQLite file used by the TaskChampion sync protocol. In the current architecture this usually means the shared per-user `users/<user-id>/sync.sqlite`.

- **SyncStorage**
  The low-level storage wrapper around a TaskChampion sync SQLite file. It implements the version-chain and snapshot operations used by the TaskChampion sync handlers.

- **Sync storage manager**
  The server component that caches open shared per-user sync SQLite connections for TaskChampion sync handling.

- **SyncInFlight**
  The per-user synchronisation guard that prevents the server from running overlapping bridge sync work for the same user at the same time.

- **User webhook**
  A webhook configured under `/api/webhooks` by an authenticated user for that
  user's own task event stream.

- **Webhook scheduler**
  The in-process background poller that scans for time-driven webhook events
  such as `task.due` and `task.overdue` and emits them without making ordinary
  task reads or writes carry that timing responsibility inline.

- **TaskChampion sync surface**
  The `/v1/client/*` API used by Taskwarrior-compatible clients speaking the TaskChampion sync protocol.

- **Token**
  Usually shorthand for an API token unless otherwise specified. It authenticates a REST client, not a TaskChampion sync device.

- **User**
  The top-level account owner in the server. Users own API tokens, views, config, one canonical replica, and zero or more registered devices.

## 1. System Overview

`cmdock-server` is a single Rust process with two main external surfaces:

- A REST API used by the cmdock iOS app and other first-party clients.
- A TaskChampion sync API at `/v1/client/*` used by Taskwarrior-compatible clients.

Internally, the server keeps one canonical plaintext task database per user and one encrypted shared TaskChampion sync database per user. Each registered device still has its own `client_id` and derived secret, and the server translates between those device credentials and the shared sync DB.

At a high level:

```text
REST / iOS client
        |
        v
  Canonical replica (plaintext)
        ^
        |
 Sync bridge + scheduler
        |
        v
Shared per-user TC sync DB (encrypted)
        ^
        |
Taskwarrior / TC clients
```

Separately, the server keeps a config database that stores users, auth tokens, views, contexts, presets, and device metadata.

The operator surface is separate again:

```text
Authenticated user            Operator
      |                         |
      v                         v
  /api/*, /v1/client/*       /admin/*
      |                         |
      +-----------+-------------+
                  |
                  v
             cmdock-server
```

## 2. The Two Persistence Layers

The server has two distinct persistence concerns.

### 2.1 Config Database

`config.sqlite` stores server-managed metadata:

- users
- API tokens
- views
- contexts
- presets
- stores
- canonical sync identity metadata
- registered devices

This database is operational metadata, not the user’s task history itself.

### 2.2 Task Data

Each user has their own TaskChampion data directory under `data/users/<user-id>/`.

That directory contains:

- `taskchampion.sqlite3`
  - the canonical plaintext replica used by REST and iOS
- `sync.sqlite`
  - the shared encrypted TaskChampion sync DB used by Taskwarrior-compatible clients
- optionally `sync/<client-id>.sqlite`
  - legacy or maintenance-time artifacts that are not part of the normal hot path

This split is intentional:

- REST and iOS want a direct, canonical task view.
- Taskwarrior-compatible clients want native TaskChampion sync semantics.

### 2.3 How a Taskwarrior Client Actually Works

This is the key detail that makes the rest of the sync architecture make sense.

A Taskwarrior client is not a thin remote UI over the server’s canonical database.

Instead:

- the client has its own local TaskChampion task database
- local commands read and write that local database first
- `task sync` then exchanges TaskChampion protocol data with a sync server

What the sync protocol moves is not “run this SQL against the canonical DB.”

It moves TaskChampion sync state:

- history segments
- version links
- snapshots
- per-client sync metadata

So from the point of view of a Taskwarrior-compatible client, the server is not exposing a canonical SQL database. The server is exposing a TaskChampion sync server.

That distinction matters because the protocol is replica-oriented, not request/response CRUD in the REST sense.

### 2.4 Why the Server Keeps TaskChampion Sync Databases

The server keeps a TaskChampion sync SQLite file because it needs somewhere to store TaskChampion protocol state in the shape the client expects.

That file is not just an arbitrary cache. It holds the protocol-facing sync state:

- versions
- parent version links
- snapshots
- sync metadata used by the protocol

That lets the server behave like a proper TaskChampion sync endpoint for `task sync`, while still keeping the canonical REST state separate.

## 3. Canonical Replica vs Shared Sync DB

The canonical replica is the server’s source of truth for task state as exposed through the REST API.

Properties of the canonical replica:

- plaintext
- one per user
- used by REST CRUD handlers
- optimized for direct server-side reads and writes

The shared sync DB exists to support TaskChampion clients without making every client share one mutable credential.

Properties of the shared sync DB:

- encrypted
- one per user
- used by the TaskChampion protocol endpoints
- not the REST source of truth

Properties of device identity remain per device:

- each device has its own `client_id`
- each device has its own derived secret
- each device can be individually revoked

This means the server is effectively translating between:

- canonical user state
- per-device sync credentials at the HTTP edge
- one shared sync transport state on disk

### 3.1 Why We Do Not Just Expose the Canonical DB to Taskwarrior

It is tempting to ask:

- why not just use the canonical `taskchampion.sqlite3` for everything?
- why also keep `sync/<client-id>.sqlite` files?

The short answer is that the canonical replica and the TaskChampion sync chain serve different purposes.

The canonical replica is optimized for:

- direct REST reads
- direct REST writes
- server-side task manipulation
- app-facing “current state” semantics

The TaskChampion sync chain is optimized for:

- protocol-native `task sync`
- version-chain traversal
- snapshot exchange
- per-device encrypted transport

The server could not simply hand the canonical SQLite file to every Taskwarrior client because that is not how the client protocol works.

The client expects:

- a sync server
- identified by its own `client_id`
- able to accept and return TaskChampion protocol blobs
- with version and snapshot semantics matching the protocol

That is a different contract than “here is the one canonical task database.”

### 3.2 Why The Runtime Uses One Shared TaskChampion Sync DB

What we need for revocation is:

- one `client_id` per device
- one derived secret per device
- per-device auth and revocation

That does **not** require one server-side sync DB per device.

During implementation, the server tried a per-device storage model. It looked clean on paper, but it broke an important convergence property: a single canonical TaskChampion replica cannot reliably relay device-originated changes across multiple independent server-side chains and still propagate those changes to other devices.

So the runtime was corrected to use:

- one shared per-user `sync.sqlite`
- per-device auth and per-device envelope translation at the HTTP boundary

This gives the right blast-radius reduction for revocation without the broken per-device relay model.

### 3.3 The Real Role of the Shared Sync DB

It helps to think of the shared sync DB as the protocol-facing store for that user.

The canonical replica answers:

- what is the user’s current task state for REST and server-side operations?

The shared sync DB answers:

- what TaskChampion sync state does this user need to exchange with the server?

Those are related questions, but they are not the same question.

That is why the current architecture keeps both:

### 3.4 Webhook Delivery Model

Webhook delivery is a separate outward-facing side effect of task and sync
activity. The runtime keeps it outside the core task mutation path by routing
events through one delivery orchestration boundary.

```text
task mutation / sync completion / scheduler event
                    |
                    v
            Webhook orchestrator
              |               |
              v               v
      user-scoped webhooks   admin/per-server webhooks
              \               /
               v             v
                 HTTPS delivery targets
```

This split matters because:

- task and sync code emit events, but do not own retry/backoff policy
- user-scoped and operator-scoped webhook configuration stay distinct
- delivery history and disablement are part of the webhook subsystem rather
  than scattered across task handlers

The two webhook surfaces are intentionally different:

- user webhooks under `/api/webhooks`
  - owned by one authenticated user
  - scoped to that user's task stream
  - used for user-level integrations and automation
- admin/per-server webhooks under `/admin/webhooks`
  - owned by the operator surface
  - used for environment-wide integrations
  - configured with the operator bearer token rather than a user token

The current event model comes from three places:

- task mutations such as `task.created`, `task.modified`, `task.completed`, and
  `task.deleted`
- sync completion via `sync.completed`
- time-driven events such as `task.due` and `task.overdue`

Normal request handlers and sync paths do not deliver directly to remote URLs.
They emit into the webhook subsystem, and the delivery runtime owns:

- target selection
- secret handling and request signing
- retry/backoff
- delivery history
- disablement after repeated failures

### 3.5 Webhook Scheduler

Not all webhook events come directly from a user request.

`task.due` and `task.overdue` are time-driven events, so the runtime uses a
separate in-process webhook scheduler to poll for them.

At a high level:

```text
background poll tick
        |
        v
 list users with due/overdue webhooks
        |
        v
 open canonical replica for each user
        |
        v
 inspect pending tasks with due dates
        |
        v
 emit task.due / task.overdue once per task+timestamp
```

This scheduler is separate from the bridge scheduler.

The bridge scheduler exists to coalesce canonical/sync reconciliation work.
The webhook scheduler exists to notice clock-driven task state transitions that
would not otherwise generate a request-time mutation.

The current webhook scheduler model is:

- in-process background poller
- wakes on a fixed interval
- scans only users who have enabled due/overdue-capable webhooks
- reads canonical task state
- records event history so repeated polls do not re-emit the same due/overdue
  event for the same task timestamp

That design keeps time-driven webhook semantics out of ordinary task reads and
writes while still making due/overdue delivery part of the server-owned
runtime.

- canonical state for the app/server world
- shared protocol state for the Taskwarrior sync world

## 4. Why Each Device Has Its Own Client ID

Older sync designs often reused one `client_id` and one sync secret across all devices for a user. That makes revocation too coarse:

- lose one laptop
- rotate that credential
- every other client breaks too

`cmdock-server` uses a device registry so each physical client has:

- its own `client_id`
- its own derived device secret
- its own status (`active` or `revoked`)

That gives the operator a clean lifecycle:

- create device
- list devices
- revoke one device
- unrevoke if needed
- delete later as cleanup

## 5. Authentication Model

The server currently has two auth models.

### 5.1 REST Auth

REST endpoints use bearer tokens.

- token is presented by the client
- server hashes it
- hash is looked up in `config.sqlite`
- request is scoped to one user

### 5.2 TC Sync Auth

TaskChampion sync endpoints use `X-Client-Id`.

- server looks up the device row by `client_id`
- device row maps to the owning user
- revoked devices are rejected
- active devices are allowed to read/write their own sync chain

This is why device registration is a first-class concept rather than just a convenience wrapper.

## 6. Encryption Model

There are two different encryption concerns in the system.

### 6.1 Escrowed Secrets in the Server

The server stores sync secrets encrypted at rest using the configured server master key.

That lets the server:

- issue device credentials
- decrypt device secrets when it needs to bridge sync traffic
- keep secrets out of plaintext storage

### 6.2 TaskChampion Transport Encryption

Each device uses TaskChampion-compatible envelope encryption.

That is what lets a Taskwarrior-compatible client talk to the server using the normal sync protocol while still giving the server control over per-device identity.

## 7. The Sync Bridge

The sync bridge is the translation layer between:

- REST/canonical operations
- TaskChampion sync protocol traffic

Examples:

- REST task create:
  - writes to canonical replica
  - bridge eventually pushes that change into the shared sync DB
- TC device write:
  - is translated into the shared sync DB
  - bridge reconciles that back into canonical state

Without the bridge, the REST world and the TaskChampion world would drift apart.

## 8. Why There Is a Bridge Scheduler

An earlier model did bridge reconciliation inline in request handlers. That caused the request path to pay the cost of:

- fan-out to multiple devices
- SQLite contention
- TaskChampion sync timing

That was acceptable at low volume but became the bottleneck under multi-device and shared-user load.

The current model uses an in-process bridge scheduler:

- request handlers enqueue bridge work
- background workers perform the sync
- duplicate work is coalesced per user

This keeps request latency more predictable while preserving eventual convergence.

### 8.1 Freshness Tracking

The bridge also tracks whether a device is already caught up with the canonical replica.

That lets the server avoid needless targeted sync work on every TaskChampion read.

In practice:

- canonical write marks devices stale
- a successful targeted sync marks the device fresh again

### 8.2 Bridge Scheduler Lifecycle

At a code level, the bridge scheduler is a per-user queued work system.

The main pieces are:

- `BridgeScheduler`
- `UserSyncLane`
- `BridgeSyncContext`
- `SyncInFlight`

The model is:

1. A request handler decides that canonical and device state may have diverged.
2. It calls `bridge_scheduler.schedule(user_id, priority, source)`.
3. The scheduler finds or creates a per-user lane.
4. The lane records the highest pending priority for that user.
5. If no worker is currently running for that user, the scheduler starts one background task.
6. The worker waits for a short debounce window.
7. The worker drains the pending priority and runs one sync for that user.
8. If more work was queued while that sync was running, the loop repeats.

The important property is that this is not an unbounded FIFO of duplicate jobs.

Instead, the scheduler coalesces repeated work for one user into one running lane plus one pending priority value.

### 8.3 Priority and Coalescing

The scheduler currently uses three priorities:

- `Low`
- `Normal`
- `High`

The meaning is operational rather than user-facing:

- `Normal` is typical REST write follow-up work.
- `High` is used when a TaskChampion path wants fast convergence or when the server falls back after an inline bridge failure.
- `Low` is available for opportunistic work.

If multiple schedules happen for the same user before the worker drains them, the scheduler keeps only the highest pending priority.

That means:

- ten REST writes for the same user do not create ten independent bridge workers
- one later high-priority request can upgrade pending work for that user

### 8.4 Why There Is Still a Dedicated OS Thread

The scheduler itself is an in-process async background facility, but the actual bridge sync execution still uses a dedicated OS thread.

That is because the TaskChampion sync path uses a `!Send` future shape through `Replica::sync(...)` and the server cannot safely run that directly in normal Axum request futures.

So the split is:

- scheduler and queue management: Tokio/in-process
- actual sync execution: dedicated OS thread with a current-thread Tokio runtime

This is a pragmatic implementation boundary, not a separate service.

### 8.5 Why `SyncInFlight` Exists

`SyncInFlight` enforces one bridge sync at a time per user.

Without it, bursty traffic could cause:

- many overlapping bridge executions for the same user
- excess thread creation
- more SQLite contention
- worse convergence rather than better convergence

So even if many requests are trying to cause reconciliation, only one bridge execution per user is allowed to run at once.

If another sync is already in progress and the lock cannot be acquired within the configured timeout, the server skips that sync attempt and relies on eventual consistency.

### 8.6 Timeouts and Eventual Consistency

Bridge execution is intentionally bounded.

There are two important timeout decisions:

- the server will not wait forever to acquire the per-user sync lock
- the server will not wait forever for the bridge execution thread to finish

If either times out, the server logs the event, keeps the process healthy, and relies on a later sync attempt.

This is one of the key design tradeoffs in the current runtime:

- favor server responsiveness
- accept eventual convergence rather than strict synchronous convergence

## 9. Detailed Runtime Flows

The easiest way to understand the bridge is to follow the main flows.

### 9.1 REST Write Flow

A typical REST mutation path looks like this:

1. Authenticate the user with a bearer token.
2. Open the canonical replica through `ReplicaManager`.
3. Apply the task mutation to the canonical replica.
4. Record audit and metrics.
5. Mark canonical state as changed in the bridge freshness tracker.
6. Schedule a normal-priority bridge sync for that user.
7. Return the REST response immediately.

The important point is that the REST write does not wait for the TaskChampion sync surface to converge before returning success.

That is the scheduler’s job.

### 9.2 REST Read Flow

A normal REST read now stays on canonical state.

The server:

1. authenticates the user
2. opens the canonical replica
3. reads directly from canonical state
4. returns the response

It does not synchronously pull the sync surface first.

That change was important for performance. Earlier bridge models paid too much cost on read paths.

### 9.3 TaskChampion Write Flow

A TaskChampion device write is stricter than a REST write in one important way: the payload itself must be valid for that device before the server stores it.

The current shape is:

1. Authenticate the device via `X-Client-Id`.
2. Load the device record from the registry.
3. Verify the device is active.
4. Verify the device has stored secret material when bridge mode is enabled.
5. Validate that the encrypted payload can actually be decrypted for that device/version context.
6. Translate the payload into the shared sync DB.
7. Attempt immediate reconcile of shared sync state back into canonical state.
8. If the immediate reconcile fails with a non-corruption error, queue high-priority bridge work and still return success.
9. If corruption is detected, return failure and quarantine as needed.

That means the server currently distinguishes between:

- invalid device payloads
  - rejected immediately
- valid writes followed by temporary bridge trouble
  - accepted, then reconciled asynchronously if needed

### 9.4 TaskChampion Read Flow

TaskChampion reads are device-aware.

When a device asks for child versions or a snapshot, the server:

1. authenticates the device
2. checks whether that device is marked stale relative to canonical state
3. if not stale, reads directly from the shared sync DB
4. if stale, attempts reconcile for that user before serving the read
5. if targeted sync fails, queues high-priority background sync and continues with eventual consistency semantics

This is where bridge freshness matters most.

The goal is to avoid paying sync cost for every TC read when the device is already caught up.

### 9.5 Multi-Device Freshness Example

Suppose a user has:

- device A
- device B
- canonical REST state

If device A writes new TaskChampion data:

1. the shared sync DB changes through device A’s credentials
2. canonical state is reconciled
3. device A is marked synced to the new generation
4. device B remains behind that generation and is therefore stale

Then when device B next reads:

1. the server sees that device B is stale
2. it attempts targeted reconciliation for device B
3. on success, device B is marked fresh

This lets the server track freshness per device rather than treating all devices as equally current.

### 9.6 Why REST and TC Paths Behave Differently

The server treats REST and TC differently because they represent different product concerns.

REST:

- is the primary app-facing surface
- reads canonical state directly
- favors low latency and predictable request behavior

TaskChampion sync:

- is a device-specific transport protocol
- needs device identity and device secrets
- sometimes needs targeted catch-up before serving the next read

So while both surfaces manipulate the same logical user task universe, they are not implemented as identical request pipelines.

## 10. Consistency Model

The server is not fully synchronous across every layer.

The intended model is:

- canonical REST state is immediately authoritative for REST clients
- shared sync state converges shortly after
- TaskChampion reads may perform targeted reconciliation when needed

This is eventual consistency between surfaces, with canonical REST state as the main operational source of truth.

That tradeoff is deliberate. It avoids putting bridge fan-out directly on the hot request path.

### 10.1 What “Source of Truth” Means Here

In the current runtime, “source of truth” does not mean that only one storage file exists.

It means:

- the canonical replica is the authoritative state for REST behavior
- the shared sync DB is transport-specific state that must converge with canonical state

This is why operator reasoning should usually start with the canonical replica, then treat the sync DB as managed protocol state around it.

## 11. Corruption and Quarantine

SQLite corruption is treated as a serious operational event.

If corruption is detected for a user:

- the user is quarantined
- cached replica connections are evicted
- sync storage connections are evicted
- further requests for that user fail fast with `503`

This prevents the server from continuing to operate on suspect files and makes the failure mode explicit to operators.

Recovery is then an operator workflow:

- inspect
- restore from backup if needed
- bring the user back online

### 11.1 What Gets Evicted on Quarantine

When quarantine is triggered for a user, the server evicts the cached state that could otherwise keep touching damaged files:

- canonical replica cache entries
- shared sync storage cache entries
- bridge freshness state
- related bridge cryptor cache entries

This keeps the failure mode explicit and reduces the chance of continuing to operate on suspect on-disk state.

### 11.2 Recovery State Assessment

The recovery mindset for this server should be similar to a traditional database startup assessment:

- determine what state exists on disk
- determine whether that state is safe to use
- determine what can be repaired automatically
- determine what requires operator intervention

In this server, that means thinking in terms of:

- config DB coherence
- canonical replica coherence
- shared sync DB presence and openability
- whether missing sync state is rebuildable from canonical state
- whether a user should be brought online normally, brought online in a degraded or rebuildable state, or left quarantined

The important point is that “all SQLite files open” is not the full recovery question.

The real question is:

- is the user’s runtime state coherent enough to resume service safely?

### 11.3 Two Recovery Modes

Operationally, the server has two distinct recovery modes.

#### Startup recovery

This is the case where the process is starting or restarting and must assess
on-disk state before resuming normal service.

The current implementation follows this shape:

1. open core metadata
2. discover relevant users and devices
3. assess canonical and sync state
4. classify each user as healthy, rebuildable, or needing intervention
5. keep already-offline users offline
6. place `needs_operator_attention` users offline before serving requests
7. leave `rebuildable` users online but visible to operators

#### Online per-user recovery

This is the case where the server remains running, but one user is being selectively restored or repaired.

This is a different operational problem because live state may still exist in memory:

- cached canonical replica handles
- cached sync storage handles
- bridge freshness state
- queued or in-flight bridge work
- active client traffic

So online recovery for one user should be treated as a coordinated state transition, not just a file copy.

### 11.4 Why Online Selective Restore Needs Coordination

If an operator restores one user’s files while the server remains running and does not coordinate that restore, the server may still be holding:

- open handles to the old canonical replica
- open handles to old sync DB files
- freshness information that no longer matches disk
- queued bridge work based on pre-restore state

That can produce confusing post-restore behavior even if the restored files themselves are valid.

So the safe conceptual model is:

1. take that user offline
2. evict that user’s cached runtime state
3. restore files
4. reassess that user’s consistency and rebuildability
5. only then bring the user back online

### 11.5 Offline Marker and Runtime Coordination

The current implementation uses a simple persisted coordination primitive for
per-user recovery work:

- `users/<user_id>/.offline`

That file is the server's operator-controlled "this user is offline" marker.

What happens when it exists:

- the server treats the user as quarantined/offline
- REST requests for that user fail fast with `503`
- TaskChampion sync requests for that user fail fast with `503`
- cached canonical replica handles are evicted
- cached sync DB handles are evicted
- bridge freshness state is cleared

What happens when it is removed:

- the user can be brought back online
- the next request reopens replica and sync state from disk

This is important because it gives the local admin CLI a way to coordinate with
a still-running server without having to write directly into in-memory process
state. The CLI writes or removes the offline marker; the running process notices
that state change and converges to the correct runtime behaviour.

Operationally, that means:

- `admin user offline <user-id>` is not just a log message
- it creates persisted recovery state that survives process restarts
- `admin user online <user-id>` clears that persisted state
- selective restore is therefore a real coordinated workflow, not a convention

## 12. Admin Surfaces

There are two distinct admin concepts.

### 12.1 `admin sync`

This manages the user’s canonical sync identity.

Use it once per user.

### 12.2 `admin device`

This manages physical device lifecycle.

Use it for:

- create
- list
- taskrc rendering
- rename
- revoke
- unrevoke
- delete

The important conceptual split is:

- `sync` is user-level bootstrap
- `device` is per-client lifecycle

## 13. Self-Hosted vs Managed Control Plane

The open server is intended to remain fully functional for self-hosters.

That means a self-hosted operator can:

- create users
- create sync identity
- create devices
- retrieve per-device credentials
- revoke or delete devices
- back up and restore the system

A future admin UI can add convenience:

- invite links
- QR flows
- team admin workflows
- polished remote onboarding

But those are an operator-automation and UX layer, not the core runtime model.

## 14. Mental Model Summary

If you remember only a few things, remember these:

- `config.sqlite` stores metadata, not the actual task replica state
- `taskchampion.sqlite3` is the canonical per-user plaintext task store
- each device has its own `client_id` and derived secret
- the bridge keeps canonical state and shared sync state aligned
- the scheduler exists so bridge work does not dominate request latency
- revocation is per device, not per user
- backup must include both config metadata and all user replica files
