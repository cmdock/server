# Recovery Reference

This document explains the internal recovery model used by the server.

It is not a step-by-step operator guide. For procedures, use
[Backup and Recovery Guide](../manuals/backup-and-recovery-guide.md).

Use this document when reasoning about:

- quarantine vs operator offline state
- offline markers
- recovery assessment states
- restore-time schema uplift
- startup assessment checks and classification
- what runtime state is evicted during repair
- why online per-user restore needs coordination

## 1. Recovery Model

The server treats recovery as a runtime state problem, not just a file-copy
problem.

The key internal question is:

- can this user's state safely resume and converge?

That is different from:

- do all SQLite files merely open without error?

The runtime boundary for this work is now an explicit coordinator:

- `RuntimeRecoveryCoordinator`
- `admin::services::recovery::RecoveryCoordinator`

`AppState` still exposes convenience methods such as `is_user_quarantined()` or
`mark_user_offline()`, but those now delegate into that coordinator rather than
owning the marker/quarantine policy directly.

The higher-level operator-facing recovery flow now goes through
`RecoveryCoordinator`, which unifies:

- user assessment
- offline / online transitions
- startup recovery assessment
- local/offline operator usage and running-server operator usage
- recovery transition audit emission
- recovery metrics emission

## 2. Offline / Quarantine State

The server currently uses one operational mechanism for both:

- corruption-triggered quarantine
- operator-driven temporary offline state

That mechanism is a persisted per-user marker:

- `users/<user_id>/.offline`

If the marker exists:

- the user is treated as offline/quarantined
- REST requests for that user fail with `503`
- TaskChampion sync requests for that user fail with `503`
- runtime state is evicted

If it is removed:

- the user can come back online
- runtime state is reopened from disk on demand

## 3. Why the Marker Is Persisted

Persisting the offline state solves two problems:

- the local admin CLI can coordinate with a running server
- the state survives server restarts

The runtime coordinator owns:

- marker persistence/removal
- in-memory quarantine tracking
- marker-to-memory reconciliation
- runtime eviction on offline/online transitions

That matters for recovery because an operator may:

- take a user offline from the CLI
- restore files
- restart the server
- still expect that user to remain blocked until validation completes

The operator-facing service layer owns the policy decision of when those
runtime capabilities are invoked.

The observability split is now:

- `RuntimeRecoveryCoordinator`
  - owns the authoritative in-memory quarantine set
  - maintains the current quarantine gauge
- `RecoveryCoordinator`
  - owns transition audit events
  - owns recovery assessment/transition counters
  - owns startup recovery summary gauges

## 4. Runtime Eviction on Offline / Quarantine

When the user is taken offline, the server evicts:

- canonical replica cache entries
- per-device sync storage cache entries
- bridge freshness state
- bridge cryptor cache entries related to that user

The goal is to prevent the process from continuing to use stale or suspect
runtime state after the operator or corruption logic has decided the user should
be taken out of service.

## 5. Recovery Assessment States

The current assessment model classifies a user as one of:

- `healthy`
- `rebuildable`
- `needs_operator_attention`

### Healthy

The current user state is coherent enough to resume service normally.

Typical shape:

- canonical replica exists
- sync identity exists if devices exist
- active devices have stored secrets
- the shared `sync.sqlite` exists if this user actually uses sync
- the shared `sync.sqlite` is at the current supported schema level

### Rebuildable

The current state is not complete, but the server should be able to recover
working behaviour from canonical state plus metadata.

Typical shape:

- the shared `sync.sqlite` is missing, older than the current supported schema,
  or otherwise recoverable
- canonical replica and device metadata still exist

This is a logical recovery state, not an exact historical restoration state.

### Needs operator attention

The server lacks some prerequisite needed for safe reuse of the existing user /
device topology.

Examples:

- active device missing stored encryption secret
- devices exist but no canonical sync identity exists
- devices exist but key sync metadata is missing in a way the server cannot
  safely rebuild

In this state, the safer operator choice is often:

- leave the user offline
- repair the data
- or rotate/re-register affected devices

## 6. Startup Recovery vs Online Recovery

There are two conceptually different recovery modes.

### Startup recovery

The process starts by assessing configured users before normal service. Users
already marked offline stay offline; users classified as
`needs_operator_attention` are placed offline automatically and an offline
marker is written to disk; users classified as `rebuildable` remain online but
are surfaced clearly to operators.

### Online per-user recovery

The server remains running, but one user's state is being restored or repaired.

This second case is harder because the process may already have:

- open DB handles
- cached freshness assumptions
- in-flight bridge work
- live client traffic

## 7. Startup Assessment Inputs

