# Developer Guide

This guide explains how to extend `cmdock-server` without regressing the core
architecture.

It is the practical companion to:

- [ADR-0002: Design Simplicity Principles](../adr/ADR-0002-design-simplicity-principles.md)
- [ADR-0004: Control-Plane Boundary for Device Onboarding](../adr/ADR-0004-control-plane-boundary-for-device-onboarding.md)
- [ADR-0005: Operator HTTP Auth Boundary](../adr/ADR-0005-operator-http-auth-boundary.md)
- [ADR-0006: Operator Surface Scope](../adr/ADR-0006-operator-surface-scope.md)
- [Issue Triage Labels Reference](../reference/issue-triage-labels-reference.md)
- [Testing Strategy Reference](../reference/testing-strategy-reference.md)

Use this guide and the ADRs together when reviewing pull requests.

## CI Note

Repository automation should be expressed in repo-local entrypoints first:

- `just` recipes for common build, test, lint, and verification flows
- checked-in scripts for more structured orchestration
- lightweight GitHub workflows that call those entrypoints where public
  provider automation is needed

So when changing build, dependency, image, or qualification behavior, update
the repo-local entrypoint first and keep any provider-specific workflow wiring
thin.

## 1. Purpose

The server now has enough moving parts that "just add the code where it fits"
is no longer safe.

The main architectural risks are:

- coupling REST handlers directly to sync/runtime mechanics
- letting `AppState` become a business-logic hub
- duplicating business rules across HTTP, CLI, and background paths
- leaking operator concerns into end-user surfaces
- letting one new feature scatter changes across unrelated modules

This guide exists to keep those failure modes visible during design and review.

## 2. How To Use This Guide

Use this guide in three places:

1. before starting a feature
2. while choosing file/module boundaries
3. during pull-request review

The standard review question is:

> Does this change preserve independence, change locality, and the current
> surface boundaries?

If the answer is unclear, stop and resolve that before merging.

## 3. Core Architectural Boundaries

The main server boundaries are:

- **User REST surface**
  - task CRUD
  - app config
  - self-service device actions
  - ordinary bearer-token auth
- **TaskChampion sync surface**
  - protocol handlers
  - per-device sync auth
  - shared per-user `sync.sqlite`
- **Operator HTTP surface**
  - running-process diagnostics and control
  - separate operator auth
- **Local admin CLI**
  - local/offline operator workflows
  - backup/restore
  - bootstrap and recovery operations

The rule is simple:

- do not collapse these surfaces for convenience

Examples:

- a user REST token must not become operator auth
- task CRUD must not own bridge scheduling policy
- backup/restore must not become an ordinary end-user API concern

## 4. Preferred Internal Shape

The preferred internal pattern is:

- handlers and CLI commands parse input, call services, render output
- services own business rules and orchestration
- storage modules own persistence
- explicit coordinators own runtime cross-cutting mechanics
- `AppState` stays a state container and integration seam

Current examples in the repo:

- device provisioning lives behind shared services instead of separate CLI and
  HTTP copies
- device rename/revoke/unrevoke/delete now also live behind the shared device
  service instead of separate self-service, operator, and CLI copies
- operator HTTP endpoints are split by concern instead of one admin handler
  hub:
  - `src/admin/handlers.rs` for shared helpers plus server status
  - `src/admin/users.rs`, `src/admin/user_diagnostics.rs`, and
    `src/admin/user_lifecycle.rs` for user-scoped operator flows
  - `src/admin/runtime_ops.rs` for runtime cache/quarantine/checkpoint actions
- operator recovery work has explicit recovery coordination instead of being
  embedded directly in `AppState`
- canonical-change signalling is narrower than the old direct
  `tasks -> sync_bridge` dependency
- bearer auth and TaskChampion sync auth now share one runtime-access
  enforcement helper instead of duplicating runtime-policy gating logic
- webhook delivery stays behind one intentional orchestrator entrypoint, but
  target selection and delivery-state mutation are now internal helpers instead
  of one large undifferentiated module
- staging verification keeps shell as a compatibility wrapper and moves
  JSON-heavy scenario logic into Python runners such as
  `staging_admin_runtime.py`, `staging_product_runtime.py`,
  `staging_backup_restore.py`, and `staging_webhooks_runtime.py`
