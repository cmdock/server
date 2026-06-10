# Per-User Schema Uplift Research Note

Updated: 2026-04-01

Related:

- [Schema and Live Migration Reference](../reference/schema-and-live-migration-reference.md)
- [Recovery Reference](../reference/recovery-reference.md)
- [Administration Guide](../manuals/administration-guide.md)
- [Admin Surfaces Reference](../reference/admin-surfaces-reference.md)

## Purpose

This note explores what the server would need in order to support a **controlled
per-user schema uplift** after restore.

The motivating case is:

- the server has moved to a newer runtime/storage expectation
- one user is restored from an older backup
- the operator does not want that user's first live request to perform ad hoc
  lazy repair
- the operator wants an explicit, inspectable path to bring that user's storage
  forward before the user is brought back online

This is a future implementation note, not a description of current behaviour.

## Problem Statement

The current system already has pieces of the story:

- per-user offline marker
- runtime eviction
- selective restore for one user
- startup and on-demand recovery assessment
- open-time repair logic for the shared per-user sync DB
- startup config DB migrations

But it does **not** yet have an explicit concept of:

- "this restored user is structurally older than the current runtime expects"
- "run the required per-user upgrade now, while offline"
- "show me whether the uplift completed cleanly"

Today, some repairs are implicit:

- config DB migrations happen globally at startup
- sync DB repairs happen lazily when those DBs are opened

That is workable for many cases, but it is weaker than an explicit operator
workflow for restored old user state.

## Design Goals

- let an operator restore one user and upgrade only that user's on-disk state
- keep the user offline until upgrade and reassessment are complete
- make the upgrade path inspectable and repeatable
- avoid inventing one fake global schema version that does not match the real
  storage surfaces
- reuse the same migration/repair code paths the runtime already trusts
- remain safe under retry, crash, and partial progress

## Non-Goals

- replacing startup `config.sqlite` migrations
- adding a full general-purpose database migration framework for TaskChampion
- guaranteeing byte-identical recreation of old device sync history
- making every future schema change online/rolling-upgrade safe in one release

## Storage Surfaces and What They Need

### 1. `config.sqlite`

This DB is already covered by startup migrations and `_migrations`.

For the per-user restore case, the main need is not a new schema version
mechanism for the whole DB. The main need is:

- a way to verify that the restored user rows are compatible with the current
  runtime
- a way to repair any per-user data backfills that newer code now expects

So the server should keep:

- `_migrations` as the authoritative config DB migration ledger

It does **not** need:

- one coarse `config_schema_version` integer to replace `_migrations`

### 1.1 Metadata schema vs per-user metadata uplift

There are two different questions here and they should not be collapsed.

#### Global metadata schema level

This is the normal `config.sqlite` schema question:

- do the tables/columns/indexes required by the current binary exist?

For that, `_migrations` is still the right answer.

#### Restored user metadata currentness

This is the selective-restore question:

- after importing one user's rows from an older backup into the current
  `config.sqlite`, are that user's rows at the semantic level current code
  expects?

That is **not** the same as the global schema version.

Examples:

- the main `devices` table may have the current columns
- but restored rows for one user may still be missing values that newer code
  expects to derive or backfill

So the likely future need is:

- keep `_migrations` for global DB schema
- add a **per-user metadata uplift level** only if we accumulate enough
  user-scoped metadata repair/backfill logic that it is worth tracking

Possible shape:

- `user_storage_versions`
  - `user_id`
  - `metadata_uplift_version`
  - `updated_at`

This would not replace `_migrations`. It would answer a different question:

- has this restored user's metadata been brought up to the current semantic
  level after import?

### 2. Shared per-user sync DB

This is the strongest case for explicit version metadata.

Today, it relies mainly on structural detection and repair logic on open. That
is safe, but not ideal for a controlled per-user uplift workflow.

The server likely needs:

- `metadata.schema_version`
- optional later `metadata.repair_version` if we accumulate substantial
  backfills that are logically separate from physical schema layout

That would let the server say:

- this user's shared `sync.sqlite` is at schema version 1
- current runtime expects schema version 2
- uplift is required before this user should resume service

### 3. Canonical replica

The canonical TaskChampion replica is the least attractive place to introduce
our own version ledger.

The preferred approach is:

- treat it as TaskChampion-managed storage
- use structural/open probes and compatibility checks
- add explicit uplift hooks only if we later have repo-owned metadata or repair
  expectations around that replica

## Full DR Restore vs Selective Per-User Restore

