# Performance and Scaling Guide

**Status:** Validated via load testing (March-April 2026)

## Performance Baseline

Tested on WSL2 (Linux 6.6, `32` visible vCPUs, `47 GiB` RAM), single
process, release build. All tests use isolated temporary environments with no
impact on real data.

## Profile-Based Results After Device Registry / Bridge Refactor

The load harness now supports four explicit profiles:

- `personal-only` — isolated users only
- `mixed` — a handful of isolated users plus one shared team user
- `team-contention` — all VUs share one hot team user/device
- `multi-device-single-user` — one user with many registered devices

### Current Findings (April 2026)

The important result is that the server behaves very differently by profile:

- `personal-only` scales well, even at high VU counts
- `mixed` degrades primarily because the shared team user drives bridge contention
- `team-contention` is intentionally pessimistic and shows the worst shared-replica conflict pattern
- `multi-device-single-user` currently exposes the largest architectural weakness because one user with many active devices causes bridge fan-out pressure
- the harness now narrows scenario mix per profile so small-run probes still
  hit the intended code paths; `multi-device-single-user` now prioritises
  `Mixed`, `SyncWrite`, `SyncRead`, and `BridgeSync` instead of unrelated
  hotspot scenarios

### Personal-Only Ladder (30s, 10 tasks/user)

| Metric | 10 VUs | 20 VUs | 50 VUs | 100 VUs | 200 VUs |
|--------|--------|--------|--------|---------|---------|
| `add-version` ok | 727 | 1,855 | 2,868 | 4,215 | 4,016 |
| `add-version` conflict | 3 | 8 | 16 | 33 | 67 |
| Shared bridge warnings | 0 | 0 | 0 | 0 | 0 |
| Server stability | Stable | Stable | Stable | Stable | Stable |

Interpretation:

- isolated single-user traffic is in good shape
- there is no evidence here of the shared-team meltdown pattern
- the remaining conflict counts are low enough to be protocol noise rather than a core bottleneck

### High-VU Capacity Probe (30s, 10 tasks/user)

These runs were used as a rough EC2 sizing probe for the healthiest production
shape: isolated users only.

| Metric | 50 VUs | 100 VUs | 200 VUs |
|--------|--------|---------|---------|
| Scenarios/s | 4,217 | 2,188 | 430 |
| Avg latency | 10.73 ms | 42.24 ms | 436.79 ms |
| Median latency | 2 ms | 7 ms | 80 ms |
| Peak RSS | 118.8 MB | 318.7 MB | 219.1 MB |
| Peak FDs | 384 | 798 | 1,424 |

Interpretation:

- `50`-`100` VUs on `personal-only` remain comfortable on the current runtime
- `200` VUs stays stable but is no longer a low-latency operating point
- CPU and file-descriptor growth show up before memory becomes the main limit
- use `personal-only` for baseline deployment sizing, not `mixed` or
  `multi-device-single-user`

### Comparison Profiles at 20 VUs (30s, 10 tasks/token)

| Profile | `add-version` ok | `add-version` conflict | Operational shape |
|---------|------------------|------------------------|-------------------|
| `personal-only` | 1,855 | 8 | Healthy baseline |
| `mixed` | 558 | 949 | Shared team user drives bridge warnings |
| `team-contention` | 89 | 1,321 | Deliberately pessimistic hotspot |
| `multi-device-single-user` | 126 | 0 | Severe bridge pressure, but harness auth noise removed and TC pushes no longer fail with bridge-induced `400`s |

### Current Bottleneck Analysis

The bottleneck is no longer device auth or per-device sync storage. It is the
bridge strategy when shared state is hot.

Before the queued-bridge change, REST reads and writes synchronously paid
bridge cost. The current runtime now queues REST-triggered bridge work, which
improves the single-user path substantially, but the profile results still show
that these shapes remain expensive:

- one shared team replica under active write contention
- one user with many active devices

The worst profile is still `multi-device-single-user`, but the latest bridge
cleanup changed the shape of the failure:

- REST reads no longer schedule bridge pulls
- TC reads only reconcile a device when that chain is stale relative to
  canonical state
- canonical replica cold-start is now serialised per user, which removed the
  earlier same-user `Creating table` startup race

That means the next optimisation target remains bridge fan-out policy rather
than raw TaskChampion storage throughput, but the remaining pain is now more
clearly isolated to shared-device reconciliation.

