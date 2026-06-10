# TaskChampion Integration Reference

This document explains how the server integrates with TaskChampion /
Taskwarrior-style sync clients.

For the broader mental model, see
[Concepts Guide](../manuals/concepts-guide.md).

## 1. Why This Integration Is Special

The server is not just a generic REST API with an extra sync endpoint. It has to
support two related but distinct worlds:

- server-side canonical task state used by REST and other first-party logic
- TaskChampion sync protocol state expected by external task clients

The TaskChampion protocol expects a version chain and snapshots. It does not
expect direct access to the server's canonical task DB.

## 2. Runtime Shape

`MergedSyncGateway` is the single runtime path for `/v1/client/*`.

```text
TaskChampion client
  -> TC sync HTTP adapter
  -> MergedSyncGateway
  -> canonical source replica(s)
  -> durable merged TaskChampion chain
```

For personal-only beta the visible source scope contains one account: the
user's personal canonical replica. Teams add source-account policy later; they
do not add a second sync architecture.

## 3. Storage Shape

Each physical device still gets its own `client_id` and derived secret because
revocation is device-facing. The server-side protocol chain is shared per user
and gateway-backed:

- canonical source truth: `users/<user-id>/taskchampion.sqlite3`
- merged projection cache: `users/<user-id>/merged/replica/taskchampion.sqlite3`
- gateway-served TC chain: `users/<user-id>/merged/sync.sqlite`

Older files such as `users/<user-id>/sync.sqlite` may exist after upgrades or in
legacy tests, but they are not served by `/v1/client/*` after the merged-gateway
cutover.

## 4. Payload Handling

TaskChampion history segments arrive encrypted per device. The HTTP adapter
handles request authentication and envelope translation, then passes plaintext
protocol payloads to the gateway boundary.

Raw TaskChampion history JSON is decoded only in
`merged_sync_gateway::codec`. The rest of the gateway works with typed decoded
operations, source-write plans, journal state, and projection/correction output.

## 5. Auth Model

TaskChampion sync does not use REST bearer tokens.

REST:

- bearer token auth

TaskChampion sync:

- `X-Client-Id`
- device lookup
- device status validation
- per-device encryption secret

Device lifecycle actions (`revoke`, `disable`, `rotate`, `delete`) apply to the
gateway identity immediately because every sync request resolves through the
device registry before reaching the gateway.

## 6. Source Truth And Projection

Canonical replicas remain authoritative for task state. The merged chain is a
durable protocol/projection surface, not source truth.

- TW writes are journaled, decoded, authorized/routed, applied to canonical
  source truth, then projected/corrected back into the merged chain.
- REST writes update canonical source truth and mark devices stale; the next TW
  read projects source changes into `merged/sync.sqlite` before serving child
  versions.
- `cmdock_task_scope` is the canonical Task Scope command/projection input
  and is validated by the gateway (TSKEY-007/SG-011). `cmdock_account` is no
  longer written by the server; inbound `cmdock_account` values from TW clients
  are filtered during merge and cleared via corrective projection so legacy TC
  UDA history converges forward without operator action.
- `cmdock_key` remains server-owned corrective state.

### Reserved-UDA drift correction is single-pass

When a client pushes a drifted reserved UDA (for example a forged
`cmdock_key` or `cmdock_task_scope` value), the gateway corrects it in **one
projection pass**. The inbound op lands in the merged sync storage and marks the
scope as requiring projection; the projection step syncs/pulls the merged
projection replica from storage *before* it mirrors canonical fields, so it
diffs against the client-pushed value (not its own already-canonical state),
detects the divergence, and emits a corrective op in the same pass. The client
converges on its next sync. Each emitted correction is audited as
`merged_sync.cmdock_task_scope_corrected`, `merged_sync.cmdock_key_corrected`,
or `merged_sync.cmdock_account_corrected` (the last clears legacy values).

This is a stronger guarantee than the legacy `sync_bridge.rs` model, whose
reverse-op correction depended on a *subsequent* canonical-changing event to
propagate and could remain pending indefinitely on a quiet user. REST
`TaskItem.key` is unaffected throughout — it always reads the allocation table.

## 7. Recovery Posture

The gateway journal records inbound-version progress so recovery can move
forward after crashes. Once an inbound version is accepted, recovery appends,
replays, corrects, or quarantines; it does not rewrite accepted client history.

Operator diagnostics expose journal/recovery status through admin user stats and
merged-gateway audit/metrics signals.

## 8. What The Server Guarantees

The goal is:

- working canonical task behaviour
- working device sync behaviour through one gateway runtime path
- convergent source truth and merged TaskChampion projection
- device-scoped credential lifecycle and immediate revoke/disable enforcement

The goal is not byte-identical preservation of pre-gateway sync-chain history.
Old personal sync-chain files are retained only as backup/diagnostic artifacts
unless an operator explicitly cleans them up.
