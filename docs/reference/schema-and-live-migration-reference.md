# Schema and Live Migration Reference

This document describes how to think about schema changes in `cmdock-server`,
especially when the system is live.

It is aimed at developers and reviewers, not day-to-day operators.

Related:

- [Administration Guide](../manuals/administration-guide.md)
- [Storage Layout Reference](storage-layout-reference.md)
- [Recovery Reference](recovery-reference.md)
- [Testing Strategy Reference](testing-strategy-reference.md)

## 1. Why This Needs Its Own Note

The project has more than one SQLite schema surface:

- `config.sqlite`
  - server metadata
  - migrated via numbered SQL files under `migrations/`
- canonical TaskChampion replica
  - managed by TaskChampion, not by our migration files
- shared per-user sync DB
  - primary runtime path is `users/<user-id>/sync.sqlite`
  - schema owned in Rust code in `tc_sync/storage.rs`
  - self-healed on open with idempotent migration / backfill logic

Those surfaces do not evolve the same way.

If we do not document that clearly, people will assume every schema change is
"just add a migration file", which is not true here.

## 2. Current Runtime Reality

### `config.sqlite`

Today, the server runs config DB migrations at startup before serving traffic.

That means:

- startup is a schema gate
- failed config migrations prevent the process from starting
- each migration file is run inside its own SQLite transaction

This is fine for the current single-process self-hosted model.

### Shared per-user sync DB

Shared TaskChampion sync storage is different.

That DB is opened lazily and the runtime applies schema repair and data
backfill logic on open.

That means these migrations must be:

- idempotent
- safe to rerun
- safe after partial completion
- robust against crash-restart mid-upgrade

### Canonical TaskChampion replica

Treat the canonical replica as TaskChampion-owned storage.

Do not invent ad hoc migration logic for that DB unless there is no other
choice and the invariants are extremely clear.

## 3. The Main Rule

Prefer **expand, backfill, switch, contract**.

In practice:

1. add new schema in a backwards-compatible way
2. make new code tolerate both old and new shapes
3. backfill gradually or lazily
4. switch reads/writes to the new shape
5. only then remove the old shape

This is the default migration pattern for any change that affects live or
restart-safe state.

## 4. Change Categories

### Safe additive changes

Usually safe:

- adding a nullable column
- adding a column with a sensible default
- creating a new table
- creating a new index

These are the best kind of changes because old code usually ignores them.

Example:

- `devices.encryption_secret_enc` was added as nullable first so existing rows
  could be backfilled later

### Additive changes with backfill

Common pattern:

- add nullable or defaulted column
- deploy code that can handle missing values
- backfill on first use or via targeted repair path
- later tighten invariants if needed

This is often the right approach for large datasets or values derived from
existing state.

### Behavioural or semantic changes

Higher risk:

- changing uniqueness rules
- changing auth semantics
- changing what a row means
- splitting one concept into two tables or identities

These need more than SQL. They need compatibility logic and rollout planning.

### Destructive changes

Highest risk:

- dropping a column or table
- renaming a field that older code still expects
- making a previously-nullable field required without a full backfill

These should almost never ship in one step.

## 5. Config DB Migration Rules

For `config.sqlite`, use numbered SQL files under `migrations/`.

### Should `config.sqlite` have a single schema version number?

Not as the primary mechanism.

For this DB, the current `_migrations` table is the right source of truth
because it tells us exactly which migrations ran, not just one coarse version
integer.

That is better than a single `schema_version = 13` style marker when:

- migrations may be added over time
- operators may retry startup after failure
- we need to know which exact step has or has not run

Recommended rule:

- keep `_migrations` as the authoritative migration ledger
- do not replace it with one global version integer
- if a coarse release marker is ever useful, treat it as advisory metadata, not
  the migration truth source

### Good rules

- migrations must be ordered and append-only
- each file should do one coherent change
- assume startup may be retried after partial operator work
- prefer additive SQL first
- use idempotent forms where SQLite supports them

### Do not assume

- that every DDL change is safely reversible
- that rollback means "run the old binary and everything is perfect"
- that a migration file alone is enough when code behaviour also changes

### Review checklist for a config DB migration

- can old code still run against the migrated schema if rollback is needed?
- can new code run safely before every row is backfilled?
- is there a default or nullability path for older rows?
- do admin CLI and HTTP handlers both tolerate the intermediate state?
- do docs and recovery expectations change?

## 6. Sync Storage Migration Rules

The shared per-user `sync.sqlite` DB is special because migration logic lives
in code and runs when the DB is opened.

### Should sync DBs have an explicit schema version?

