---
created: 2026-04-02
status: accepted
tags: [architecture, admin, control-plane, operator, recovery]
---

# ADR-0006: Operator Surface Scope

## Status

**Accepted**

## Relationship To Architecture ADRs

This ADR is a repo-local application of:

- `cmdock/architecture ADR-0005: Open-Core And Managed-Service Boundary`

It defines which operator capabilities should remain self-hosted/local versus
which may be exposed through remote operator surfaces without weakening the
open-core server boundary.

## Context

`cmdock-server` now has cleaner internal boundaries for:

- operator HTTP auth
- device provisioning
- recovery/runtime coordination
- canonical sync change signalling

The next question is not "can everything be made remote?" but "which operator
actions belong on which surface?"

That matters because different admin operations have very different risk and
dependency profiles:

- some act on the running process only
- some act on both the running process and on-disk state
- some are break-glass filesystem operations
- some are strong candidates for a future remote operator UI

Without an explicit policy, remote admin work will drift by convenience rather
than by design.

## Decision

The operator surface is split intentionally into three categories.

### 1. Local CLI Only

These operations remain local CLI responsibilities by default:

- full backup
- full restore
- selective restore from backup
- destructive on-disk cleanup work
- break-glass/local-only repair procedures

Reason:

- they operate directly on the data directory
- they are tightly coupled to filesystem state
- they are difficult to make safe over remote HTTP in the open-core model

### 2. Operator HTTP / Future Remote UI Candidates

These operations are valid candidates for a remote operator control plane:

- user recovery assessment
- user offline / online transitions
- runtime eviction and diagnostics
- checkpoint operations
- device lifecycle management
- canonical sync identity lifecycle

Reason:

- they are operator workflows that can be expressed as service calls rather
  than raw filesystem mutation
- they are natural future remote-operator actions

### 3. Internal Operator Services

Before expanding remote admin endpoints, reusable operator behaviour should be
promoted into service boundaries that are independent of both CLI and HTTP.

The first explicit operator-service domains are:

- recovery
- canonical sync identity management

This keeps future admin HTTP/PWA work from duplicating sensitive operator
logic.

## Consequences

### Positive

- The remote admin boundary stays narrower and safer.
- Break-glass local repair remains a first-class self-hosting capability.
- Future remote operator UI work has clearer service boundaries to build on.
- Operator-surface growth is guided by policy rather than convenience.

### Negative

- Some operator actions will intentionally remain unavailable over HTTP.
- The CLI remains an important long-term surface rather than just a temporary
  bootstrap path.

### Neutral

- This ADR does not define hosted-service RBAC or tenant/team-admin semantics.
- This ADR does not require immediate expansion of the `/admin/*` HTTP API.

## Surface Matrix

### CLI-first and expected to stay that way

- backup
- restore
- selective restore
- break-glass local cleanup

### Shared service now, remote candidate later

- recovery assessment
- offline / online transitions
- canonical sync identity lifecycle
- device lifecycle

### Already live as operator HTTP

- status
- stats
- evict
- checkpoint
- offline / online

## Non-Goals

This ADR does not define:

- the exact future admin HTTP endpoint set
- richer remote operator auth/RBAC
- invite or onboarding UX
- external product-layer scope