The startup recovery pass iterates over users known to `config.sqlite` and
evaluates each user against a small set of structural checks.

Per-user checks currently include:

- whether the user directory exists under `users/<user_id>/`
- whether the canonical replica file exists
- whether the canonical sync identity exists in `config.sqlite`
- whether active devices have stored encrypted secrets
- whether the shared `sync.sqlite` exists when the user topology expects it
- the detected `sync.sqlite` `schema_version`
- the current runtime-supported `sync.sqlite` schema version

The startup pass also scans `users/` for orphan user directories that do not
correspond to a configured user record.

Each startup assessment also emits:

- per-user recovery assessment counters
- startup summary gauges
- audit events for users auto-offlined at boot
- an in-process startup recovery snapshot exposed through `/admin/status`

## 8. Startup Classification Rules

At boot, the current rules are:

- `healthy`
  - no blocking structural gaps were found
- `rebuildable`
  - the shared `sync.sqlite` is missing but can be rebuilt
  - the shared `sync.sqlite` is older than the current runtime storage level
  - the canonical replica is missing, but the user still has sync identity and
    device metadata from which working state can be rebuilt
- `needs_operator_attention`
  - one or more active devices are missing stored encrypted secrets
  - active devices exist but there is no canonical sync identity
  - the shared `sync.sqlite` reports a schema version newer than this binary
    supports

The key policy distinction is:

- missing sync transport state is often rebuildable
- missing credential or identity prerequisites is not

## 9. Startup Actions

For each assessed user:

- already-offline users remain offline
- `healthy` users stay online
- `rebuildable` users stay online
- `needs_operator_attention` users are placed offline automatically

When startup places a user offline, it:

- writes `users/<user_id>/.offline`
- adds the user to the in-memory quarantine set
- evicts cached runtime state for that user
- emits an audit event so the quarantine decision is preserved in off-host
  audit retention as well as local logs

The server also logs a startup summary containing:

- total users assessed
- healthy user count
- rebuildable user count
- needs-operator-attention count
- users already offline
- users newly taken offline
- orphan user directories found on disk

## 10. Why Online Selective Restore Needs Coordination

If an operator replaces files for one user underneath a running server without
coordinating the runtime state, the process may continue to operate using
pre-restore assumptions.

That can produce:

- stale handles against replaced files
- bridge work based on pre-restore state
- confusing post-restore behaviour that is not obvious from the files alone

So the server's recovery model is:

1. take the user offline
2. evict runtime state
3. restore files
4. run any required schema uplift / repair for restored older storage
5. assess resulting state
6. bring the user back online only after review

## 10.1 Restore-Time Schema Uplift

Restore and schema uplift are not the same thing.

A restored user may come from:

- an older backup taken before a schema change
- a backup where only some per-user stores were upgraded
- a point in time before a backfill or metadata repair had completed

So a safe recovery path should be able to answer:

- is the restored user structurally compatible with the current binary?
- if not, can we bring that user forward while they remain offline?

The preferred pattern is:

- keep the user offline
- run an explicit per-user upgrade / repair pass
- reassess
- only then return the user to service

This is cleaner than relying on incidental lazy-open behaviour during the first
live request after restore.

## 11. Rebuildable Sync State

Missing or outdated shared `sync.sqlite` state is special.

If the server still has:

- canonical replica
- sync identity metadata
- the user-level sync identity
- at least one valid device row with stored encrypted device secret

then a missing or older shared sync DB may be classified as rebuildable.

That means the server may be able to recreate working sync behaviour, but not
the exact original transport history graph.

## 12. What Recovery Does Not Mean

Recovery in this server does not imply:

- exact physical replay like a traditional database redo-log recovery system
- proof that every sync history edge exactly matches a prior pre-failure state
- byte-perfect preservation of all transport history

Recovery means:

- safe enough state to resume service and converge

## 13. Current Implemented Surfaces

The main current implementation surfaces are:

- admin HTTP:
  - `/admin/user/{id}/offline`
  - `/admin/user/{id}/online`
  - `/admin/user/{id}/stats`
- admin CLI:
  - `admin user offline`
  - `admin user assess`
  - `admin user online`
  - `admin restore --user-id`

## 14. Audit Notes

Recovery and offline transitions intersect with audit.

Current state:

- manual offline/online actions from CLI and admin HTTP emit audit events
- corruption-triggered quarantine emits audit events
- startup auto-offlining emits an audit event per affected user
- startup recovery also emits a structured summary log for the whole boot pass

## 15. Future Work

Likely future work includes:

- richer recovery diagnostics
- optional repair helpers for rebuildable state
- clearer machine-readable admin recovery APIs for future admin/PWA control
  planes
