---
created: 2026-04-01
status: accepted
tags: [architecture, principles, quality]
---

# ADR-0002: Design Simplicity Principles

## Status

**Accepted**

Ported from deckengine ADR-0011, adapted for cmdock-server (Rust/Axum).

## Relationship To Architecture ADR

This ADR is a repo-local application of
`cmdock/architecture ADR-0001: Simplicity As A Cross-Repo Principle`.

It defines how the shared simplicity principle is applied within
`cmdock/server`'s responsibilities and constraints as the open-core runtime
server.

## Context

Complexity in software accumulates through entanglement, not size. Systems become difficult to understand and change not because they have many parts, but because those parts are intertwined in ways that prevent reasoning about them independently.

This is especially important for cmdock-server because:

- **The server bridges two protocols** (REST API + TaskChampion sync) — entangling them creates cascading changes
- **Encryption, auth, and storage are separate concerns** — complecting them leads to "change one, break three" scenarios
- **The ConfigStore trait boundary exists specifically for independence** — SQLite today, Postgres later, without touching handlers
- **Per-user SQLite isolation is a simplicity decision** — one user's corruption can't affect another

This ADR adopts principles from Rich Hickey's "Simple Made Easy" framework,
adapted for practical application in cmdock-server.

## Repo-Specific Interpretation

Within `cmdock/server`, simplicity has a few specific meanings:

- keep the runtime server standalone and open-core coherent
- keep hosted and proprietary product semantics out of the server boundary
- keep REST, sync, storage, auth, and admin concerns separable
- keep change local so new capabilities do not scatter across unrelated modules
- prefer generic contracts that a self-hoster or alternate orchestration layer
  could use in principle

### Key Distinctions

**Simple vs Easy**

| Simple | Easy |
|--------|------|
| Not entangled; concerns are independent | Familiar; convenient; at hand |
| Objective property of the system | Relative to the developer |
| "Can I reason about X without thinking about Y?" | "Do I already know how to use this?" |

**Simple usually means more work upfront.** The simple solution often takes longer to implement than the easy one. That's acceptable — the payoff is in maintenance, debugging, and future changes.

**Complecting** (from Latin *complectere*: to braid together)

The act of intertwining concerns that could be independent. Complecting creates hidden dependencies that:

- Force understanding of multiple concerns simultaneously
- Cause changes to cascade unexpectedly
- Make testing require complex setup
- Reduce reusability

### The Problem

Without explicit criteria, we tend to optimise for ease over simplicity:

- Choosing familiar patterns over appropriate ones
- Adding convenience methods that couple concerns
- Growing functions to handle "just one more case"
- Passing large context objects because it's easier than threading specific values

This works initially but compounds into systems where every change requires understanding everything.

## Decision

We adopt a two-tier approach: **measurable criteria** for objective enforcement, and **review prompts** for judgment-based assessment.

### Core Principles

#### 1. Independence

Concerns should be separable. Each module should have a single reason to change.

**cmdock-server concerns:**

| Concern | Responsibility | Should NOT know about |
|---------|---------------|----------------------|
| Auth (bearer) | Token → user_id resolution | Sync protocol, encryption, tasks |
| Auth (sync) | client_id → user_id resolution | Bearer tokens, REST handlers, encryption |
| Task CRUD | REST task operations | Sync protocol, encryption, device registry |
| Sync protocol | TC version chain read/write | REST handlers, filter engine, views |
| Sync bridge | REST ↔ TC translation | Device identity, auth mechanism |
| Encryption | Key derivation, seal/unseal | Auth, device registry, REST API |
| Device registry | Track physical devices | Encryption keys, sync storage |
| Filter engine | Evaluate TW filter expressions | HTTP, database, sync |
| Config store | DB abstraction (trait boundary) | HTTP handlers, business logic |
| Views/Contexts | User configuration CRUD | Task operations, sync |
| Admin CLI | User/token/backup management | HTTP server, middleware |

**Dependency Evaluation (Independence Lens)**

The question isn't "does a library exist?" but "is depending on it *simpler* than owning the capability?"

