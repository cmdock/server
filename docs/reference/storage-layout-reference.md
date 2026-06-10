# Storage Layout Reference

This document describes the on-disk layout used by the server and the role of
each storage surface.

For backup and restore procedure, use
[Backup and Recovery Guide](../manuals/backup-and-recovery-guide.md).

## 1. Top-Level Layout

Typical structure:

```text
data/
├── config.sqlite
├── config.sqlite-wal
├── config.sqlite-shm
└── users/
    └── <user-id>/
        ├── taskchampion.sqlite3
        ├── taskchampion.sqlite3-wal
        ├── taskchampion.sqlite3-shm
        ├── merged/
        │   ├── replica/
        │   │   └── taskchampion.sqlite3
        │   ├── sync.sqlite
        │   ├── sync.sqlite-wal
        │   └── sync.sqlite-shm
        ├── .offline
        ├── sync.sqlite              # optional legacy/non-serving artifact
        └── sync/
            └── <client-id>.sqlite   # optional legacy/maintenance artifact
```

Not every file exists at all times. WAL/SHM files depend on SQLite activity and
checkpoint timing.

## 2. `config.sqlite`

This is the server metadata database.

It stores:

- users
- API tokens
- views / contexts / presets / stores / other config
- canonical sync identity metadata
- device registry metadata

What it does not store:

- the canonical task graph itself
- the merged TaskChampion sync DB

## 3. Canonical Replica

Per user:

- `users/<user-id>/taskchampion.sqlite3`

This is the canonical per-user TaskChampion replica used by the server for:

- REST task reads
- REST task writes
- canonical bridge state

This is the server-side task source of truth for normal API behavior.

## 4. Merged Gateway Sync State

Per user:

- `users/<user-id>/merged/replica/taskchampion.sqlite3`
- `users/<user-id>/merged/sync.sqlite`

The merged gateway sync DB stores the TaskChampion protocol version chain and
snapshots served by `/v1/client/*`. The merged replica is a durable projection
cache used to build that chain from canonical source truth.

It exists because the TaskChampion client protocol expects version-chain and
snapshot semantics, not direct access to the canonical server task DB. Devices
still have distinct credentials and are revoked independently, but they share
this one gateway-backed server-side sync chain.

### Optional legacy sync artifacts

Some upgraded or test environments may still have files at:

- `users/<user-id>/sync.sqlite`
- `users/<user-id>/sync/<client-id>.sqlite`

These are not served by `/v1/client/*` after the merged-gateway cutover. Normal
device registration creates/opens `users/<user-id>/merged/sync.sqlite`; legacy
files are retained only for backups, diagnostics, or explicit operator cleanup.

## 5. `.offline`

Per user:

- `users/<user-id>/.offline`

This is a persisted runtime coordination marker.

If present:

- the user is treated as offline/quarantined
- runtime state is evicted
- normal requests for that user are blocked

## 6. Authoritative vs Rebuildable

### Authoritative

- `config.sqlite` for metadata
- `taskchampion.sqlite3` for canonical server-side task state

### Rebuildable

The merged gateway sync DB and merged projection replica may be logically
rebuildable if enough metadata and canonical state still exist.

That does not make them unimportant to back up. It just means they are not the
same kind of authority surface as canonical state.

## 7. WAL and SHM Files

SQLite may use:

- `-wal`
- `-shm`

for:

- `config.sqlite`
- canonical replica DBs
- merged gateway sync DBs

Operators should treat them as part of the live SQLite state while the DB is
active.

## 8. Lifecycle Effects

### Device create

Typically results in:

- device row in `config.sqlite`
- merged gateway sync DB present on disk

### Device revoke

Typically results in:

- metadata status change only
- merged gateway sync DB remains

### Device delete

Typically results in:

- device row removal
- no change to the merged gateway sync DB
- optional legacy per-device file cleanup only if one already exists from an
  older/manual environment

### User offline

Typically results in:

- `.offline` marker created

### User online

Typically results in:

- `.offline` marker removed

## 9. Restore Implications

Storage restore can produce a coherent or mixed-point state.

Important examples:

- restoring `config.sqlite` without matching user files
- restoring the canonical replica without matching `merged/sync.sqlite`
- restoring one user directory without corresponding metadata changes

That is why restore is treated as both:

- a file operation
- and a runtime coordination / assessment event

## 10. Future Considerations

Likely future additions to the storage layout model include:

- startup assessment markers or richer recovery metadata
- more explicit backup manifests
- future remote operator metadata