Yes.

Unlike `config.sqlite`, this DB is not migrated by an append-only external
migration ledger. It is opened lazily and repaired in code.

That makes an explicit metadata key such as `schema_version` useful for:

- fast branching in open-time migration code
- clearer tests
- better diagnostics in admin/recovery tooling
- distinguishing "schema step not applied" from "column exists but backfill is incomplete"

Current direction in this repo:

- `metadata.schema_version` is now present on shared `sync.sqlite`
- open-time upgrade still uses structural checks and backfill helpers as the
  final truth

Recommended rule:

- keep structural detection for safety
- add an explicit `metadata.schema_version` as an optimisation and diagnostic aid
- do not trust the version marker alone

In other words:

- version marker tells us what should be true
- structural checks confirm what is actually true

That combination is safer than either approach on its own.

### Required properties

- idempotent
- crash-safe
- tolerant of partially migrated DBs
- able to detect missing columns or missing metadata independently

The current `seq` migration is the model:

- detect missing `versions.seq`
- detect missing `snapshots.seq`
- backfill versions independently
- backfill snapshot metadata independently
- repair `latest_seq` metadata independently

That pattern matters because SQLite DDL and multi-step backfills may not behave
as one clean all-or-nothing unit in the real world.

### Design principle

If a sync DB migration has multiple steps, write each step so it can be rerun
without harming already-correct state.

### Practical pattern for sync DBs

For future sync-storage migrations, prefer:

1. read `schema_version` from metadata if present
2. inspect structure directly for the fields/tables the current code needs
3. run any missing schema step
4. run any required backfill step
5. only then advance `schema_version`

That ordering matters.

Do not bump the version marker before the data and metadata repairs that make
the new version actually true.

Also:

- reject sync DBs whose `schema_version` is newer than the running binary
  supports
- surface current vs expected schema version in recovery/admin diagnostics

## 6.1 Restore-Time Schema Uplift

This is the important special case for recovery work:

- a user is restored from an older backup
- the running binary expects a newer schema or metadata shape
- we need a controlled way to bring that user's on-disk state forward before
  normal traffic resumes

The right mental model is:

- **restore** gets the user back onto disk
- **schema uplift** brings that restored state up to the runtime's expected
  storage level
- **assessment** decides whether the user can safely come back online

These should be treated as separate steps.

### Recommended operational sequence

For per-user restore after a schema-changing release:

1. mark the user offline
2. evict runtime state
3. restore the user's files from backup
4. run a per-user schema uplift / repair pass
5. reassess the resulting state
6. only then bring the user back online

Do not rely on the first live request from that user to discover and repair a
large schema gap implicitly.

### Why this matters

Without a controlled uplift step, a restored user may be:

- structurally older than the current runtime expects
- only partly upgraded by ad hoc on-open logic
- exposed to live traffic before repair has completed

That creates exactly the kind of ambiguous "it opens, but is it actually safe?"
state we want to avoid in recovery.

## 6.2 Recommended Versioning Strategy For Restore/Uplift

If we want controlled per-user uplift, we need enough metadata to answer:

- what level is this storage currently at?
- what level does the current binary expect?
- did the uplift finish, or did it stop halfway?

### `config.sqlite`

Keep using `_migrations` as the source of truth.

That already answers the global config DB question better than a single integer
version.

### Shared per-user sync DB

Add explicit metadata keys such as:

- `schema_version`
- optional `data_version` or `repair_version` if backfills become substantial

Suggested meaning:

- `schema_version`
  - physical schema/layout level of `sync.sqlite`
- `data_version` / `repair_version`
  - higher-level backfill or metadata-repair completion level

This split is useful when a DB can have:

- the right columns and tables
- but still need additional backfill or metadata repair before it is truly
  "current"

If we do not need that distinction yet, start with `schema_version` only.

### Canonical TaskChampion replica

Do **not** invent a parallel schema version unless there is a concrete need.

Prefer:

- structural/open checks
- TaskChampion-managed compatibility behaviour
- a separate recovery/uplift assessment that knows what the current runtime
  requires around that replica

## 6.3 Controlled Uplift Design

The engineering pattern worth aiming for is a dedicated per-user uplift path,
not scattered implicit fixes.

Good shape:

- `assess_user_storage_versions(user_id)`
  - inspects relevant per-user stores, especially `sync.sqlite`
  - reports current vs expected levels
- `upgrade_user_storage(user_id)`
  - runs the required uplift steps under explicit operator-controlled state
- `assess_user_recovery(user_id)`
  - confirms the post-upgrade state is acceptable

