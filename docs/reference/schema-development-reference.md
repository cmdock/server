# Schema Development Guidelines

This document is a practical developer guideline for changing storage schemas in
`cmdock-server` after the migration infrastructure exists.

Use it together with:

- [Schema and Live Migration Reference](schema-and-live-migration-reference.md)
- [Storage Layout Reference](storage-layout-reference.md)
- [Testing Strategy Reference](testing-strategy-reference.md)
- [Recovery Reference](recovery-reference.md)

## 1. Purpose

The goal is to keep schema changes predictable, reviewable, and recoverable.

This guideline is intentionally practical. It is about how to develop and ship
schema changes in this repo, not just how to reason about them abstractly.

## 2. Storage Surfaces

There are three different schema-governance models:

- `config.sqlite`
  - repo-owned SQL migrations
  - `_migrations` is authoritative
- shared per-user `sync.sqlite`
  - repo-owned code-driven open-time uplift
  - `metadata.schema_version` is advisory and diagnostic
- canonical TaskChampion replica
  - TaskChampion-managed storage
  - probe and assess rather than layering repo-owned migration ledgers on top

Do not treat these as one generic migration problem.

## 3. Pre-Freeze vs Post-Freeze

### Pre-freeze development

Before the first compatibility freeze:

- it is acceptable to edit the latest unreleased SQL migration
- it is acceptable to reshape sync-storage upgrade steps
- fixtures may be rebuilt as the baseline changes

This is the phase where we optimise for iteration speed.

### Post-freeze / release compatibility

After the first schema freeze:

- existing SQL migration files are immutable
- migration numbering is append-only
- sync-storage schema versions are append-only
- previously released backup/restore paths are compatibility obligations

This is the phase where we optimise for upgrade safety.

## 4. Change Design Rules

Default rule:

- prefer expand, backfill, switch, contract

That means:

1. add compatible schema first
2. make new code tolerate old and new states
3. backfill explicitly or lazily
4. switch reads/writes
5. remove old shape only after the transition window

Avoid one-step destructive changes whenever possible.

## 5. `config.sqlite` Rules

When changing `config.sqlite`:

- add a new numbered SQL file under `migrations/`
- keep each migration focused
- assume startup may be retried after failure
- prefer additive changes first
- do not replace `_migrations` with one coarse version integer

Review questions:

- can the new binary tolerate partially backfilled rows?
- do CLI and HTTP admin surfaces both handle the intermediate state?
- do backup/restore and recovery docs need updating?

## 6. Shared `sync.sqlite` Rules

When changing shared sync storage:

- add or update explicit upgrade helpers in `src/tc_sync/storage.rs`
- keep structural checks as the final truth
- treat `metadata.schema_version` as advisory, not sufficient
- make each step idempotent and crash-safe
- only advance `schema_version` after the schema and backfill are truly complete
- reject unsupported newer schema versions clearly

Good pattern:

1. read `schema_version` if present
2. inspect real structure
3. run missing DDL
4. run missing backfill/metadata repair
5. write the new `schema_version`

## 7. Recovery and Restore Rules

Schema work is not done until recovery paths are considered.

Every schema-affecting change should consider:

- full restore from an older backup
- selective per-user restore into a newer running system
- startup recovery assessment
- explicit offline uplift before re-enabling a user

If a restored user may require special handling, that needs both code and docs.

## 8. Required Test Updates

Every meaningful schema change should update some mix of:

### Unit

- version parsing
- idempotent rerun behavior
- helper-level backfill logic

### Integration

- old DB opens and upgrades
- partial upgrade state converges
- admin/recovery assessment reflects the new state

### System / Recovery

- restore from older backup
- keep user offline until uplift and assessment succeed
- unaffected users remain online

Do not treat migration tests as optional.

## 9. Backup Fixture Policy

Before freeze:

- fixture regeneration is acceptable

After freeze:

- keep release-tagged fixtures stable
- prefer fixtures produced by real released binaries
- include old-backup restore tests in the regression suite

## 10. Review Checklist

Before merging a schema change, verify:

- which storage surface is changing?
- what is the compatibility window?
- what is the rollback story?
- what happens on restart after a partial upgrade?
- what happens on restore from an older backup?
- what tests were added or updated?
- what docs were updated?

## 11. Anti-Patterns

Avoid:

- editing old released migration files
- relying only on a version marker without structural checks
- letting the first live request discover a major restore-time schema gap
- implementing one-off repair logic in a CLI path instead of shared helpers
- changing schema without updating recovery and testing expectations
