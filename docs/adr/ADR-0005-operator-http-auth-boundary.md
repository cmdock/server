---
created: 2026-04-02
status: accepted
tags: [architecture, auth, admin, control-plane, security]
---

# ADR-0005: Operator HTTP Auth Boundary

## Status

**Accepted**

## Relationship To Architecture ADRs

This ADR is a repo-local application of:

- `cmdock/architecture ADR-0001: Simplicity As A Cross-Repo Principle`
- `cmdock/architecture ADR-0005: Open-Core And Managed-Service Boundary`

It defines how the shared simplicity and open-core boundary principles apply to
the server's operator HTTP surface.

## Context

`cmdock-server` currently exposes two distinct control surfaces:

- the local admin CLI
- admin HTTP endpoints under `/admin/*`

The local admin CLI is intentionally a self-hosted/operator surface. It can
mutate on-disk state, perform restore/recovery work, and coordinate with a
running server via the offline marker.

The admin HTTP surface is also an operator surface. It can:

- inspect per-user recovery state
- evict runtime caches
- force checkpoints
- take users offline and bring them back online

At the moment, these `/admin/*` endpoints are protected only by ordinary bearer
token user auth.

That is wrong for two reasons:

1. **Security:** A normal end-user token should not imply operator/process
   control capability.
2. **Simplicity (ADR-0002):** User REST auth and operator control-plane auth are
   separate concerns. Braiding them together makes both boundaries harder to
   reason about and encourages more drift in future remote operator UI work.

ADR-0002 argues to prefer independent concerns over familiar shortcuts. Adding
"admin-ness" to normal user tokens would be the easy path, not the simple one.

## Decision

The running-server admin HTTP surface will use a **separate operator auth
mechanism** from the normal user REST API.

### Separation Rule

There are now three distinct auth/control contexts:

- **User REST auth**
  - bearer token
  - user-scoped CRUD and self-service device actions
- **Operator HTTP auth**
  - separate operator credential/validator
  - running-server diagnostics and process control
- **Local admin CLI**
  - offline/local operator control without HTTP auth

These concerns must not be collapsed into one token or extractor by default.

### Preferred Mechanism

Prefer a deliberately separate operator HTTP credential rather than adding
roles/scopes onto ordinary user tokens.

For the open-core self-hosted server, the simplest acceptable initial mechanism
is:

- one configured operator bearer token (or equivalent static operator secret)

This is sufficient for current self-hosted and staging use.

If a richer remote control plane is added later, that can evolve independently
without requiring the user bearer-auth model to change first.

### Handler Boundary

Admin HTTP handlers should no longer depend on the normal `AuthUser` extractor.

They should depend on a separate operator auth extractor, for example:

- `OperatorAuth`
- `AdminAuth`

That extractor is the explicit architectural marker that the request is part of
the operator control plane rather than the user API.

### OpenAPI Boundary

The public API documentation must make the split visible:

- user REST auth scheme
- operator/admin auth scheme or explicit operator-auth documentation

### Audit Boundary

Operator HTTP actions must continue to emit audit events with:

- `source = "api"`
- operator identity or operator-auth source information when available

This keeps operator actions distinct from both:

- CLI admin actions (`source = "cli"`)
- normal end-user REST calls

## Consequences

### Positive

- Admin HTTP becomes a true operator surface.
- User REST auth remains simpler and narrower.
- Future remote operator UI work has a cleaner boundary.
- The architecture better matches ADR-0002's independence rule.

### Negative

- Operators must manage a separate HTTP admin credential.
- OpenAPI/docs/testing need an extra auth concept.
- There are now intentionally multiple auth paths in the server.

### Neutral

- The local admin CLI remains the primary break-glass/offline surface.
- This ADR does not define a future RBAC/team-admin model.

## Non-Goals

This ADR does not define:

- hosted-service RBAC
- team-admin semantics
- invite/control-plane UX
- replacement of the local admin CLI

Those belong to later control-plane design work.