- internal release qualification follows the same pattern:
  - `load-test.sh` remains the thin profile-driven load wrapper
  - `release_qualification.py` owns budget evaluation
  - `release_endurance.py` owns the phased restart/resume endurance path rather
    than growing that logic into shell or the generic qualification evaluator

For HTTP boundary validation, the current preferred convention is:

- use typed Axum extractors first
- use `garde` for lightweight DTO validation where that keeps request rules
  local to the DTO
- keep the public wire error model simple unless an endpoint already defines a
  richer contract

See [HTTP DTO Validation With `garde`](../implementation/http-dto-validation-with-garde-note.md).

For secure boundary handling, the current preferred convention is:

1. SQL boundaries:
   - keep caller data out of SQL text
   - pass user values through bind parameters rather than interpolation
   - when SQL shape must vary, use typed enums or fixed allowlisted helpers
   - avoid internal dynamic SQL unless the static alternative is materially worse
2. Forwarded-header trust:
   - treat forwarded headers as a deployment-trust boundary
   - parse forwarded headers defensively and ignore malformed values
   - do not treat arbitrary header text as trusted client identity
3. HTTP boundary validation:
   - use typed extractors first
   - keep lightweight DTO validation close to the boundary
   - keep external error contracts simple unless the endpoint already defines richer ones
4. Parser hardening:
   - for parser-heavy or protocol-boundary code, consider a small dedicated
     `cargo-fuzz` target as a hardening layer separate from normal tests

For fuzzing, the current preferred convention is:

- use `cargo-fuzz` selectively for parser-heavy or protocol-boundary code
- keep fuzz targets out of normal `cargo test` runs
- run fuzz targets with nightly Rust
- check in a small seed corpus when that makes the target easier to exercise
- replay and minimize crashing artifacts rather than treating fuzz failures as
  one-off local events

## 5. Enforcement Heuristics

These heuristics exist to make the review rules more enforceable.

They are not mathematical laws, but they should make reviews less subjective.

### Heuristic 1: What "Thin" Usually Means

A handler or CLI command module is usually still thin if it mostly:

- parses input
- checks auth/permissions
- performs lightweight request validation
- calls one service/coordinator boundary
- maps the result to a response or terminal output

It is usually no longer thin if it starts combining multiple concern-heavy
steps itself, for example:

- store lookup + crypto workflow + runtime coordination
- store mutation + bridge/scheduler policy + response shaping
- duplicated lifecycle transitions already present in another surface

The exact line count is less important than concern count.

### Heuristic 2: What A Good Shared Service Boundary Looks Like

A shared service boundary is usually a good extraction when it:

- owns one coherent concern
- can be called from more than one surface
- accepts typed inputs and returns typed results
- does not depend on Axum, Clap, or other surface-specific types
- hides the multi-step business rule the surface should not repeat

A poor service extraction usually:

- becomes a grab-bag for unrelated workflows
- still requires the caller to orchestrate half the business rule itself
- leaks route/CLI/framework types into the service contract

### Heuristic 3: What Counts As Acceptable Cross-Cutting Change

Some changes are expected to touch multiple layers.

Usually acceptable:

- handler + OpenAPI + tests for an endpoint change
- store + migration + tests for a persistence change
- service + audit/metrics + docs where the behaviour changed

Usually suspicious:

- a new CRUD resource forcing edits in unrelated auth/sync/runtime modules
- a user-facing feature requiring operator-surface changes without a clear
  ownership reason
- a feature that only makes sense after adding new `AppState` business logic

The question is not "did several files change?" The question is whether those
files changed for one coherent reason.

### Heuristic 4: What `AppState` Should And Should Not Do

`AppState` is a good place for:

- shared handles
- caches
- config
- managers/coordinators
- thin delegating seams

`AppState` is a bad place for:

- domain lifecycle transitions
- recovery policy
- bridge/scheduling policy
- feature-specific orchestration

If a new method needs to make business decisions, it probably belongs somewhere
else.

### Heuristic 5: Observability Should Follow Ownership

When adding audit, metrics, or logs:

