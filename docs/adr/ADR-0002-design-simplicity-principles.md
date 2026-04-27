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

Some complecting is acceptable when the architecture requires it:

- **Sync bridge** (`sync_bridge.rs`): Intentionally coordinates REST replica, TC storage, encryption, and per-user locking. It's a designated orchestrator.
- **AppState**: Holds all shared state (store, config, caches, managers). Intentional integration point — but it should remain a thin container, not contain business logic.
- **`main.rs`**: Wires routes, middleware, OpenAPI. Designated orchestrator.

Intentional complecting must be documented, not hidden.

## Hard Criteria

These are measurable and can be flagged in review.

### HC-1: Function Complexity

Functions with more than **5 parameters** warrant scrutiny. Consider:
- Should parameters be bundled into a config struct?
- Is this function doing too much?
- Are some parameters actually configuration from AppState?

**Exception:** `tokio-rusqlite` closures often need many `rusqlite::params![]` — these are data threading, not complexity.

### HC-2: Import Boundaries

Enforce architectural layering:

```text
src/tasks/        → must NOT import from src/views/, src/sync_bridge.rs
src/views/        → must NOT import from src/tasks/, src/tc_sync/
src/tc_sync/      → must NOT import from src/tasks/, src/views/
src/store/        → must NOT import from any handler module
src/auth/         → must NOT import from handler modules
src/devices/      → must NOT import from src/tc_sync/, src/sync_bridge.rs
```

**Allowed shared imports** (not counted toward coupling):
- `src/app_state.rs` — shared state container
- `src/store/mod.rs` — ConfigStore trait
- `src/store/models.rs` — record types
- `src/audit.rs` — audit logging helpers
- `src/auth/` — AuthUser extractor

### HC-3: Change Locality

Adding a new CRUD resource (like devices, geofences) should require changes to **5 or fewer core files** plus tests and migration:
1. `store/mod.rs` — trait methods
2. `store/models.rs` — record type
3. `store/sqlite.rs` — queries
4. New handler module (`src/{resource}/`)
5. `main.rs` — route + OpenAPI registration

### HC-4: Module Fan-Out

Any single module importing from more than **5 other internal crates/modules** is becoming a coupling hub.

**Exceptions:** `main.rs`, `sync_bridge.rs`, and `app_state.rs` are designated orchestrators.

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
