# Disaster Recovery Architecture

**Status:** Design — not yet implemented

This document is an architecture/reference document, not an operator runbook.

Use it for:

- evaluating DR tiers
- understanding RPO/RTO trade-offs
- comparing replication approaches
- planning future hosted or multi-region architecture

Do not use it as the primary restore procedure document.

For operator backup, restore, and validation steps, use the
[Backup and Recovery Guide](../manuals/backup-and-recovery-guide.md).

## Problem Statement

The current architecture runs as a single process on a single server with SQLite files on local disk. If the server or its availability zone goes down, the service is unavailable until a new instance is provisioned and data is restored from backup.

This document evaluates DR strategies from simple backup-restore through to active-active multi-region, with specific analysis of whether TaskChampion's built-in sync protocol can serve as an application-layer replication mechanism.

---

## Data Classification

Understanding what needs replicating and its consistency requirements:

| Data | Storage | Size | Consistency need | Change rate |
|------|---------|------|-----------------|-------------|
| **Config DB** (users, tokens, views, contexts, presets) | config.sqlite | Small (~100KB + 1KB/user) | Strong — auth must be consistent | Low (config changes are rare) |
| **Task replicas** (per-user task data) | users/{id}/taskchampion.sqlite | Medium (~2KB/task) | Eventual OK — tasks sync naturally | Medium (users add/complete tasks) |
| **Server binary** | /usr/local/bin/ | ~15MB | N/A — artifact from build | Per release |
| **Server config** | config.toml | <1KB | N/A — version controlled | Per release |

**Key insight:** Config data needs strong consistency (you can't have two servers disagree on whether a token is valid). Task data can tolerate eventual consistency (users won't notice if a task appears 1-2 seconds late on the standby).

---

## DR Tiers

These tiers describe architecture choices, not the procedural recovery steps.
Even when a higher tier is adopted, the backup/restore validation workflow in
the Backup and Recovery Guide still applies.

### Tier 1: Backup to External Storage (minimum viable DR)

**RPO:** Hours (backup frequency). **RTO:** 15-30 minutes.

```
Primary Server (zone-a)
├── data/config.sqlite
├── data/users/*/
└── cron: hourly backup → S3

On failure:
1. Launch new instance (zone-b)
2. Pull latest backup from S3
3. Install binary (from container registry or S3)
4. Start server
5. Update DNS (Route53 failover)
```

**Architecture changes needed:** None to server code. Add external backup
shipping around the procedures documented in the Backup and Recovery Guide.

**Trade-offs:**
- Simple to implement and operate
- Data loss = time since last backup (1 hour default)
- Manual or scripted failover (15-30 min RTO)
- Good enough for small and early deployments

### Tier 2: Continuous SQLite Replication (Litestream)

**RPO:** Seconds (continuous WAL streaming). **RTO:** 5-10 minutes.

```
Primary (zone-a)                    S3 (cross-zone)
├── config.sqlite ──WAL stream──→  s3://backups/config/
├── users/abc/taskchampion.sqlite → s3://backups/users/abc/
├── users/def/taskchampion.sqlite → s3://backups/users/def/
└── Litestream (sidecar process)

Standby (zone-b) — cold, launched on failure
├── Litestream restore from S3
├── Start server
└── DNS failover
```