- put domain-level events near the service/coordinator that owns the behaviour
- put request/response or CLI surface events at the boundary layer
- avoid duplicating the same fact at both layers unless the duplication is
  intentional and useful

If adding observability requires every helper in a flow to know about metrics
or audit, the observability boundary is probably wrong.

### Heuristic 6: Secure Boundary Rules Should Be Explicit

Secure coding expectations in this repo are intentionally narrow and concrete.

For SQL and query construction:

- keep user data in bind parameters
- do not build SQL by interpolating caller-controlled values
- if query shape must vary, prefer typed enums or static allowlisted helpers
- treat free-form dynamic SQL as a review failure unless the SQL text is
  entirely code-owned and the static alternative is clearly worse

For trusted headers:

- treat forwarded headers as meaningful only behind trusted ingress
- parse them as structured values, not opaque strings
- ignore malformed forwarded-header values rather than logging or reflecting
  them as client identity

### Heuristic 7: OpenAPI Should Be Easy To Export Locally

The generated OpenAPI document is part of the server contract, especially for
downstream typed clients such as the control plane.

Current convention:

- keep `operation_id` values stable
- keep schemas concrete enough for generated clients
- make the spec exportable without booting the server

Preferred workflow:

- `just openapi-export`
- or `cargo run --bin cmdock-server -- openapi --output <path>`

Use that local export path when another repo needs to generate a typed client.
Do not require a live environment just to fetch the current server contract.

For request boundaries:

- parse typed inputs first
- apply lightweight boundary validation close to the DTO
- keep external error contracts simple unless the endpoint already defines a
  richer one

## 6. Extension Rules

### Rule 1: Choose The Surface First

Before writing code, decide which surface the feature belongs to:

- user REST
- TaskChampion sync
- operator HTTP
- local admin CLI
- background runtime coordinator

Do not start by editing whichever file looks closest.

If a feature touches more than one surface, define the shared service boundary
first.

### Rule 2: Keep Handlers Thin

Handlers should mostly do four things:

1. parse typed input
2. authenticate/authorize
3. call one service or coordinator
4. render the response

Handlers should not accumulate:

- storage orchestration
- crypto workflows
- bridge policy
- multi-step business rules duplicated elsewhere

The same rule applies to CLI command modules.

### Rule 3: Keep Business Rules In One Place

If CLI and HTTP both need the same rule, extract it.

Typical extraction triggers:

- device lifecycle logic needed by both admin CLI and API
- recovery transitions needed by both CLI and operator HTTP
- canonical sync identity rules needed by more than one surface
- auth/runtime-access rejection rules needed by more than one auth surface

Duplicating the same lifecycle logic in two surfaces is a regression even if
the code is short.

### Rule 4: Protect Change Locality

Adding a new typed CRUD concern should stay localized.

A good pattern is:

1. add store trait methods
2. add model types
3. implement storage
4. add a dedicated handler module
5. register routes/OpenAPI
6. add migration and tests

If the change also requires unrelated edits in tasks, sync, auth, admin CLI,
and runtime coordinators, the boundary is probably wrong.

### Rule 5: Parse At Boundaries

Parsing belongs at system boundaries:

- Axum extractors
- config loading
- SQLite row mapping
- protocol header parsing

Internal functions should work with typed values, not raw strings or unshaped
JSON where avoidable.

### Rule 6: Keep `AppState` Thin

`AppState` is allowed to hold:

- shared config
- store handle
- caches
- manager/coordinator references

`AppState` should not become the place where new business rules go.

If you are adding:

- lifecycle transitions
- recovery rules
- bridge policy
- multi-step orchestration

put that behind a service or coordinator instead of adding more methods to
`AppState`.

### Rule 7: Respect The Operator Boundary

Operator concerns stay separate from end-user concerns.

Examples:

- `/admin/*` uses operator auth, not user bearer auth
- destructive or filesystem-oriented workflows stay CLI-first unless there is a
  strong reason otherwise
- user-facing APIs should not quietly acquire operator semantics

When in doubt, default to:

- local CLI for filesystem-heavy repair
- operator HTTP for running-process control
- user REST for user-scoped application behaviour