| Classification | When | Action |
|---------------|------|--------|
| **Must depend** | Domain expertise (taskchampion, rusqlite, ring) | Document usage boundaries |
| **Should depend** | Framework infrastructure (axum, tokio, serde) | Accept coupling, track upgrades |
| **Evaluate carefully** | Utility libraries with narrow usage | Create issue when introducing |
| **Internalize** | Small surface, disproportionate deps | Replace and remove |
| **Dead weight** | Declared but not imported | Remove immediately |

#### 2. Values Over State

Prefer immutable data flowing through transformations over mutable objects being modified.

**Prefer:**

```rust
let tasks = replica.list_pending_tasks()?;
let filtered = evaluate_filter(&tasks, &filter_expr, now);
let response = filtered.into_iter().map(TaskItem::from).collect();
```

**Avoid:**

```rust
let mut ctx = FilterContext::new(&replica);
ctx.set_filter(filter_expr);
ctx.set_now(now);
ctx.evaluate(); // mutates internal state
let response = ctx.get_results(); // reads internal state
```

**Effectful Boundaries:** Some mutation is unavoidable (SQLite writes, Replica operations, tokio-rusqlite closures). These are effectful boundaries — keep them thin and clearly identified.

#### 3. Minimal Interfaces

Functions should know only what they need.

**cmdock-server application:** The `ConfigStore` trait is the canonical example. Handlers receive `&dyn ConfigStore`, not `&SqliteConfigStore`. They can't call SQLite-specific methods, can't access the connection pool, can't run raw SQL. They only know the trait surface.

#### 4. Change Locality

Adding a new capability should not scatter changes across unrelated modules.

**cmdock-server test:** Adding a new config type (e.g. geofences CRUD) should touch:
1. `store/mod.rs` — trait methods
2. `store/sqlite.rs` — implementation
3. `store/models.rs` — record type
4. New handler module
5. `main.rs` — route registration
6. Migration file + tests

If it also requires changes to auth, sync bridge, admin CLI, or the filter engine — concerns are complected.

#### 5. Parse at Boundaries

Raw data should be parsed into typed structures at system boundaries. Internal code works only with typed domain objects.

**System boundaries in cmdock-server:**

- Axum extractors (`Json<T>`, `Path<T>`) — parse HTTP → typed request
- `tokio-rusqlite` closures — parse rows → typed records
- TC sync handlers — parse `X-Client-Id` header → `Uuid`
- Config loading — parse TOML → `ServerConfig`
- Filter engine — parse filter string → AST → evaluate

**Internal functions should never parse.** If a function takes `&str` and parses it, the parsing should move to the boundary.

### Intentional Complecting

Some complecting is acceptable when the architecture requires it. The
designated orchestrators below are allowed to coordinate multiple
concerns; everything else should respect the import-boundary and
fan-out rules. Each entry names the orchestrator, what it MAY know,
and what it MUST NOT take on. Drift across the "must not" line is a
real ADR-0002 violation regardless of the designation.