These are different upgrade problems and the server should treat them
differently.

### Full DR restore

For full-system restore, the right model is:

1. restore the whole backup set
2. start the current binary
3. run global `config.sqlite` migrations
4. run startup recovery assessment
5. keep broken users offline if needed

This is mostly a **global startup migration** problem.

### Selective per-user restore

For single-user restore into a live or current system, the problem is harder:

- the current `config.sqlite` may already be on the latest schema
- the backup `config.sqlite` may be older
- the restored user rows may not match the current semantic expectations

This is a **user-scoped metadata import + uplift** problem, not just a database
migration problem.

That distinction matters for implementation.

## Proposed Server Capabilities

To support controlled per-user uplift, the server would need five concrete
capabilities.

### A. Per-user storage version assessment

Add an internal assessment that reports, per user:

- config DB compatibility status for that user
- per-user metadata uplift status
- canonical replica probe result
- shared sync DB:
  - exists?
  - opens?
  - schema version
  - repair version if used
  - expected version
  - uplift needed?

This should be richer than today's recovery assessment.

Suggested shape:

- `assess_user_storage_state(user_id) -> UserStorageAssessment`

Useful fields:

- `user_id`
- `status`
  - `current`
  - `upgradeable`
  - `needs_operator_attention`
- `canonical_probe`
- `metadata_state`
- `device_sync_states[]`
- `notes[]`

### B. Explicit per-user uplift service

Add a dedicated internal service that upgrades a user's storage while the user
is offline.

Suggested shape:

- `upgrade_user_storage(user_id) -> UserUpgradeReport`

This should:

1. verify the user is offline
2. reopen storage from disk cleanly
3. run required config-level per-user metadata uplift/backfills if any exist
4. run sync DB upgrade/repair for the shared per-user `sync.sqlite`
5. collect a report of what changed

Important:

- this must call shared repair helpers, not reimplement migration logic in a
  second ad hoc path
- it must be idempotent

### C. Admin surface for uplift

The operator needs a first-class surface for this.

Minimum CLI:

- `admin user assess-storage <user-id>`
- `admin user upgrade-storage <user-id>`

Possible future convenience command:

- `admin user prepare-online <user-id>`

That wrapper could:

1. verify offline state
2. run upgrade
3. run recovery/storage assessment
4. print whether it is safe to bring the user online

If the admin HTTP surface later grows, the likely parallel endpoints are:

- `POST /admin/user/{id}/upgrade-storage`
- `GET /admin/user/{id}/storage-state`

### D. Runtime coordination and locking

Offline marker plus cache eviction gets most of the way there, but a proper
upgrade path should also make race behaviour explicit.

The server likely needs:

- a per-user admin upgrade lock or lane
- a rule that storage upgrade refuses to run unless the user is offline
- startup recovery and runtime request paths to treat "upgrade in progress"
  distinctly if that state is persisted

A light-weight first step would be:

- require `.offline`
- evict runtime state
- perform upgrade from CLI only

A stronger future design could add:

- `.upgrade_in_progress` marker or equivalent persisted state

That would help with crash recovery and clearer operator diagnostics.

### E. Audit and metrics

If this becomes real, it should not be silent.

Audit events:

- `admin.user.storage_upgrade_started`
- `admin.user.storage_upgrade_completed`
- `admin.user.storage_upgrade_failed`

Metrics:

- `storage_upgrade_total{result=...}`
- `storage_upgrade_duration_seconds`
- `storage_upgrade_device_db_total{result=...}`

This matters because restore-time upgrade failures are operationally important.

### F. A safer selective metadata restore path

This is the biggest metadata-specific requirement.

The current selective restore model conceptually does:

- attach backup DB
- copy a user's rows back into the current DB

For future schema-heavy restores, that approach needs to become schema-aware.

The server likely needs a restore/import layer that:

1. inspects the current table columns
2. inspects the backup table columns
3. copies the intersection explicitly by column name
4. fills missing current columns with:
   - default values
   - `NULL`
   - or explicit derived placeholders
5. runs per-user metadata uplift after import

This is safer than `INSERT ... SELECT * ...` because column shape may diverge
across backup age and current runtime.

That import layer is probably a prerequisite for reliable per-user restore
across meaningful metadata schema evolution.

## Proposed Versioning Approach

The versioning model should be per storage surface, not fake-global.

### `config.sqlite`

Keep:

- `_migrations`

Use it to answer:

- has the global config schema reached the current binary's level?

Do not replace it with one integer.