### Latest Follow-Up Probe (April 2026)

After adding the bridge freshness tracker and canonical cold-start
serialisation:

- `mixed` at `20` VUs still completed cleanly with no TC write `500`s, but the
  shared team device continued to produce bridge warnings and heavy `409`
  conflict traffic
- `personal-only` at `200` VUs remained healthy: no shared-device warning
  pattern, no server instability, and `add-version` stayed mostly successful
- `multi-device-single-user` no longer reproduced the earlier canonical
  first-open race, and the harness no longer emits the bogus REST `401`s seen
  before the profile-specific scenario cleanup
- the targeted TC bridge path now degrades to queued reconciliation on
  non-corruption bridge failures, which removed the bridge-induced `400`s from
  the multi-device profile; the remaining issue is convergence pressure and
  warning volume, not protocol breakage
- `multi-device-single-user` should still be treated as an architecture stress
  probe rather than a sizing benchmark

### Recommended Perf Test Matrix

- `personal-only`
  - primary high-VU baseline
  - use this for sizing and general regression tracking
- `mixed`
  - realistic product/UAT profile
  - use this for “normal shared usage” checks
- `team-contention`
  - targeted stress profile
  - usually keep at `<= 20` VUs unless deliberately exploring worst-case behaviour
- `multi-device-single-user`
  - targeted architecture profile
  - use this to evaluate bridge fan-out changes

### REST API Load Test Results

| Metric | 5 users | 20 users | 100 users | 200 users | 500 users |
|--------|---------|----------|-----------|-----------|-----------|
| **Throughput (tx/s)** | 50 | 1,840 | 11,686 | 20,761 | ~20,000 |
| **Total transactions (30s)** | 701 | 60,708 | 385,654 | 685,128 | 643,670 |
| **HTTP 500 errors** | 0 | 0 | 0 | 0 | 0 |
| **SQLite BUSY errors** | 0 | 0 | 0 | 0 | 0 |
| **Auth DB queries** | — | 47,321 | 11 | 42 | 42 |
| **Replica opens** | 672 | 18,781 | 11 | 21 | 21 |

### Latency at 200 Users (20 personal + 1 shared team replica)

| Endpoint type | Median | Average | Max |
|--------------|--------|---------|-----|
| Read (list tasks, views, config) | **1ms** | 20ms | 6.4s |
| Write (add, complete, delete) | **99ms** | 724ms | 10.9s |
| Mixed (add → list → modify → delete) | **110ms** | 736ms | 16.8s |
| Contention (hot task modify) | **<1ms** | 5ms | 4.1s |

### Sync Protocol Load Test Results (20 VUs, 30s)

All users have both REST and sync capabilities. Sync scenarios (SyncWrite, SyncRead, SyncMixed) run alongside REST scenarios in the same test.

| Metric | Value |
|--------|-------|
| **add-version (ok)** | 2,217 |
| **add-version (conflict)** | 48,615 (96% — expected with shared replicas) |
| **get-child-version** | 45,019 |
| **add-snapshot** | 638 |
| **get-snapshot** | 638 |
| **Snapshot urgency (high)** | 442 |
| **Peak sync storage in-flight** | 4 |

The high conflict rate on add-version is expected: multiple VUs sharing the team replica race to append versions. Each conflict returns 409 with the correct parent version, allowing the client to retry — this matches the TaskChampion sync protocol's design.

### Memory Profile (measured via load test resource monitor)

The load test script samples server RSS, virtual memory, file descriptors, and disk usage every 2 seconds during the run.

| Metric | 5 VUs / 10s | 20 VUs / 30s |
|--------|-------------|--------------|
| **Baseline RSS** | 15.5 MB | 15.5 MB |
| **Peak RSS** | 19.4 MB | 35.8 MB |
| **RSS growth** | 3.9 MB | 20.3 MB |
| **Per-VU estimate** | ~0.78 MB | ~1.0 MB |
| **Peak FDs** | 33 | 75 |
| **Disk growth** | 13.3 MB | 14.2 MB |

The harness also emits a machine-readable summary file for local analysis and
benchmark comparison:

- `load-test-summary.json` by default
- or a caller-provided path via `--summary-json`

### Effect of Replica Size on Performance (20 VUs, storage-focused ladder)

Per-VU memory is stable regardless of how many tasks users have — TaskChampion streams from SQLite rather than loading all tasks into memory.

