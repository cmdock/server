---
created: 2026-04-01
status: accepted
tags: [architecture, product-boundary, admin, devices, onboarding]
---

# ADR-0004: Control-Plane Boundary for Device Onboarding

## Status

**Accepted**

## Relationship To Architecture ADRs

This ADR is a repo-local application of:

- `cmdock/architecture ADR-0002: Implementation-Agnostic Boundaries`
- `cmdock/architecture ADR-0005: Open-Core And Managed-Service Boundary`

It defines how the shared open-core and orchestration-boundary principles apply
to device onboarding in `cmdock/server`.

## Context

cmdock-server has two architectural goals that pull in different directions:

1. **Self-hosters must have a fully functional server.**
   A user running the open-source server in a homelab or small team environment
   must be able to manage devices and onboard clients without depending on any
   external orchestration layer.

2. **Future remote orchestration should stay outside the core runtime.**
   Invite links, short codes, deep links, QR flows, and richer remote onboarding
   workflows are convenience-layer concerns rather than sync-runtime concerns.

If these concerns are collapsed into one implementation, either:

- the open core becomes artificially limited, or
- product-specific invite/orchestration logic leaks into the core server.

ADR-0002 argues against complecting security primitives, device lifecycle, and
product UX.

## Decision

We split responsibilities between the **open core server** and the
**external orchestration layer**.

### Open Core Server Responsibilities

The open-source core server owns the security-critical primitives:

- canonical sync identity (`replicas`)
- device registry (`devices`)
- device credential generation (`client_id`, per-device secret)
- sync authentication and revocation enforcement
- admin CLI for local device lifecycle
- manual/self-hosted provisioning

Self-hosted onboarding must remain fully functional without any external
orchestration service. The baseline supported path is:

1. operator creates a device with the admin CLI
2. server prints the server URL, `client_id`, and device secret
3. the client is configured manually

This path is less convenient than hosted onboarding, but it is complete and
supported.

### External Orchestration Responsibilities

Any future external orchestration layer may own onboarding convenience and
remote workflow orchestration:

- invite creation and delivery
- team-admin provisioning workflows
- links, short codes, QR flows, and deep links
- remote-device handoff UX
- higher-level user/team administration

These are product-layer concerns, not core sync/server concerns.

### No Invite Subsystem in Core (for now)

The core server will not implement a first-class invite/one-time enrollment
subsystem at this stage.

In particular:

- QR is **not** required in the core server
- invite links and short codes are **not** required in the core server
- self-hosted onboarding uses manual provisioning via the admin CLI

This preserves a clean separation: the core remains fully functional, while the
external layer remains an optional wrapper around existing server-side
primitives.

### Remote Admin API

The core server may expose a narrow remote admin/device-management API so that a
future external orchestration layer can administer a running core server without direct SQLite
access.

That API should stay focused on device lifecycle primitives:

- create/list/show/rename devices
- revoke/unrevoke/delete devices
- create/show/delete the canonical sync identity

If such an API exists, the admin CLI should prefer using it in normal operation,
while still retaining a local/offline mode for break-glass self-hosted use.

### Manual Provisioning Must Remain Supported

Client applications must be architecturally able to support a manual setup flow.
Any future invite-based onboarding is an additional UX path, not a replacement
for manual provisioning.

This keeps the architecture open to future hosted onboarding without forcing the
open core server to depend on those flows.

## Consequences

### Positive

- Self-hosters retain a complete, non-hosted path for onboarding and device
  management.
- Security-critical credential issuance remains in the core server.
- Remote onboarding UX can evolve independently without destabilising core sync
  logic.
- The product boundary is explicit and defensible.

### Negative

- Self-hosted onboarding is more manual than a richer remote orchestration
  experience.
- The system may temporarily have two admin paths: local CLI and remote admin
  API.
- Some convenience features will live outside the core server.

### Neutral

- The architecture remains open to a future enrollment-token design if that is
  later deemed necessary.
- QR onboarding can still be added later in the managed layer without changing
  the device registry model.