Potentially add later:

- per-user metadata uplift tracking

But only for restored-row currentness, not as a replacement for global schema
migration bookkeeping.

### Device sync DBs

Add:

- `metadata.schema_version`

Potentially later add:

- `metadata.repair_version`

Recommended semantics:

- `schema_version`
  - physical layout level
- `repair_version`
  - post-schema backfill / metadata repair completion level

Why both might matter:

- columns may exist
- but `latest_seq`, derived secrets, or other metadata may still need repair

If we can avoid the extra complexity at first, start with:

- `schema_version` only
- structural verification as the final truth check

## Upgrade Execution Model

The safest execution model is:

1. user is offline
2. runtime state is evicted
3. upgrade service opens each relevant store directly
4. each step is applied idempotently
5. version marker is bumped only after the step is actually true
6. report is written to the operator
7. reassessment runs before the user is brought online

For sync DBs specifically, the implementation pattern should be:

1. read `schema_version`
2. inspect actual structure
3. apply missing DDL
4. run backfills/repairs
5. validate resulting invariants
6. write new `schema_version`

Never rely on the version marker alone.

For metadata import during selective restore, the execution model should be:

1. ensure current `config.sqlite` is fully migrated
2. attach the backup DB
3. import the user's rows with schema-aware column mapping
4. run per-user metadata uplift
5. restore user replica files
6. run sync-storage uplift
7. reassess before online

## Integration With Existing Recovery Flow

The desired operator flow becomes:

```text
offline -> restore -> metadata-uplift -> storage-uplift -> assess -> online
```

That implies the server should distinguish:

- recovery assessment
- storage version assessment
- storage uplift

Current `assess_user_recovery()` is a good starting point, but it is focused on
structural recovery, not version/currentness.

A future model could look like:

- `assess_user_recovery()`
  - can this user safely resume at all?
- `assess_user_metadata_state()`
  - were restored metadata rows imported and uplifted to the current semantic level?
- `assess_user_storage_state()`
  - is this user at the current runtime storage level?
- `upgrade_user_storage()`
  - bring the user forward while offline

## Risks and Tradeoffs

### Good

- explicit operator-controlled upgrade path
- better restore story after schema changes
- clearer diagnostics than lazy repair during first live request
- easier to test and reason about

### Costs

- more state and code paths
- versioning logic for sync DBs must be maintained carefully
- extra admin CLI/API surface
- more cases in recovery and startup diagnostics

### Main risk

The main risk is duplicating migration logic in multiple places.

Avoid this by ensuring:

- open-time repair helpers and explicit upgrade commands use the same internal
  functions

The operator surface should orchestrate upgrade, not own the low-level repair
rules.

## Suggested Phased Implementation

### Phase 1: Assessment only

Add:

- sync DB `schema_version`
- metadata import/currentness assessment
- storage-state assessment output
- richer admin diagnostics

Do not yet add an explicit upgrade command if the repair logic is still too
implicit.

### Phase 2: Explicit offline upgrade command

Add:

- schema-aware selective metadata import helpers
- `admin user upgrade-storage <user-id>`
- audit + metrics
- tests for restore-old-user -> upgrade -> assess

### Phase 3: Convenience workflow

Add:

- `admin user prepare-online <user-id>`
- optional admin HTTP surface
- clearer startup reporting around users who are offline only because uplift is pending

## Testing Requirements

This feature would need more than unit tests.

### Unit

- schema version parsing
- step ordering
- idempotent rerun of each repair helper

### Integration

- old `sync.sqlite` -> explicit upgrade -> expected version
- partial upgrade state -> rerun -> converged result
- upgrade report output

### System / recovery

- restore one user from an older backup
- keep user offline
- run upgrade-storage
- assess
- bring online
- verify unaffected users remain online throughout

### Negative cases

- corrupted DB cannot be upgraded
- missing secret metadata blocks upgrade
- upgrade interrupted and retried
- version marker says new but structure is old

## Recommendation

The most practical future approach is:

- keep `_migrations` for global config DB schema
- treat restored per-user metadata uplift as a separate concern from global schema migration
- replace schema-blind selective metadata restore with column-aware import + uplift
- add explicit `schema_version` metadata for the shared per-user `sync.sqlite`
- introduce a dedicated per-user offline storage upgrade service
- expose that service first through the admin CLI
- keep recovery and uplift as separate but connected phases

That gives the server a controlled way to bring a restored old user forward
without pretending that one global schema number can describe every storage
surface in the system.