The latest storage-focused ladder used `personal-only`, `20` VUs, `15s`
duration, and larger seed counts so disk growth is visible.

| Metric | 100 tasks/user | 500 tasks/user | 1000 tasks/user |
|--------|----------------|----------------|-----------------|
| **Seed time** | 3s | 37s | 139s |
| **Baseline RSS** | 44.5 MB | 90.9 MB | 160.9 MB |
| **Peak RSS** | 63.7 MB | 109.5 MB | 186.7 MB |
| **Per-VU estimate** | ~0.96 MB | ~0.93 MB | ~1.29 MB |
| **Peak FDs** | 167 | 181 | 198 |
| **Scenarios/s** | 3,469 | 1,803 | 1,809 |
| **Avg latency** | 4.81 ms | 9.72 ms | 9.46 ms |
| **Median latency** | 1 ms | 3 ms | 6 ms |
| **Final disk** | 122.3 MB | 137.3 MB | 169.2 MB |
| **Approx. total disk per user** | 6.1 MB | 6.9 MB | 8.5 MB |
| **GET /api/tasks count** | 18,603 | 5,011 | 2,544 |

**Key observations:**
- **Per-VU memory stays close to ~1 MB** even as replica size grows
- **Seed time becomes the main cost** once replicas reach hundreds of tasks
- **Read throughput falls as replicas grow**, which is expected for unfiltered
  list-task scans
- **Raw on-disk user footprint is still modest** in this harness, reaching
  roughly `8.5 MB/user` at `1000` tasks/user

The disk figures above come from isolated temporary test environments and
should be treated as raw active-data measurements, not direct EBS sizing
targets. They include the harness environment's config DB plus per-user data,
and they exclude operational headroom such as snapshots, backup retention,
container layers, and log growth.

### Practical Compute and Disk Starting Points

Use `personal-only` as the baseline sizing shape. Shared hot-user profiles are
useful stress probes, but they are a poor way to pick initial deployment size.

### Registered Users vs Peak VUs

A Goose VU is closer to an actively doing-work user session than a registered
account. Do not size CPU/RAM and disk from the same multiplier.

- **Compute sizing:** driven by peak concurrent active users (`VUs`)
- **Disk sizing:** driven by total registered accounts and their stored data

Until real production telemetry exists, use these planning assumptions:

- **Expected peak active ratio:** about `0.5%`-`2%` of registered users
- **Recommended first-pass planning point:** `1%`
- **Conservative launch planning point:** `2%`

| Registered users | `0.5%` peak VUs | `1%` peak VUs | `2%` peak VUs |
|------------------|-----------------|---------------|---------------|
| `1,000` | `5` | `10` | `20` |
| `5,000` | `25` | `50` | `100` |
| `10,000` | `50` | `100` | `200` |
| `25,000` | `125` | `250` | `500` |

Use `1%` as the default first estimate for deployment sizing. Use `2%` if launch
traffic is likely to cluster into work-hour peaks, many users will keep
multiple devices active, or you want extra early headroom.

**Compute**

| Expected active shape | Suggested starting point | Notes |
|-----------------------|--------------------------|-------|
| Light pre-production environment | `2 vCPU / 4 GiB` | Good for smaller environments and smoke traffic |
| First production deployment | `4 vCPU / 4 GiB` | Reasonable starting point for mostly isolated-user traffic |
| Extra headroom / uncertain launch | `4 vCPU / 8 GiB` | Safer initial buffer before real telemetry exists |

Do not use burstable `t*` classes for steady production deployments. Sustained sync and
SQLite write traffic make fixed-performance compute a better fit.

**Storage**

For planning, use:

- **Raw active-data expectation:** about `8-10 MB/user` for a user with around
  `1000` tasks
- **Practical provisioned budget:** about `30-50 MB/user` once WAL/SHM,
  snapshots, backups, logs, and maintenance slack are included

That means the simplest first-pass planning model is:

- **CPU/RAM:** size to peak VUs
- **Disk:** size to registered users
- **Default conservative assumptions:** `1%` peak VUs and `40 MB` provisioned
  disk per registered user

| User count | Practical storage budget |
|-----------------|--------------------------|
| `10,000` | `300-500 GB` |
| `25,000` | `750 GB-1.25 TB` |
| `50,000` | `1.5-2.5 TB` |

### Small Self-Hoster Baseline