### Rule 8: Keep The Open-Core Boundary Clean

The open-core repo should document and implement server primitives, not
product-specific orchestration.

Good open-core scope:

- device records
- sync identity
- revoke semantics
- summary preferences owned by the server
- outbound hooks or narrow extension points

Bad open-core scope:

- product-specific onboarding UX
- provider-specific notification delivery logic
- external entitlement or account-capability orchestration

If a feature depends on a future external layer, keep only the server-side
boundary and primitives here.

### Rule 9: Be Deliberate About Observability

Audit and metrics are important, but they can become new coupling hubs.

When adding observability, decide explicitly:

- is this a domain event owned by a service?
- is this a surface event owned by a handler/CLI path?
- is this runtime health owned by a coordinator?

Do not let every extracted service grow ad hoc metrics and audit calls without
review.

### Rule 10: Use The Right Tool For Test And Ops Complexity

Keep the implementation language matched to the shape of the work:

- Rust for product behavior, protocol logic, integration tests, and typed
  harnesses
- shell for thin entrypoints, environment setup, and simple command
  composition
- Python for staging/load scenarios that are JSON-heavy, stateful, or need
  reusable orchestration helpers

Current examples:

- `scripts/staging-test.sh` stays a stable operator-facing entrypoint
- `scripts/staging_verify.py` owns staging preflight and runner selection
- Python helper runners own structured scenario bodies
- shell should not be the long-term home for growing assertion logic once it
  starts to carry significant state or data shaping

## 7. Common Regression Patterns

These are warning signs in code review:

- a handler imports `sync_bridge` or other deep runtime machinery directly
- a new feature adds methods to `AppState` instead of adding a service
- the same workflow appears once in CLI code and again in HTTP code
- a new resource edit forces unrelated changes across many subsystems
- a user-facing token or route quietly gains operator powers
- an implementation note in the public technical-note set starts driving runtime
  coupling instead of documenting it
- a shell test/deploy script starts embedding large JSON parsing or
  multi-scenario orchestration that would be clearer as a structured helper

## 8. PR Review Checklist

Use this checklist with ADR-0002 during review.

- Which surface owns this change: user REST, sync, operator HTTP, CLI, or a
  runtime coordinator?
- Is the surface choice explicit in the implementation?
- Are handlers/CLI modules thin, or did business rules leak into them?
- Is shared logic extracted once instead of duplicated?
- Did `AppState` stay a container rather than becoming a rules engine?
- Did the change preserve auth and operator boundaries?
- Did the change stay localized, or did it sprawl across unrelated modules?
- Are audit, metrics, OpenAPI, docs, and tests updated in the right layer?
- If this introduces a future-facing feature, is the open-core boundary still
  clean?

If several answers are uncomfortable, the change probably needs a boundary
adjustment before merge.

## 9. When To Write Or Amend An ADR

Write or update an ADR when a change:

- creates a new long-lived boundary
- changes which surface owns an operation
- changes auth or security responsibilities
- changes the storage/runtime mental model
- introduces a new intentional orchestration point

Do not bury architectural decisions only in code or PR comments.

## 10. Minimum Change Set For Meaningful Features

For non-trivial features, expect to review all of these:

- code boundary
- tests
- docs
- OpenAPI if HTTP is involved
- audit/metrics impact
- operator impact

The cross-cutting checklist in this guide, the ADR set, and the coding-style
reference is part of the implementation standard, not optional cleanup.

## 11. Suggested Reading Order

For contributors working on architecture-sensitive changes:

1. [Concepts Guide](concepts-guide.md)
2. [Developer Guide](developer-guide.md)
3. [ADR-0002: Design Simplicity Principles](../adr/ADR-0002-design-simplicity-principles.md)
4. [Admin Surfaces Reference](../reference/admin-surfaces-reference.md)
5. [TaskChampion Integration Reference](../reference/taskchampion-integration-reference.md)
6. [Testing Strategy Reference](../reference/testing-strategy-reference.md)

## Summary

The standard for changes in this repo is not just "works" or "passes tests".

The standard is:

- correct behaviour
- preserved boundaries
- localized change
- explicit ownership
- no convenience coupling that future work will have to unwind
