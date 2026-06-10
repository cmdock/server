# Invite Code Onboarding Reference

This document describes the **open-core boundary** for future invite-style
onboarding.

It is a **reference note**, not a statement of current behaviour.

Today, the supported self-hosted path is still manual provisioning via the
admin CLI. See [Installation and Setup Guide](../manuals/installation-and-setup-guide.md)
and [Administration Guide](../manuals/administration-guide.md).

Related:

- [ADR-0004: Control-Plane Boundary for Device Onboarding](../adr/ADR-0004-control-plane-boundary-for-device-onboarding.md)
- [Admin Surfaces Reference](admin-surfaces-reference.md)
- [TaskChampion Integration Reference](taskchampion-integration-reference.md)

## Purpose

The goal is to make future invite-style onboarding possible without changing
the core security model.

For the open-core server, the important architectural point is:

- invite UX is an external orchestration layer
- the core server continues to own the real long-lived credentials and device
  primitives

## What Must Stay True

Any invite-based design should preserve these existing truths:

- one canonical sync identity per user
- one device record per physical client
- per-device revoke remains possible
- manual provisioning remains supported
- the invite itself is not the long-lived credential

The invite flow should be a different way to obtain the same underlying
credentials, not a parallel credential model.

## Current Manual Baseline

Today, a self-hosted operator does this manually:

1. create the user
2. create the canonical sync identity once for that user
3. create a device record for each physical client
4. paste the emitted values into the client

For the current REST-first iOS client, the important values are the server URL
and bearer token.

For TaskChampion-capable clients, the important values are the server URL,
`client_id`, and the per-device secret.

Any future invite flow is a convenience wrapper around this same primitive set.

## Core Server Responsibilities

The open-core server should continue to own:

- user creation and lookup
- device record creation and lifecycle
- canonical sync identity creation
- minting the real long-lived credentials
- revoke enforcement

If an external onboarding layer exists later, it should call narrow server APIs
for those primitives rather than introducing a second credential model.

## External Onboarding Layer

An external orchestration layer may later provide:

- invite creation and delivery
- QR / deep-link / short-code UX
- onboarding sessions
- pairing and device-name collection

Those are product-layer concerns, not sync-runtime concerns.

This note does not define that external system's storage model or product UX.

## Minimal Contract the Core Should Support

If invite-style onboarding is added later, the open-core server should expose
or preserve enough capability to let an external layer:

1. create or identify the target user
2. create the canonical sync identity if needed
3. create a real device record at redemption time
4. obtain the final credentials needed by the client
5. revoke the device later if onboarding fails or the device is lost

The safest default remains:

- do not create a real device record at invite issuance time
- create it when redemption is actually happening

## Non-Goals

This note does **not** define:

- QR payload formats
- short-code formats
- external invite/session tables
- richer onboarding UX
- mobile app screen flows

Those belong outside the open-core repo.

## Summary

For the open-core server, the boundary is simple:

- manual provisioning remains complete and supported
- the server owns the real credential and device primitives
- any future invite flow is an external wrapper around those primitives
- no invite subsystem is required in the core runtime