For a small self-hosted deployment, the current small-scale benchmark envelope
is:

- `5` users
- `30s` mixed-shape load at low scale
- startup ready within `4s`
- peak server RSS within `96 MB`
- peak file descriptors within `128`
- data-dir growth within `32 MB`
- zero unexpected `5xx`

That budget describes the server-process envelope, not the whole host budget.
For a practical small self-hosted starting point:

- **CPU:** `2 vCPU`
- **RAM:** `2 GiB`
- **Disk:** `10 GB` if the host is dedicated to the server and local data is
  expected to stay modest
- **Disk with local backup headroom:** `20 GB` if backups, restore staging, and
  safety snapshots are kept on the same host

The runtime process itself is small at this scale; the larger disk
recommendation is mostly operational headroom for WAL/SHM files, backups,
restore staging, logs, and maintenance slack rather than live task data alone.

Memory samples are saved to `load-test-memory.csv` after each run for analysis.

### Key Optimisations Applied

| Optimisation | Impact |
|-------------|--------|
| **ReplicaManager** (DashMap connection cache) | 32K opens → 21 opens per test |
| **Auth token cache** (LRU, 30s TTL) | 88K DB queries → 42 per test |
| **Parse filter once** (eval per task) | Eliminated O(tasks) × parse_cost |
| **Zero-allocation filter eval** | `eq_ignore_ascii_case`, pre-parsed dates, threaded `now` |
| **Drop lock before filter/map** | Reads don't block writers during CPU work |
| **Healthz cache** (30s TTL) | Sub-microsecond health checks |
| **task_to_item + urgency_for_task** | Urgency computed via dedicated helper; cheap TC lookups read twice for clarity over micro-optimisation |

---

## Threading Model

The server is **multi-threaded** and benefits from multiple CPU cores. Three thread pools handle different workloads:

| Pool | Default size | What it does |
|------|-------------|-------------|
| **Tokio async workers** | 1 per CPU core | HTTP accept/route, auth cache lookups, request/response I/O |
| **`spawn_blocking` pool** | Up to 512 threads | All sync protocol SQLite I/O, admin handlers — prevents blocking async workers |
| **tokio-rusqlite** | 1 thread per connection | Config DB queries (auth lookups, views, stores) |

`#[tokio::main]` uses the default multi-threaded runtime (not `current_thread`). This means:

- **2 cores** is the sweet spot for < 200 users. While `spawn_blocking` prevents direct blocking of async workers, a single core must share between async I/O and the blocking pool.
- **4 cores** is sufficient for up to 500 users. Beyond that, SQLite's single-writer lock becomes the bottleneck before CPU saturates.
- The `spawn_blocking` pool grows on demand — `sync_storage_in_flight` tracks concurrent sync ops, `sync_storage_cached_count` tracks cached connections.

---

## Replica Model and Contention

### Personal Replicas (1:1)

Each user gets their own TaskChampion SQLite database at `data/users/{user_id}/`. Personal replicas have **zero contention** — only one user accesses them. In production, this covers:

- Personal task management (iOS app + CLI)
- Single-user deployments
- The majority of traffic in a multi-user system

### Shared Replicas (1:N)

