# TaskChampion Integration Reference

This document explains how the server integrates with TaskChampion /
Taskwarrior-style sync clients.

For the broader mental model, see
[Concepts Guide](../manuals/concepts-guide.md).

## 1. Why This Integration Is Special

The server is not just a generic REST API with an extra sync endpoint.

It has to support two different worlds:

- server-side canonical task state used by REST and other first-party logic
- TaskChampion sync protocol state expected by external task clients

Those are related, but not identical.

## 2. What a TaskChampion Client Expects

A TaskChampion client expects:

- a per-client identity (`client_id`)
- a sync version chain
- snapshots
- opaque encrypted history segments

The client does not expect to talk directly to the server's canonical task DB.

## 3. Why The Runtime Uses One Shared Sync DB

Each physical device still gets its own `client_id` and derived secret because
revocation is device-facing.

What changed during implementation is the server-side storage shape.

The runtime does **not** keep one live sync DB per device. Instead it keeps one
shared `users/<user-id>/sync.sqlite` per user and translates device envelopes at
the HTTP boundary.

That lets the server:

- revoke one device without rotating everyone else
- keep one `client_id` per device
- preserve TaskChampion protocol semantics without the broken multi-chain relay model

## 4. Why the Canonical Replica Still Exists

The canonical replica exists because the server needs one authoritative task
state for:

- REST reads
- REST writes
- server-side task logic
- operational inspection

The canonical replica is not a substitute for the shared sync DB.

## 5. Opaque Payload Handling

TaskChampion history segments are opaque protocol payloads.

The server:

- validates the device envelope
- translates it into the canonical sync envelope
- stores it in the shared sync DB
- bridges between shared sync state and canonical state

The server is not simply storing plaintext tasks in the device sync DB.

## 6. Per-Device Crypto

Each device has:

- its own `client_id`
- its own derived encryption secret

That is what makes device-scoped revoke practical.

The server stores the device secret encrypted under the server master key.

## 7. Auth Model

TaskChampion sync does not use the same auth path as the REST API.

REST:

- bearer token auth

TaskChampion sync:

- `X-Client-Id`
- device lookup
- device status validation

The runtime authenticates sync requests against the device registry.

## 8. Bridge Role

The bridge exists because the canonical replica and the TaskChampion sync
surface must stay logically aligned without collapsing into one storage surface.

That is the core integration trick in this server.

The bridge is a designated orchestrator. The intended boundary is:

- task/domain code changes canonical state
- a narrower coordinator signals that canonical state changed
- bridge scheduling and reconcile policy stay on the bridge side of that boundary

Current implementation note:

- REST task handlers signal canonical writes through
  `RuntimeSyncCoordinator::note_canonical_change(...)`
- they no longer import `sync_bridge` directly

## 9. Why Per-Device Credentials Are Enough

What revoke actually needs is:

- one device record per physical client
- one `client_id` per device
- one derived secret per device

That already gives the correct security and lifecycle boundary.

The earlier idea that we also needed one server-side sync DB per device turned
out to be wrong for TaskChampion. A single canonical relay replica cannot
reliably propagate device-originated changes across many independent server-side
chains.

## 10. What the Server Guarantees

The goal is:

- working canonical task behaviour
- working device sync behaviour
- convergent reconciliation between the two

The goal is not:

- exact preservation of every historical transport detail through every
  recovery or operator action

## 11. Current Constraints

Important current realities:

- bridge pressure is higher for shared or multi-device users
- the shared sync DB is protocol-facing state, not just a cache file
- recovery may restore working sync behaviour without restoring byte-identical
  historical transport state

## 12. Future Directions

Likely future work includes:

- further narrowing of bridge fan-out
- more explicit sync-state rebuild and repair tooling
- push-triggered targeted sync as an optimisation for mobile devices
