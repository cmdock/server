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
        ├── sync.sqlite
        ├── sync.sqlite-wal
        ├── sync.sqlite-shm
        ├── .offline
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
- the shared TaskChampion sync DB

## 3. Canonical Replica

Per user:

- `users/<user-id>/taskchampion.sqlite3`

This is the canonical per-user TaskChampion replica used by the server for:

- REST task reads
- REST task writes
- canonical bridge state

This is the server-side task source of truth for normal API behavior.

## 4. Shared Sync DB

Per user:

- `users/<user-id>/sync.sqlite`

This DB stores TaskChampion sync protocol state for that user.

It exists because the TaskChampion client protocol expects version-chain and
snapshot semantics, not direct access to the canonical server task DB.

Devices still have distinct credentials and are still revoked independently,
but they share this one server-side sync DB.

### Optional legacy per-device files

Some environments may still have files under:

- `users/<user-id>/sync/<client-id>.sqlite`

These are not part of the normal hot path.
Normal device registration and normal admin diagnostics no longer create or
scan them.

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

The shared sync DB may be logically rebuildable if enough metadata and
canonical state still exist.

That does not make them unimportant to back up. It just means they are not the
same kind of authority surface as canonical state.

## 7. WAL and SHM Files

SQLite may use:

- `-wal`
- `-shm`

for:

- `config.sqlite`
- canonical replica DBs
- shared sync DBs

Operators should treat them as part of the live SQLite state while the DB is
active.

## 8. Lifecycle Effects

### Device create

Typically results in:

- device row in `config.sqlite`
- shared sync DB present on disk

### Device revoke

Typically results in:

- metadata status change only
- shared sync DB remains

### Device delete

Typically results in:

- device row removal
- no change to the shared sync DB
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
- restoring canonical replica without matching `sync.sqlite`
- restoring one user directory without corresponding metadata changes

That is why restore is treated as both:

- a file operation
- and a runtime coordination / assessment event

## 10. Future Considerations

Likely future additions to the storage layout model include:

- startup assessment markers or richer recovery metadata
- more explicit backup manifests
- future remote operator metadata