- **Legacy sync bridge** (removed as a runtime path by server#143):
  Previously coordinated REST replica, TC storage, encryption, and per-user
  locking. ADR-0009 supersedes this as the served `/v1/client/*` architecture.
  The retained concept is freshness tracking, now owned by
  `RuntimeSyncCoordinator` for merged-gateway read-triggered projection.

- **AppState** (`app_state.rs`): Shared-state container.
  - **May know:** store, config, replica manager, sync storage manager,
    runtime coordinators, webhook transport, idempotency runtime.
  - **Must not own:** business logic, lifecycle policy, auth decisions.
    Construction-time `start(...)` hooks for background workers are
    permitted but should remain thin — push policy down to the
    coordinator being started.

- **`main.rs`**: Wires routes, middleware, OpenAPI.
  - **May know:** every module's public surface for wiring.
  - **Must not own:** any logic that could live behind a route handler
    or a shared service.

- **Runtime recovery coordinator** (`runtime_recovery.rs`,
  `RuntimeRecoveryCoordinator`): Owns per-user offline/quarantine
  state, the offline-marker filesystem, and the per-user/per-device
  runtime cache eviction recipe (replica + sync storage + merged gateway
  state + freshness). Single shared owner of the `evict_user` /
  `evict_device` recipe so callers don't duplicate it.
  - **May know:** replica manager, sync storage manager, merged sync storage
    manager, gateway lock/cache eviction hooks, freshness tracker, tc_sync
    cryptor cache (via free function), startup recovery summary, metrics.
  - **Must not own:** business policy beyond
    quarantine+eviction (e.g. user deletion semantics, device
    lifecycle decisions, auth resolution).
  - **Note:** the module name describes its origin (recovery
    quarantine) but the responsibility now extends to plain runtime
    cache eviction. A future rename is acceptable; not required.

- **Runtime sync coordinator** (`runtime_sync.rs`,
  `RuntimeSyncCoordinator`): Narrow façade over merged-gateway freshness
  tracking. REST mutation paths call `note_canonical_change` to mark TC
  devices stale; the next `/v1/client/*` read triggers gateway projection.
  It must not warm the legacy `users/<id>/sync.sqlite` chain.
  - **May know:** freshness generation state and device freshness markers.
  - **Must not own:** projection, protocol parsing, task CRUD policy, storage
    writes, or device lifecycle decisions.

- **Idempotency runtime** (`idempotency.rs`, `run_idempotent`): The
  three-phase write-ahead state machine specified by
  `cmdock/architecture` `task-write-contract.md` § Idempotency. Runs
  Phase 1 (`pending` insert) → Phase 2 (caller closure) → Phase 3
  (finalize) across the config DB + TC DB boundary, with attempt-id
  guards and lookup-time expiry.
  - **May know:** AppState, audit logging, idempotency store records,
    the closure shape Phase 2 must return.
  - **Must not own:** endpoint-specific policy (e.g. inferring metric
    operation from request_path is fragile and should move to the
    caller). New idempotent endpoints should hand in their own metric
    classification.

- **TaskChampion sync runtime** (`tc_sync/runtime.rs`): Protocol-side
  runtime helpers — currently owns request in-flight metrics and any legacy
  sync-storage cache primitives that remain for maintenance/tests.
  - **May know:** sync storage primitives, corruption classification, and
    quarantine gating.
  - **Must not own:** wire-level protocol parsing (stays in
    `tc_sync/handlers.rs`), webhook side-effects (go through
    `tc_sync::events`), task projection, gateway source-apply policy, or
    cryptographic key derivation/envelope internals.

- **Merged sync gateway** (`merged_sync_gateway/`,
  `MergedSyncGateway`): Day-one TW sync runtime orchestrator introduced
  by ADR-0009 / server#143. Owns the forward-only boundary between the
  TC sync protocol edge and canonical source replicas.
  - **May know:** resolved sync identity, decrypted history-segment
    bytes through a payload/crypto facade, typed decoded TC wire ops
    through its codec boundary, visible source-account scope, source
    write primitives, task-key/account correction primitives, durable
    merged-chain storage/snapshots/projection cursors, gateway journal
    state, audit/metrics/task-event contracts.
  - **Must not own:** bearer auth, sync auth resolution, device
    lifecycle decisions, cryptographic key derivation/envelope internals,
    REST DTOs or `TaskItem` projection, view/filter resolution,
    billing/control-plane/team-management UX, raw SQLite details outside
    store/storage primitives, or generic REST task CRUD validation.
  - **Note:** during the server#143 feature branch, old bridge code and
    new gateway code may coexist as scaffolding. The released beta
    runtime should expose one TW sync path, not a product switch between
    old personal sync and gateway sync.

#### What is NOT a designated orchestrator

The following modules and functions have accreted into de facto
orchestrators but should shed responsibilities, not be blessed.
Listed here so future reviews recognise them as drift, not exceptions:

- `tasks/mutations.rs::finalize_success` — post-commit work (audit +
  webhook scheduler maintenance + webhook delivery + runtime_sync) is
  too much for a finalizer. Webhook scheduler-history maintenance
  should not live in task mutation code.
- `webhooks/scheduler.rs::poll_once_inner` — combines user listing,
  replica open, TC projection, webhook event history, REST `TaskItem`
  conversion, and delivery. Splitting projection out is tracked in
  server#126.
- `app_config/handlers.rs::get_app_config` — aggregating multiple
  resource types into one response is fine; the default-reconciliation
  *ordering* policy embedded in the handler should move into a
  seeding/reconciliation service.
- `admin/services/bootstrap.rs::bootstrap_user_device` — coordinates
  10+ concerns in one method. Connect-config consolidation in
  server#123 is the path to shedding most of them.
- `tc_sync/handlers.rs::add_version` — protocol handler still owns auth +
  content-type + payload translation + audit + metrics + response-header
  policy + boundary logging. Sync-completed webhook emission now goes through
  `tc_sync::events`, and snapshot urgency is computed by the gateway protocol
  layer, but the handler remains a pressure point to keep thin.

Intentional complecting must be documented, not hidden. New
orchestrators are not added by accreting responsibilities — they are
named in this ADR with explicit responsibility limits.

## Hard Criteria

These are measurable and can be flagged in review.

### HC-1: Function Complexity

Functions with more than **5 parameters** warrant scrutiny. Consider:
- Should parameters be bundled into a config struct?
- Is this function doing too much?
- Are some parameters actually configuration from AppState?

**Exception:** `tokio-rusqlite` closures often need many `rusqlite::params![]` — these are data threading, not complexity.

### HC-2: Import Boundaries

Enforce architectural layering. The pairings below match the concerns
table in §Independence — each forbidden direction prevents the
"should NOT know about" relationship from leaking into actual code.

```text
src/tasks/        → must NOT import from src/sync_bridge.rs, src/tc_sync/, src/devices/
src/views/        → must NOT import from src/tasks/, src/tc_sync/
src/tc_sync/      → must NOT import from src/tasks/, src/views/
src/store/        → must NOT import from any handler module
src/auth/         → must NOT import from handler modules
src/devices/      → must NOT import from src/tc_sync/, src/sync_bridge.rs
```

`src/tasks/` MAY import from `src/views/`. The `GET /api/tasks?view=<id>`
read path consumes view definitions via the published `views::resolve_view`
entry point — this matches §Independence (Task CRUD's must-not list does
not include Views/Contexts; the asymmetric pairing is enforced in the
opposite direction by Views must NOT know about Tasks). Reaches into
`views/` submodules (e.g. `crate::views::defaults::*`) from outside
`src/views/` are still violations — call only the published top-level
surface.

**Tooling:** `scripts/check-hc2-boundaries.sh` (also `just hc2`) detects
both `use crate::X::` and fully-qualified `crate::X::*` paths; the
fully-qualified form was missed by the original use-only sweep until the
2026-05-04 review (see server#128).

**Allowed shared imports** (not counted toward coupling):
- `src/app_state.rs` — shared state container
- `src/store/mod.rs` — ConfigStore trait
- `src/store/models.rs` — record types
- `src/audit.rs` — audit logging helpers
- `src/auth/` — AuthUser extractor

### HC-3: Change Locality

Two separate budgets — pick the one that matches the change shape.

**(a) New CRUD resource** (like devices, geofences) should require changes to
**5 or fewer core files** plus tests and migration:
1. `store/mod.rs` — trait methods
2. `store/models.rs` — record type
3. `store/sqlite.rs` — queries
4. New handler module (`src/{resource}/`)
5. `main.rs` — route + OpenAPI registration

**(b) Cross-cutting feature** (header-on-existing-endpoints + storage backing,
e.g. `Idempotency-Key` per `cmdock/architecture` `task-write-contract.md`
§ Idempotency) should require changes to **8 or fewer load-bearing core
files** plus tests, migrations, CHANGELOG, and the cross-cutting
checklist items (CLAUDE.md, audit-reference, OpenAPI, metrics, config
example, CORS allowlist):

1. New module under `src/<feature>.rs` — the orchestration surface.
2. New submodule under `src/store/sqlite/<feature>.rs` — storage primitive.
3. `store/mod.rs` + `store/models.rs` — trait + types.
4. `store/sqlite.rs` — trait dispatch.
5. Handler integration (existing endpoint module).
6. `app_state.rs` (start hook), `lib.rs` (mod decl), `main.rs` (OpenAPI + CORS).
7. `metrics.rs` if the feature emits new metrics.
8. Service layer reshape if the feature requires new failure-classification
   types (e.g. the `CommitPhase` enum required by `task-write-contract.md`
   § Failure handling).

**Mechanical config-literal sprawl (e.g. test fixtures, connect-config
helpers, sync_bridge test setup) does NOT count toward the budget** when
all that's added is a `<section>: <Section>::default()` field. This is a
Rust struct-literal exhaustiveness tax, not entanglement.

The budgets are deliberately distinct: a new CRUD resource that touches
8 files signals over-engineering; a cross-cutting feature that touches
5 files signals shortcuts that may be entangling concerns.

### HC-4: Module Fan-Out

Any single **file** importing from more than **5 other internal crates/
modules** (excluding the allowed-shared list in HC-2) is becoming a
coupling hub.

**Measurement is per-file, not per-directory.** A directory like `admin/`
that aggregates many small files each with 3-4 imports is not a
violation — the ADR is about cohesive single-responsibility modules,
not directory-level distribution. Tools that report per-directory
import unions inflate the count and miss the actual coupling shape.

**Exceptions:** `main.rs` and `app_state.rs` are designated orchestrators.

## Review Prompts

### RP-1: The Independence Test

> "If I delete this module entirely, how many other files break?"

More than 2-3 direct dependents suggests a coupling hub.

### RP-2: The Knowledge Test

> "What does this function need to know to do its job? Is all of that knowledge actually necessary?"

If `authenticate_sync_client` needs to know about encryption, device registration, replica records, AND user records — it knows too much.

### RP-3: The Reasoning Test

> "Can I explain what this function does without referring to implementation details of its dependencies?"

### RP-4: The Change Impact Test

> "If I change the internal implementation of X, what else might break?"

### RP-5: The Boundary Test

> "Does this dependency introduce a new concern boundary, or entangle existing ones?"

Applied to the device registry design: adding per-device encryption to the device registry entangles device identity, crypto key management, sync storage, and the sync bridge into one intertwined system. These should be independent concerns — even if connecting them requires more upfront work.

## Application to Architecture Decisions

When evaluating design options, apply these prompts:

1. **Count the concerns.** How many independent things does this option entangle?
2. **Count the files.** How many modules need to change? (HC-3)
3. **Test independence.** Can each concern be understood, tested, and changed independently?
4. **Prefer more modules over fewer entangled ones.** Five simple modules is better than two complex ones.
5. **Accept upfront cost.** The simple solution often takes longer to build. That's the point.

### Example: Device Registry Crypto

| Option | Concerns Entangled | Files Changed | Independence |
|--------|-------------------|---------------|--------------|
| A: Auth-only (shared crypto) | 2 (auth + devices) | 3-4 | High |
| B: Server re-encryption | 5 (auth + devices + crypto + sync storage + bridge) | 8+ | Low |
| D: Per-device auth token | 2 (auth + devices) | 3-4 | High |

Options A and D are simpler. Option B solves a real problem (no secret rotation on device loss) but at high complexity cost. The ADR-0002 guidance: **start with the simple option, document the limitation, and only add complexity when users actually hit the problem.**

## Consequences

### Positive

- **Explicit criteria** for design discussions (not just "this feels complex")
- **Reviewable** — can point to specific violations
- **Incremental** — apply to new code without rewriting everything
- **Guards against premature optimisation** — build the simple thing first

### Negative

- **Overhead** — requires thought during review
- **May conflict with ease** — sometimes the simple solution takes longer
- **Judgment required** — review prompts aren't pass/fail

### Neutral

- Existing code may violate these criteria; that's expected debt
- Criteria may evolve as we learn what matters in this codebase
- Orchestrator modules are allowed exceptions by design

## References

- Rich Hickey, "Simple Made Easy" (Strange Loop, 2011)
- deckengine ADR-0011 (original, with Python-specific hard criteria and automated checks)
- ADR-0001: Sync Bridge Architecture (applies independence to REST ↔ TC boundary)