This gives us a clean lifecycle:

- offline
- upgrade
- reassess
- online

### Important constraint

Upgrade code should still be idempotent and safe to rerun.

Even in an explicit uplift workflow, crashes and retries are normal recovery
conditions.

## 6.4 Recommendation For This Repo

For `cmdock-server`, the practical recommendation is:

- keep `_migrations` for `config.sqlite`
- add explicit `schema_version` metadata to the shared per-user `sync.sqlite`
- continue using structural detection as the final truth check
- add a dedicated per-user offline uplift step for restore/recovery workflows

That gives us a controlled way to bring an old restored user forward without
pretending one global version marker can describe every storage surface.

## 7. What "Live Migration" Means Here

In this codebase, "live migration" can mean different things.

### Single-instance self-hosted

Today, config DB migrations are effectively **restart-time migrations**:

- stop old process
- start new process
- run migrations
- begin serving

That is acceptable for the current deployment model.

### Multi-instance or near-zero-downtime future

If we later run multiple instances or want true rolling upgrades, schema changes
must be safe across a compatibility window where:

- old binaries may still be running
- new binaries may start reading the same DB
- not every row is backfilled yet

That means destructive or tightly-coupled schema changes require at least two
releases:

1. additive schema + compatibility code
2. cleanup / contraction after the old shape is no longer needed

## 8. Recommended Migration Patterns

### Pattern A: Add column, lazy backfill

Use when:

- new value can be derived from existing state
- backfill can happen on read/open/first use

Sequence:

1. add nullable/defaulted column
2. ship code that tolerates missing values
3. backfill lazily
4. add stronger invariants only later if truly needed

### Pattern B: Add new table, dual-read

Use when:

- one logical concept is moving into a cleaner structure

Sequence:

1. create new table
2. write new code that can read old and new paths
3. backfill data
4. switch writes fully
5. remove fallback later

### Pattern C: Expand / contract for destructive changes

Use when:

- an old column or table must eventually go away

Sequence:

1. expand schema
2. deploy compatibility code
3. migrate data
4. verify no reads depend on old shape
5. contract in a later release

## 9. Rollback Expectations

Be precise about rollback.

### Best case

If a startup SQL migration fails inside its transaction:

- migration is rolled back
- server does not start
- data stays unchanged

### Harder case

If new code has already started writing new semantics, "rollback to old binary"
may not be enough unless the old binary tolerates the migrated schema and new
data shape.

So before merging a schema change, ask:

- what does rollback actually mean?
- can the prior binary still read the post-migration state?
- if not, do we need a staged release instead?

## 10. Operational Discipline

Before any meaningful schema change:

- take a real backup
- test migration on a copy of production-like data
- run startup/migration path in staging
- check recovery implications
- check admin CLI paths as well as HTTP paths

For this repo specifically, also think about:

- startup recovery assessment
- device lifecycle semantics
- cache invalidation after semantic changes
- metrics and audit signals for new failure modes

## 11. Testing Expectations

Schema changes are not done when the SQL parses.

At minimum, test:

- fresh install path
- upgrade from previous schema
- restart after migration
- repeated open or repeated migration call
- partially migrated state if code-driven migration is involved
- old rows with missing/newly derived values

High-value tests in this repo include:

- integration tests around `run_migrations()`
- open-time tests for sync storage repair paths
- recovery tests when metadata and on-disk files disagree
- staging / UAT flows if provisioning or auth semantics change

## 12. When To Write a New ADR

Write an ADR when the change is not just a schema tweak but a model change.

Examples:

- one identity becomes two identities
- one storage surface becomes many
- auth or revocation semantics change
- migration introduces a long-lived compatibility burden

If the schema change changes the product or runtime model, document the
decision separately from the migration mechanics.

## 13. Practical Checklist

Before merging a schema-related change, confirm:

- which DB surface is changing:
  - `config.sqlite`
  - sync storage
  - TaskChampion-managed storage
- whether the change is additive or destructive
- whether old and new binaries can overlap safely
- whether backfill is eager, lazy, or on-open
- whether the change is idempotent
- whether failure leaves the system recoverable
- whether tests cover upgrade and partial-state paths
- whether docs and operator runbooks need updating

## 14. Recommended Default for This Repo

For `cmdock-server`, the safe default is:

- config DB:
  - additive SQL migration
  - compatibility in handlers/store layer
  - staged cleanup later
- sync storage:
  - open-time idempotent migration and repair logic
  - explicit tests for partial progress and crash-retry

If a proposed migration does not fit that shape, treat it as high-risk until
proved otherwise.
