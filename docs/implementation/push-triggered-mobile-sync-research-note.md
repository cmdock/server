# Push-Triggered Mobile Sync Research Note

Updated: 2026-04-02

Related:

- [Sync Bridge Reference](../reference/sync-bridge-reference.md)
- [TaskChampion Integration Reference](../reference/taskchampion-integration-reference.md)
- [Invite Code Onboarding Reference](../reference/invite-code-onboarding-reference.md)
- [API Interaction Flows Reference](../reference/api-interaction-flows-reference.md)
- [ADR-0004: Control-Plane Boundary for Device Onboarding](../adr/ADR-0004-control-plane-boundary-for-device-onboarding.md)

## Purpose

This note describes the **open-core boundary** for future push-triggered mobile
sync.

It exists to clarify what the core server would need to support if some later
notification layer wants to wake clients after user-visible changes.

This is a future implementation note, not a description of current behaviour.

## Core Principle

Push is a **wake-up signal**, not the sync transport.

The open-core server should not depend on notification-provider integrations for
correctness.

The intended model is:

1. the server commits state normally
2. canonical and bridge state advance as they do today
3. the server may emit a coarse outbound "device/user should be nudged" signal
4. some external notification layer may choose to wake the client
5. the client performs its normal authenticated sync

That keeps the runtime model intact:

- REST remains REST
- TaskChampion sync remains TaskChampion sync
- notification is only an optimisation for reducing client staleness

## Open-Core Responsibilities

If push-triggered sync is added later, the core server should own:

- deciding which user-visible state changes are notify-worthy
- preserving per-device revoke semantics
- tying any notification eligibility to real device records
- optionally storing coarse per-device notification reachability metadata or an
  opaque target reference
- emitting a narrow outbound hook or API call to an external notification layer

The core server should continue to treat delivery failure as non-fatal.

## External Notification Layer

Provider-specific delivery should remain outside the open-core runtime.

An external layer may later own:

- provider credentials
- provider request formatting
- token invalidation handling
- delivery retries/coalescing

This note does **not** define that external system's implementation.

## Why This Boundary Matters

The open-core server must remain self-hostable and complete without any
product-owned notification service.

So the safe rules are:

- no provider-specific integration is required for core correctness
- notification delivery must not fail writes
- self-hosters can ignore notifications entirely or wire the outbound hook into
  their own tooling if they choose

## Design Goals

- preserve the canonical + bridge architecture
- avoid putting notification fan-out on the request hot path
- keep device lifecycle and revoke semantics authoritative in the server
- allow a generic outbound notification hook later without baking product UX
  into the runtime
- tolerate dropped, delayed, or coalesced delivery

## Non-Goals

- replacing normal sync polling entirely
- guaranteeing immediate convergence
- sending task contents through provider payloads
- requiring a hosted notification layer for correctness

## Summary

For the open-core server, the boundary is straightforward:

- the server may decide that some device or user should be nudged
- the server may expose minimal metadata and an outbound hook to support that
- actual notification delivery stays outside the core runtime