[Litestream](https://litestream.io/) continuously streams SQLite WAL changes to S3. On failure, a new server restores from S3 and has data that's seconds old, not hours.

**Architecture changes needed:** None to server code. Deploy Litestream as a sidecar.

**Trade-offs:**
- Near-zero RPO without changing the application
- Still cold standby (RTO = launch time + restore time)
- Works with ALL data (config + replicas) — no split-brain risk
- Litestream is proven, maintained, and designed for this exact use case
- Adds one process to manage (Litestream sidecar)

### Tier 2b: Warm Standby with Litestream (Postgres-free alternative)

**RPO:** Seconds (continuous WAL streaming). **RTO:** 1-2 minutes.

```
Primary (zone-a)                    Standby (zone-b)
├── config.sqlite ──Litestream──→  ├── config.sqlite (restored, read-only)
├── users/*/     ──Litestream──→   ├── users/*/     (restored, read-only)
└── Server (read-write)            └── Server (warm standby, read-only)
         ↑                                  ↑
    All writes                         Read-only mode
    (auth, config, tasks)              (serves reads during failover prep)
         ↓                                  ↓
                    S3 bucket
                    (WAL stream target)
```

This is the **Postgres-free** path. Litestream replicates ALL SQLite files (config DB + every user replica) to S3. The standby continuously restores from S3 and can serve read-only traffic.

**How it works for metadata:**
- Primary writes to config.sqlite (users, tokens, views, etc.)
- Litestream streams WAL changes to S3 within seconds
- Standby restores continuously from S3 — metadata is seconds behind primary
- Auth reads on the standby see tokens/users with bounded staleness (~1-5s)

**On failover:**
1. Standby stops restoring (becomes the new primary)
2. Server switches from read-only to read-write
3. DNS failover routes all traffic to standby
4. Litestream on new primary starts streaming to S3 (for the next standby)

**Architecture changes needed:**
- Add **read-only mode** to the server (reject writes, serve reads) — small code change
- Deploy Litestream on both primary and standby
- Health check distinguishes primary (read-write) vs standby (read-only)

**Trade-offs:**
- No Postgres dependency — pure SQLite everywhere
- Single replication mechanism for ALL data (config + task replicas)
- Bounded staleness on standby (~1-5s behind primary)
- Auth token revocation has a brief window where revoked tokens work on standby
- Simpler ops than Postgres (no database cluster to manage)
- Litestream is battle-tested for exactly this use case

**When to choose this over Postgres (Tier 3):**
- You want to stay Postgres-free as long as possible
- Your user count is <500 (SQLite handles this comfortably)
- You don't need synchronous replication (bounded staleness is acceptable)
- You want one replication mechanism instead of two

**When to move to Postgres instead:**
- You need strong consistency for auth (zero staleness on token revocation)
- You have >500 concurrent users and need connection pooling
- You need multi-region active-active writes for config data
- You want standard database ops tooling (pg_dump, pg_stat, etc.)

### Tier 3: Warm Standby with Postgres Config

**RPO:** Near-zero (streaming replication). **RTO:** 1-2 minutes.

```
Primary (zone-a)                 Standby (zone-b)
├── Postgres (config) ──stream──→ Postgres replica
├── TaskChampion replicas        ├── Litestream restore
└── Server process               └── Cold server

On failure:
1. Promote Postgres replica
2. Restore TaskChampion replicas from Litestream/S3
3. Start server
4. DNS failover
```

**Architecture changes needed:**
- Implement `PostgresConfigStore` (ConfigStore trait swap)
- Deploy Postgres with streaming replication
- Litestream for TaskChampion SQLite replicas

**Trade-offs:**
- Config data has zero RPO (Postgres synchronous replication)
- Task data has near-zero RPO (Litestream WAL streaming)
- Adds Postgres infrastructure
- Two replication mechanisms to manage (Postgres + Litestream)

### Tier 4: Active-Active with TaskChampion Sync

**RPO:** Near-zero (sync protocol). **RTO:** Zero (automatic failover).

```
Region A                              Region B
├── Server A                          ├── Server B
├── Local TaskChampion replicas       ├── Local TaskChampion replicas
├── Postgres (config) ──stream──→     ├── Postgres replica
│                                     │
└───── TaskChampion Sync Server ──────┘
       (authoritative operation log)
```

Both regions serve traffic. Task data syncs through the TaskChampion sync protocol. Config data replicates through Postgres.

---

## TaskChampion Sync vs Postgres Replication: Analysis

This is the key architectural decision for Tier 3+. Should we use TaskChampion's sync protocol for DR replication, or migrate everything to Postgres and use database-level replication?

### What TaskChampion Sync Gives You

TaskChampion's sync protocol is designed for exactly this problem — keeping multiple copies of a task database in sync. It works by:

1. Each replica maintains a local copy of all tasks
2. Changes are recorded as an **operation log** (not full-state snapshots)
3. Sync sends operations to a central sync server, which distributes them to other replicas
4. Conflicts are resolved deterministically (operations commute — CRDT-like)
5. Protocol runs over HTTP — works across zones, regions, even the public internet

**Advantages over Postgres replication:**

| Aspect | TaskChampion Sync | Postgres Replication |
|--------|-------------------|---------------------|
| **Consistency model** | Multi-writer, eventual consistency (CRDT) | Single-writer, strong consistency (or bounded-staleness if async) |
| **Infrastructure** | One HTTP sync server (lightweight) | Postgres cluster (heavier ops) |
| **Networking** | HTTP over internet (NAT-friendly) | Needs low-latency private network |
| **Conflict resolution** | Built-in (operations merge deterministically) | None — primary/replica model, no multi-writer |
| **CLI compatibility** | `task` CLI syncs natively | Lose CLI sync entirely |
| **Granularity** | Per-user replica sync | All-or-nothing database replication |
| **Offline tolerance** | Replicas work offline, sync when available | Replica must stay connected |
| **Selective sync** | Sync specific users/teams | Replicate everything |
| **Cost** | Minimal (small HTTP server) | Postgres instance per zone |

**Disadvantages:**

| Aspect | TaskChampion Sync | Postgres Replication |
|--------|-------------------|---------------------|
| **Consistency** | Eventual (sync lag) | Strong (synchronous option) |
| **What it covers** | Task data only | Everything (config + tasks) |
| **Maturity** | Newer, less battle-tested | Decades of production use |
| **Monitoring** | Custom metrics needed | Standard pg_stat tools |
| **Failover** | Application-level logic | Automatic (Patroni, etc.) |
| **Backups** | Per-replica SQLite files | pg_dump / pg_basebackup |

### The Hybrid Approach (Recommended)

The strengths of each align with different data types:

```
Config data (users, auth, views)  → Postgres with streaming replication
  - Needs strong consistency (auth decisions)
  - Small dataset, low change rate
  - Standard replication tooling

Task data (per-user replicas)     → TaskChampion sync protocol
  - Can tolerate eventual consistency
  - Large dataset, per-user partitioned
  - Built-in conflict resolution
  - CLI compatibility preserved
  - No need to rewrite storage layer
```

**Why this is better than either alone:**

1. **Postgres-only** means rewriting TaskChampion's storage layer (tasks as rows), losing CLI sync, and over-engineering the task data path. Tasks don't need synchronous replication — eventual consistency is fine for "buy milk" being visible 1s later.

2. **TaskChampion-sync-only** doesn't cover the metadata (config data). You'd still need something for auth/views/contexts. And you'd need to handle the case where Server B doesn't know about a user that was just created on Server A (strong consistency requirement for auth).

3. **Hybrid** lets each layer use the right tool. Postgres handles the small, consistency-critical metadata. TaskChampion sync handles the large, eventually-consistent task data. The CLI keeps working. No storage layer rewrite needed.

**Honest trade-off:** The hybrid approach is "less rewrite and less bespoke sync logic" — NOT "less infrastructure". At Tier 4 you run Postgres + TC sync server + local replicas. But you avoid building a custom task replication protocol, and you get TaskChampion's tested conflict resolution for free.

### Architecture of the Hybrid Approach

```
Region A                                    Region B
┌─────────────────────────┐    ┌─────────────────────────┐
│ cmdock-server     │    │ cmdock-server     │
│ ├── PostgresConfigStore │    │ ├── PostgresConfigStore │
│ │   (auth, views, etc.) │    │ │   (Postgres replica)  │
│ ├── ReplicaManager      │    │ ├── ReplicaManager      │
│ │   (local SQLite)      │    │ │   (local SQLite)      │
│ └── TaskChampion sync   │    │ └── TaskChampion sync   │
│     client              │    │     client              │
└─────────────┬───────────┘    └─────────────┬───────────┘
              │                              │
              ▼                              ▼
        ┌───────────────────────────────────────┐
        │   TaskChampion Sync Server            │
        │   (authoritative operation log)       │
        │   Deployed independently (SQLite/PG)  │
        └───────────────────────────────────────┘
              │
              ▼
        ┌───────────────────────────────────────┐
        │   Postgres (config DB)                │
        │   Primary (zone-a) → Replica (zone-b) │
        └───────────────────────────────────────┘
```

**Failover sequence:**
1. Region A health check fails
2. DNS routes all traffic to Region B
3. Postgres replica promoted to primary (automatic via Patroni)
4. Region B's TaskChampion replicas are already up-to-date (continuous sync)
5. Region B serves traffic immediately — no restore step needed

**RTO:** Seconds (DNS TTL). **RPO:** Near-zero (sync lag, typically <1s).

### Implementation Path

| Step | Effort | Prerequisite | Code changes |
|------|--------|-------------|-------------|
| 1. S3 backups (Tier 1) | Low — ops script | None | None |
| 2. Litestream cold standby (Tier 2) | Low — sidecar deployment | Step 1 | None |
| 2b. Litestream warm standby (Tier 2b) | Low-Medium | Step 2 | Read-only mode flag |
| 3. PostgresConfigStore (Tier 3) | Medium — new trait impl | ConfigStore trait (exists) | ~300 lines |
| 4. TaskChampion sync client (Tier 4) | Medium — integrate TC sync API | TC sync server deployed | ~500 lines |
| 5. Active-active routing (Tier 4) | Medium — DNS + health checks | Steps 3+4 | Infra only |

Steps 1-2 require no code changes. Step 2b needs a small server change (read-only mode). Step 3 uses the existing ConfigStore trait. Step 4 uses TaskChampion's built-in sync API (already in our dependencies). Step 5 is infrastructure.

**Recommended path for a bootstrapped product:** 1 → 2 → 2b → 3 (skip
Postgres until you need it). This gets you to 1-2 minute RTO with no Postgres
dependency and minimal code changes.

The procedural side of those tiers still lives in the Backup and Recovery
Guide. This document only explains which architectural tier you might choose
and why.

---

## Decision Matrix

| Factor | Tier 1 (S3 backup) | Tier 2 (Litestream cold) | Tier 2b (Litestream warm) | Tier 3 (Postgres) | Tier 4 (Hybrid sync) |
|--------|-------------------|------------------------|-------------------------|--------------------|--------------------|
| **RPO** | Hours | Seconds | Seconds | Near-zero | Near-zero |
| **RTO** | 15-30 min | 5-10 min | **1-2 min** | 1-2 min | Seconds |
| **Code changes** | None | None | Read-only mode | PostgresConfigStore | + TC sync client |
| **Infrastructure** | S3 bucket | + Litestream | + Litestream × 2 | + Postgres cluster | + TC sync server |
| **Postgres required** | No | No | **No** | Yes | Yes |
| **Ops complexity** | Low | Low | Low-Medium | Medium | Medium-High |
| **Cost** | ~$1/mo | ~$1/mo | ~$5/mo (standby server) | + Postgres ($20-50/mo) | + sync server |
| **CLI compatibility** | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Multi-region** | ❌ | ❌ | ❌ | ❌ | ✅ |
| **Auth staleness** | N/A | N/A | **1-5s bounded** | Zero (sync primary) | Depends on setup |

---

## Recommendation by Stage

| Stage | DR Tier | Why |
|-------|---------|-----|
| **Dogfooding** | Tier 1 (S3 backups) | Minimal effort, acceptable RPO for personal use |
| **Early adopters** | Tier 2 (Litestream cold) | Near-zero RPO, no code changes, simple ops |
| **Small teams / early revenue** | Tier 2b (Litestream warm) | Fast failover, no Postgres, minimal code changes |
| **Paying customers (SLAs)** | Tier 3 (Postgres config) | Strong consistency for auth, professional SLAs |
| **Larger multi-region deployment** | Tier 4 (Hybrid sync) | Active-active, zero downtime, multi-region |

**Note on the Postgres-free path:** Tier 2b can carry you surprisingly far. If your users are <500 concurrent, bounded auth staleness of 1-5 seconds is acceptable (tokens last much longer than that), and SQLite performance is proven at this scale. Move to Tier 3 when you need strong consistency for real-time token revocation or when ops simplicity of Postgres tooling outweighs the simplicity of no Postgres.

---

## Risks and Gotchas

### Auth token replication lag (security)

If a token is revoked on the primary but the replica hasn't received the update yet, the revoked token still works on the lagging reader. Mitigations:
- **Primary-only reads for auth** — all auth checks go to the Postgres primary (adds cross-region latency)
- **Short-lived access tokens + refresh** — tokens expire in minutes, refresh from primary
- **Explicit cache invalidation** — push invalidation events to all regions

### Backups are still required

Replication faithfully copies bad writes. If a bad migration drops a table, or an operator accidentally deletes user data, the replica will have the same damage. Point-in-time backup recovery is the only protection against:
- Ransomware
- Application bugs that corrupt data
- Operator error
- Bad migrations

### Sync server is a new SPOF

In Tier 4, the TaskChampion sync server becomes a critical dependency. Its own DR story (multi-AZ database, backups, RPO/RTO) must be defined. If the sync server is down during a region failure, task data RPO degrades to "last successful sync".

### DNS failover is slower than "seconds"

Real-world DNS caching means "seconds" RTO is optimistic. Existing client connections may hold stale DNS for minutes. For sub-minute failover, consider:
- Anycast routing (both regions share an IP)
- Load balancer health checks with connection draining
- Client-side retry with backup URL

### Active-active write routing

The Tier 4 diagram shows both regions serving traffic, but config writes must go to the Postgres primary. Options:
- **Single-writer region** — all config mutations route to Region A; Region B is read-only for config
- **Write-through proxy** — Region B forwards config writes to Region A's primary over WAN
- **Multi-primary Postgres** — complex, typically not worth it for this dataset size

For task data, TaskChampion sync handles multi-writer natively — both regions can accept task writes and conflicts are resolved by the operation log.

### Version skew

TaskChampion's sync protocol and database format may change between versions. During rolling upgrades:
- The sync server must be compatible with both old and new clients
- Replicas may need migration (handled by TaskChampion automatically)
- Sync protocol version negotiation should be verified in the upgrade checklist

## Open Questions

1. **TaskChampion sync server deployment:** Run our own, or use the existing `taskchampion-sync-server` crate? The crate supports SQLite and Postgres backends (not S3 — the sync server uses a database, not object storage).
2. **Sync conflict visibility:** Should the server surface sync conflicts to users, or silently resolve them? TaskChampion's CRDT-like operations are designed for silent merge.
3. **Config DB migration timing:** At what customer count does the SQLite→Postgres migration pay for itself in ops simplicity?
4. **Litestream vs custom WAL shipping:** Litestream is battle-tested but adds a dependency. Custom WAL shipping gives more control but more code.
5. **Cross-region latency:** TaskChampion sync over the internet adds ~100-200ms per sync. Is this acceptable for the UX?
6. **Sync server DR:** What RPO/RTO does the sync server itself need? Multi-AZ Postgres backend is likely required.
7. **Region bootstrap:** How does a new region get initial task data? Pre-seed "hot" users vs lazy sync on first request?
8. **Operation log compaction:** How long to retain the sync operation log? Long-offline replicas (CLI users on holiday) need the full history.