Teams share a single replica. Multiple users read and write to the same SQLite file. The `ReplicaManager` puts each replica behind a `tokio::sync::Mutex`, which serialises access per replica (matching SQLite's single-writer model).

**Realistic contention profiles:**

| Scenario | Users per replica | Expected behaviour |
|----------|------------------|--------------------|
| Family sharing | 2-4 | No contention — requests rarely overlap |
| Small team | 5-15 | Minimal queueing, sub-100ms writes |
| Department | 10-30 | Moderate queueing, 100-200ms median writes |
| Stress test (500 VUs on 1 replica) | 480 | 110ms median writes, 30s tail — Mutex saturated |

**When to split hot shared replicas:** If a shared replica consistently shows
>200ms median write latency, split it into multiple replicas by project or
sub-team.

---

## Scaling Stages

### Single-Process Runtime Model

```
iOS App / CLI → cmdock-server (single process)
                    ├── Config DB (SQLite)
                    ├── Personal replicas (SQLite per user)
                    └── Shared replicas (SQLite per team)
```

**Scale:** 1-100 users, 20K tx/s on modest hardware.

**When it stops working:** CPU saturation (~20K tx/s on WSL2), or more users than a single server's disk can hold replicas for.

### Optional Postgres Config DB

```
iOS App / CLI → cmdock-server
                    ├── Config DB (Postgres — shared) ← swap via ConfigStore trait
                    ├── Personal replicas (SQLite per user)
                    └── Shared replicas (SQLite per team)
```

The `ConfigStore` trait was designed for this migration. Implement `PostgresConfigStore`, change the connection string in `config.toml`, no handler changes needed.

**What you gain:**
- Shared auth across multiple server instances
- Proper backup/restore for config data
- Connection pooling via `sqlx`
- No `libsqlite3-sys` conflict (Postgres driver doesn't link SQLite)

**Scale:** 100-500 users, multiple server instances possible with sticky routing.

## Observability

### Prometheus Metrics

The server exposes metrics at `GET /metrics` in Prometheus exposition format. Key metrics for performance monitoring:

| Metric | Type | What to watch for |
|--------|------|-------------------|
| `http_request_duration_seconds{method,path}` | histogram | p95 > 1s on read paths |
| `http_requests_in_flight` | gauge | Sustained > 50 suggests saturation |
| `replica_operation_duration_seconds{operation,result}` | histogram | p95 > 500ms on writes |
| `replica_open_duration_seconds` | histogram | Should be <50ms; >100ms means disk issues |
| `replica_cached_count` | gauge | Memory growth tracking |
| `disk_available_bytes{scope}` | gauge | Low headroom on `data_dir` or `backup_dir` means runtime or backup pressure |
| `disk_read_only{scope}` | gauge | `1` means the underlying filesystem is read-only |
| `disk_metric_collection_errors_total{scope}` | counter | The disk-capacity signal itself is failing |
| `sqlite_busy_errors_total{operation}` | counter | Any non-zero means contention |
| `sqlite_busy_retries_total{operation}` | counter | Retry storms indicate overload |
| `auth_cache_total{result}` | counter | hit/(hit+miss) should be >95% |
| `config_db_query_duration_seconds{operation}` | histogram | p95 > 10ms means DB contention |
| `filter_evaluation_duration_seconds` | histogram | > 100ms means large task sets |
| `outbound_http_requests_total{target,result}` | counter | Track success vs transport/HTTP failures for runtime egress |
| `outbound_http_request_duration_seconds{target,result}` | histogram | Watch provider latency and timeout tail |
| `outbound_http_failures_total{target,class}` | counter | Connectivity breakdown for timeout/connect/http_4xx/http_5xx/decode |
| `sync_operations_total{operation,result}` | counter | Track ok/conflict/error rates per sync endpoint |
| `sync_operation_duration_seconds{operation,result}` | histogram | p95 > 100ms on add-version means disk I/O issues |
| `sync_storage_in_flight` | gauge | Concurrent sync operations in progress (not connections — see `sync_storage_cached_count`) |
| `sync_storage_cached_count` | gauge | Number of cached sync storage connections (for memory sizing) |
| `sync_conflicts_total` | counter | Expected under contention; sustained increase may indicate client retry storms |
| `sync_body_size_bytes{operation}` | histogram | Monitor payload sizes for capacity planning |
| `sync_snapshot_urgency_total{level}` | counter | High urgency means clients need to send snapshots |

### Alerting Thresholds (suggested)

| Condition | Severity | Action |
|-----------|----------|--------|
| `http_request_duration_seconds{path="/api/tasks"} p95 > 2s` | Warning | Check replica contention |
| `sqlite_busy_errors_total` increasing | Warning | Consider splitting hot shared replicas or reducing contention |
| `http_requests_in_flight > 100` sustained | Critical | CPU saturated, scale horizontally |
| `replica_open_duration_seconds p95 > 1s` | Critical | Disk I/O issue |
| `disk_available_bytes{scope="data_dir"}` below minimum write headroom | Critical | Runtime writes are at risk |
| `disk_available_bytes{scope="backup_dir"}` below planned staging headroom | Warning | Backup/restore can fail before runtime does |
| `auth_cache_total{result="miss"} / total > 20%` | Warning | Increase cache TTL or size |
| `outbound_http_failures_total{target="anthropic",class=~"timeout|connect"}` increasing | Warning | Check provider reachability, DNS, TLS, or egress path |

---

## Load Testing

### Running Load Tests

```bash
# Quick mixed test
just load-test 5 10s 5 10 mixed

# Isolated-user baseline
just load-test 20 30s 20 10 personal-only

# Realistic product/UAT blend
just load-test 20 30s 5 10 mixed

# Shared-team hotspot stress
just load-test 20 30s 5 10 team-contention

# One user, many devices
just load-test 20 30s 5 10 multi-device-single-user

# High-VU isolated-user baseline
just load-test 200 30s 200 10 personal-only
```

### Test Scenarios

| Scenario | Weight | What it does |
|----------|--------|-------------|
| **ReadHeavy** | 7 | List tasks, get views, get app-config, healthz |
| **WriteHeavy** | 3 | Full lifecycle: add → complete → delete |
| **Mixed** | 4 | Add → list → modify → list → delete |
| **Contention** | 2 | Multiple VUs modify the same "hot task" |
| **SyncWrite** | 3 | add-version to build version chain (simulates `task sync` push) |
| **SyncRead** | 2 | get-child-version to traverse chain (simulates `task sync` pull) |
| **SyncMixed** | 3 | add versions + read back + occasional snapshot |

All tokens have both REST (bearer token) and sync (X-Client-Id) credentials, so any VU can run any scenario. What changes by profile is how those tokens are assigned:

- `personal-only`: one isolated user/device per VU
- `mixed`: a small set of isolated users plus one shared team user/device
- `team-contention`: every VU shares one user/device
- `multi-device-single-user`: every VU is a different device for the same user

### Interpreting Results

- **Use `personal-only` as the baseline.** That profile now reflects the healthiest deployment shape.
- **Use `mixed` for realistic product behaviour.** If this profile regresses while `personal-only` stays healthy, the bridge/shared-state path is the likely culprit.
- **Treat `team-contention` as intentionally pessimistic.** It is useful for hotspot analysis, not everyday capacity planning.
- **Use `multi-device-single-user` to evaluate bridge fan-out changes.** If this profile regresses, the problem is likely per-user device reconciliation cost.
- **Write median > 200ms** on a shared replica means the Mutex or bridge queue is backing up. Consider splitting hot shared replicas or reducing synchronous bridge work.
- **BUSY errors > 0** means the application-level serialisation isn't containing contention well enough.
- **Auth queries > 100** per test means the cache isn't working (check TTL and cache size).
- **Replica opens > user count** means the ReplicaManager cache is being evicted or not working.
- **Sync conflict rate > 90%** on shared replicas is normal — multiple VUs race to append versions. The protocol handles this via retry.
- **`sync_storage_in_flight` sustained > 10** means the blocking thread pool is under pressure from concurrent sync connections.
- **Memory samples** in `load-test-memory.csv` show the RSS curve over time — useful for spotting leaks (steady growth) vs. stable plateaus.

### Resource Monitor

The load test script includes a background memory sampler that captures server metrics every 2 seconds. After the test, it prints a resource usage report with:

- Baseline / peak / final RSS and virtual memory
- Per-VU memory estimate (for prod VM sizing)
- Peak file descriptors and sync connections in-flight
- Disk usage growth during the test

---

## Architecture Decisions for Scale

### Why SQLite per user, not Postgres for everything?

1. **TaskChampion compatibility** — the `task` CLI syncs via TaskChampion's SQLite format. Postgres would break CLI compatibility.
2. **Zero contention for personal use** — each user's data is completely isolated.
3. **Simple deployment** — single binary, no separate database server required for small deployments.
4. **Natural partitioning** — user data is already separated by design.

### Why the ConfigStore trait?

The config database (auth, views, contexts, presets) is intentionally behind an abstract trait:

```rust
#[async_trait]
pub trait ConfigStore: Send + Sync + 'static {
    async fn get_user_by_token(&self, token: &str) -> Result<Option<UserRecord>>;
    async fn list_views(&self, user_id: &str) -> Result<Vec<ViewRecord>>;
    // ... 17 methods total
}
```

Currently implemented as `SqliteConfigStore`. When you need Postgres, implement `PostgresConfigStore` — handlers don't change.

### Why a per-user Mutex, not a read-write lock?

SQLite allows concurrent readers (WAL mode) but only one writer. A `RwLock` would allow concurrent reads, but TaskChampion's `Replica` API takes `&mut self` for all operations (including reads like `all_tasks()`). The Mutex matches the API contract. The performance impact is mitigated by dropping the lock before CPU-intensive work (filter evaluation, task serialisation).
