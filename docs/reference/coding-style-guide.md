# Coding Style Guide

This is a repo-specific coding conventions guide for `cmdock-server`.

It complements:

- [Developer Guide](../manuals/developer-guide.md)
- [ADR-0002: Design Simplicity Principles](../adr/ADR-0002-design-simplicity-principles.md)

This document is not a replacement for `rustfmt`, `clippy`, or normal Rust
idioms. It only captures the local conventions that matter for consistency and
review.

The architectural ownership rules stay canonical in
[Developer Guide](../manuals/developer-guide.md). This document focuses on how
those rules should show up in day-to-day code.

## 1. Purpose

The main style goal in this repo is:

- code should make architectural boundaries obvious

Good style here is not only about formatting. It is also about:

- making ownership clear
- keeping surface code thin
- preferring typed domain structures over loose blobs
- avoiding convenience abstractions that blur concerns

## 2. Working Within Module Roles

The canonical role split lives in the developer guide.

At the code level, the important convention is:

- **handlers**
  - should read like boundary adapters
- **services**
  - should read like business workflows
- **store**
  - should read like persistence code
- **coordinators**
  - should read like runtime orchestration
- **models / DTOs**
  - should make contracts explicit

If a file reads like it belongs to a different role, stop and move the logic.

## 3. Naming Conventions

Prefer explicit names over clever ones.

- handler modules: `handlers.rs`
- service modules: name the concern directly, such as `recovery`, `sync_identity`
- coordinator modules: name the concern directly, such as `runtime_recovery`,
  `runtime_sync`
- coordinator types: `<Concern>Coordinator`
- store records: `<Thing>Record`
- request/response types: `<Verb><Thing>Request`, `<Thing>Response` where useful

Avoid vague names like:

- `utils`
- `helpers`
- `misc`
- `common`

If a helper is only used by one concern, keep it inside that concern.

## 4. Handler Style

Handlers should stay short and unsurprising.

Prefer this shape:

1. extract typed input
2. validate lightweight request rules
   - prefer `garde` on request DTOs where that keeps boundary validation local
   - keep wire-level error shapes simple unless the endpoint already defines a richer contract
3. call one service/coordinator/store seam
4. map result to response

Avoid handlers that:

- construct SQL
- perform multi-step crypto workflows
- coordinate bridge policy directly
- duplicate logic that already exists in the CLI or another handler

If a handler starts reading like an orchestration script, extract a service.

Security-sensitive review rules that stay explicit in this repo:

- user-controlled data belongs in bind parameters, not SQL text
- caller-controlled dynamic SQL is not acceptable
- forwarded headers only mean anything under trusted ingress
- lightweight DTO validation should happen at the HTTP boundary

## 5. Error Handling

Prefer typed/domain errors at boundaries that matter.

Use typed errors when:

- the caller must distinguish behaviour
- the error maps to HTTP/operator behaviour
- the failure is part of a domain workflow

Use `anyhow`-style aggregation only where:

- the boundary is operational/CLI-heavy
- the caller mainly needs context-rich failure reporting
- the error is not part of a stable API contract

Rules:

- add context to storage and filesystem failures
- do not silently collapse distinct failure modes into one generic string
- do not invent complex error taxonomies unless the caller benefits from them

## 6. Typing Rules

Prefer typed structs over `serde_json::Value`.

`serde_json::Value` is acceptable when:

- the shape is intentionally opaque
- the endpoint is a legacy compatibility shim
- the system is temporarily bridging from untyped to typed storage

Typed structs are preferred when:

- the server owns the contract
- the data has validation rules
- the type is used in more than one place
- OpenAPI should describe it clearly

The default should be:

- typed request/response models
- typed internal domain values

## 7. Comments

Comments should explain:

- why a boundary exists
- why a pattern is non-obvious
- why a workaround is necessary

Comments should not restate obvious code.

Prefer short comments above the relevant block rather than long inline chatter.

## 8. Logging, Audit, and Metrics

Use observability deliberately.

Prefer:

- audit for meaningful write/operator actions
- metrics for hot-path behaviour, failure modes, and runtime state
- tracing/logging for execution detail and diagnosis

Avoid:

- sprinkling metrics into every helper
- duplicating the same event at handler and service layers without reason
- adding audit events to read-only paths unless there is a strong operator need

When adding observability, follow the ownership rule from the developer guide,
then keep the implementation local and boring.

## 9. OpenAPI Conventions

If an HTTP endpoint changes, review the OpenAPI surface in the same change.

Expectations:

- every `#[utoipa::path]` should have a stable `operation_id`
- request/response types should be typed where practical
- endpoint docs should reflect actual auth and error behaviour

Do not leave schema drift for later cleanup.

## 10. Testing Conventions

Match the test layer to the behaviour:

- unit tests for pure logic and helpers
- integration tests for route/state/storage seams
- system tests for CLI/server/disk/restart/operator workflows
- load tests for contention and throughput shape

When changing architecture-sensitive code, prefer adding the test at the layer
where the regression would actually appear.

## 11. Store Layer Conventions

The store trait is a real boundary, not a convenience wrapper.

Rules:

- handlers should not reach around the trait into SQLite details
- new config/resource persistence should enter through `ConfigStore`
- record mapping belongs in store code, not handlers
- backend swap concerns should stay invisible to API surfaces

## 12. Code-Level Review Smells

These are code-level style failures in this repo even if the code compiles:

- a `handlers.rs` file that reads like a service
- fresh `serde_json::Value` usage where the server owns the shape
- new `utils.rs` style dumping grounds
- route or CLI framework types leaking into shared service contracts
- SQL assembly or row-shaping logic living in handlers
- response/OpenAPI changes that leave request/response models vague or untyped
- OpenAPI drift left behind after endpoint changes

## Summary

The local style standard is:

- explicit module roles
- typed boundaries
- thin surfaces
- shared business logic
- deliberate observability
- no convenience coupling hidden behind "helper" code
