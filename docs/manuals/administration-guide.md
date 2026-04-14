# Administration Guide

## Overview

This guide covers operational concerns for running cmdock-server in production: backup and recovery, failure scenarios, monitoring, maintenance tasks, and runbooks for common support requests.

See also:

- [Documentation Library](index.md)
- [Concepts Guide](concepts-guide.md)
- [Installation and Setup Guide](installation-and-setup-guide.md)
- [Backup and Recovery Guide](backup-and-recovery-guide.md)

## Operator Surface Security

Treat `/admin/*` as an operator surface, keep it off normal public user
ingress, and continue to require the
environment-scoped operator token (`CMDOCK_ADMIN_TOKEN`).

Audit and auth diagnostics may depend on forwarded client-IP headers, so those
headers are only meaningful when the server sits behind trusted ingress.

The runtime now defaults to ignoring forwarded client-IP headers unless
forwarded-header trust is enabled explicitly:

- `[server].trust_forwarded_headers = true`
- `CMDOCK_TRUST_FORWARDED_HEADERS=true`

This is part of the server's operator boundary, not just an ops preference.
The normative rule lives in
[ADR-0007: Trusted Proxy and Forwarded-Header Model](../adr/ADR-0007-trusted-proxy-and-forwarded-header-model.md).
See [Admin Surfaces Reference](../reference/admin-surfaces-reference.md) for
the current surface split and auth model.

For deployed pre-production validation, make sure `/admin/*` coverage is not
skipped silently. The repository-owned verification helpers support fail-fast
admin coverage when an operator token is supplied explicitly in the local
shell. See [Testing Strategy Reference](../reference/testing-strategy-reference.md)
for the verification layers and helper entrypoints.

The server also now exposes a small operator console shell at
`/admin/console`. Treat that route the same way as the rest of the operator
surface:

- keep it on trusted operator ingress only
- do not treat the page shell as sufficient auth by itself
- the browser still needs the environment-scoped operator token for actual
  `/admin/*` API calls
- do not expose that token to end-user clients or normal public user ingress

## Device Provisioning Model

The current sync model has two separate operator concerns:

- `admin sync` manages the user's canonical sync identity.
- `admin device` manages each physical device that is allowed to sync.

For self-hosted operators, the normal flow is:

1. Run `cmdock-server admin sync create <user-id>` once per user.
2. Run `cmdock-server admin device create <user-id> --name "<device>"` once per physical client.
3. Copy the emitted `server_url`, `client_id`, and device secret into that client.
4. For QR/deep-link onboarding, run `cmdock-server admin connect-config create <user-id>` to print a short-lived `cmdock://connect?...` URL and terminal QR.

HTTPS-only note:

- `admin connect-config create` only supports `https://` server URLs
- this is intentional because the QR/deep-link payload carries a short-lived
  credential
- self-hosters running plain HTTP should use `admin device create` and
  manually transcribe the emitted values instead

Normal removal should use `revoke`, not `delete`:

- `revoke` blocks future sync for that device and preserves audit history.
- `unrevoke` restores that same device identity after an operator mistake or temporary disablement.
- `delete` is destructive cleanup and should usually be reserved for stale or duplicate records after the device has already been revoked.

## Data Layout

Understanding what's on disk is the foundation for everything else:

```
data/
├── config.sqlite                   # Config DB: users, tokens, views, contexts, presets
└── users/
    ├── user-abc/                   # User's TaskChampion working directory
    │   ├── taskchampion.sqlite3    # Canonical plaintext replica used by REST/iOS
    │   ├── taskchampion.sqlite3-wal
    │   ├── sync.sqlite             # Shared encrypted TC sync DB for this user
    │   └── sync/
    │       └── <client-id>.sqlite   # Optional legacy/maintenance artifact
    ├── user-def/
    │   ├── taskchampion.sqlite3
    │   └── sync.sqlite
    └── ...
```

**Key properties:**
- Each user's canonical replica is separate from their shared TaskChampion sync DB
- The config DB is shared across all users (auth, views, etc.)
- WAL files are normal — they're SQLite's write-ahead log for crash recovery
- TaskChampion manages its own schema inside the SQLite files — don't modify them directly
- Device delete affects device metadata; it does not remove the shared `sync.sqlite`

## Optional Standalone Admin CLI

`cmdock-admin` is the optional standalone operator CLI for live systems. Use it
when you want a remote operator tool for backup, restore, doctor checks,
device and user administration, and admin webhook management over the server's
admin HTTPS API. For installation and release links, see
[Installation and Setup Guide → Optional Standalone Admin CLI](installation-and-setup-guide.md#optional-standalone-admin-cli).

Typical usage:

```bash
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" doctor
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" webhook list
```

This CLI is optional. `cmdock-server` still ships a local on-host admin CLI for
local or offline break-glass work.

## Self-Hosted Operator Quick Reference

For live remote environments, prefer the optional `cmdock-admin` CLI. Use the
local `cmdock-server admin ...` commands mainly for on-host or break-glass
work.

Normal operator lifecycle:

```bash
# Create a user and save the API token shown once
cmdock-server admin user create --username alice

# Create the canonical sync identity once per user
cmdock-server admin sync create <user-id>

# Add each physical client separately
cmdock-server admin device create <user-id> --name "Alice iPhone" --server-url https://tasks.example.com
cmdock-server admin device create <user-id> --name "Alice MacBook" --server-url https://tasks.example.com

# Emit a short-lived connect URL + QR for app onboarding
cmdock-server admin connect-config create <user-id> --server-url https://tasks.example.com --name "Alice Tasks"

# Review state
cmdock-server admin device list <user-id>

# Normal security action
cmdock-server admin device revoke <user-id> <client-id> -y

# Undo an operator mistake
cmdock-server admin device unrevoke <user-id> <client-id> -y

# Permanent cleanup after revocation
cmdock-server admin device delete <user-id> <client-id> -y
```

Operator guidance:
- Use `admin sync` once per user, not once per device.
- Use `admin device create` for every physical TW/iOS client.
- Use `admin connect-config create` when you want a short-lived QR/deep-link
  onboarding artifact rather than copy/paste credentials.
- If the server is not available over HTTPS, do not use connect-config; fall
  back to manual transcription from `admin device create`.
- Prefer `revoke` for lost or suspicious devices.
- Treat `delete` as cleanup, not as the primary security control.

Connect-config troubleshooting:
- the QR/deep-link flow is considered proven once the emitted short-lived token
  completes its first successful authenticated API request, typically
  `GET /api/me`
- `cmdock-server admin token list <user-id>` now shows `FIRST_USED`,
  `LAST_USED`, and `LAST_IP` for the `connect-config` token
- the server also emits `connect_config_consumes_total{result="first_use"}`
  for aggregate visibility without adding a special-purpose test endpoint

## Webhook Operations

The server now has two webhook surfaces:

- user-scoped webhooks under `/api/webhooks`
- admin/per-server webhooks under `/admin/webhooks`

Use user-scoped webhooks when one authenticated user wants task event delivery
for their own account. Use admin/per-server webhooks when the operator wants
environment-wide delivery for monitoring, automation, or integration hooks.

Typical operator flow:

```bash
cmdock-admin webhook list
cmdock-admin webhook create --url https://hooks.example.com/cmdock \
  --secret "<shared-secret>" \
  --events task.created,task.completed,sync.completed
cmdock-admin webhook deliveries <webhook-id>
cmdock-admin webhook test <webhook-id>
cmdock-admin webhook disable <webhook-id>
cmdock-admin webhook enable <webhook-id>
```

Operational guidance:

- keep webhook targets on trusted HTTPS endpoints
- store webhook secrets in normal secret management, not shell history
- use `webhook test` after changing a target URL, TLS path, or secret
- use delivery history first when debugging failed hooks
- disable a failing webhook before deleting it if you want to preserve the
  record and inspect recent delivery state

## Startup Recovery Assessment

On boot, the server now runs a recovery assessment across configured users
before normal service begins.

Assessment outcomes:

- `Healthy`: user stays online normally.
- `Rebuildable`: user stays online, but operator review is recommended because
  some state is missing and will need logical rebuild from canonical/sync
  metadata.
- `NeedsOperatorAttention`: user is placed offline automatically and the server
  persists `users/<user-id>/.offline`.

Operational implications:

- a manually-offline user remains offline across restart
- startup can newly place broken users offline before any requests are served
- `admin user assess <user-id>` uses the same classification model as startup
- `admin user online <user-id>` should only be used after the operator has
  reviewed the recovered state

---

## Backup Strategy

The detailed backup and restore procedures now live in [backup-and-recovery-guide.md](backup-and-recovery-guide.md). This section keeps the operational summary and cross-links.

### What to back up

| Data | Path | Frequency | Method |
|------|------|-----------|--------|
| **Config DB** | `data/config.sqlite` | Daily + before changes | `cmdock-admin backup` or `sqlite3 .backup` |
| **User replicas** | `data/users/*/` | Daily | `cmdock-admin backup` or `sqlite3 .backup` |
| **Server config** | `config.toml` | On change | Version control |
| **TLS certificates** | Managed by reverse proxy | On renewal | Caddy handles this automatically |

> **Do not copy SQLite files with `cp` while the server is running.** Use
> `cmdock-admin backup`, a storage-level snapshot, or SQLite's `.backup`
> command instead.

### How to back up safely

The normal operator flow is:

1. Run `cmdock-admin backup` to have the server write a timestamped snapshot
   into the configured `backup_dir`.
2. Copy `backup_dir` off-host with your existing tooling such as `rsync`,
   `restic`, `borg`, or S3 sync.
3. Keep `config.toml` and reverse-proxy/TLS material under your usual host
   configuration backup policy.

For advanced environments, storage-level filesystem snapshots and SQLite
`.backup` remain valid techniques, but the supported day-to-day path is the
snapshot contract documented in the
[Backup and Recovery Guide](backup-and-recovery-guide.md).

### Backup automation

For Docker deployments, run `cmdock-admin` from the host or another operator
machine that can reach the published admin HTTPS endpoint:

```bash
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" backup
docker compose cp server:/data/cmdock/backups ./backups
```

For bare metal with scheduled off-host copy:

```bash
#!/bin/bash
# /etc/cron.daily/cmdock-backup
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" backup
rsync -a /data/cmdock/backups/ backup-nas:/srv/cmdock/backups/
rsync -a /opt/cmdock-server/config.toml backup-nas:/srv/cmdock/config/
```

> **Note:** The dedicated [Backup and Recovery Guide](backup-and-recovery-guide.md)
> is the authority for backup scope, restore sequencing, retention, and
> disaster-recovery workflow.

---

## Recovery Scenarios

### Scenario 1: User requests task data restore

**Situation:** User accidentally deleted all their tasks, or their app sync corrupted their data.

**Recovery:**

- the current snapshot contract is full-instance restore, not selective
  per-user snapshot restore
- if the whole instance should be returned to a known snapshot, follow the
  restore flow in the [Backup and Recovery Guide](backup-and-recovery-guide.md)
- if only one user is affected, treat it as a per-user recovery investigation:
  put the user offline, assess recoverability, and decide whether the bridge,
  device re-registration, or a full snapshot restore is the safer path

**Impact:** User loses any tasks created since the backup. Other users are unaffected.

**Prevention:** Daily backups with 30-day retention. Consider offering an "undo delete" feature (soft-delete with grace period) in a future release.

### Scenario 1b: Per-user recovery while the server stays running

**Situation:** Only one user needs recovery, and the operator wants other users to remain online.

**Recovery mindset:**

- do not copy files underneath a live user session and hope for the best
- treat this as a per-user recovery transition

**Recommended operator sequence:**

```bash
# 1. Put the user offline / quarantined
cmdock-server admin user offline <user-id>

# 2. Review the recovery assessment
cmdock-server admin user assess <user-id>

# 3. Repair or rebuild based on the assessment
#    For example: allow the bridge to rebuild sync state, or revoke and
#    re-register a broken device.

# 4. Bring the user back online after validation
cmdock-server admin user online <user-id>
```

**Important notes:**

- the offline marker is persistent and is picked up by the running server; cached state is evicted automatically
- `POST /admin/user/{id}/evict` remains available as an immediate HTTP operator action if you need it during live operations
- `admin user assess <user-id>` reports one of:
  - `Healthy`
  - `Rebuildable`
  - `NeedsOperatorAttention`
- if a device sync DB is missing but canonical state is intact, the device may be rebuildable from canonical state
- if trust or consistency is doubtful, revoke the old device and register a replacement instead
- unaffected users should continue operating normally during this workflow

### Scenario 2: Config DB corruption

**Situation:** The config.sqlite file is corrupted (disk error, power loss during write).

**Symptoms:** Server fails to start, or auth/views/contexts return errors.

**Recovery:**

```bash
# 1. Check if SQLite can recover it
sqlite3 data/config.sqlite "PRAGMA integrity_check;"

# 2. If corrupted, restore from backup
mv data/config.sqlite data/config.sqlite.corrupt
cp /backup/cmdock/YYYYMMDD/config.sqlite data/config.sqlite

# 3. Restart
systemctl restart cmdock-server
```

**Impact:** Any config changes (new users, updated views) since the backup are lost. Task data is unaffected (stored in separate files).

**Prevention:** WAL mode (already enabled) makes corruption very rare. Regular backups are the safety net.

### Scenario 3: User replica corruption

**Situation:** One user's taskchampion.sqlite is corrupted.

**Symptoms:** Requests for that user return 500 errors. Other users work fine.

**Detection:** Check logs for `"Failed to get replica"` or `"Failed to open replica"` errors scoped to one user_id.

**Recovery:** Same as Scenario 1 — restore the individual user's directory from backup.

**Impact:** Only the affected user is impacted. No other users are affected.

### Scenario 4: Disk full

**Situation:** The server can't write to disk.

**Symptoms:** All writes fail with 500 errors. Reads may still work (from cache). SQLite will refuse to commit transactions.

**Detection:**

- `disk_available_bytes{scope="data_dir"}` dropping toward zero
- `disk_available_bytes{scope="backup_dir"}` dropping toward zero before
  backup or restore operations
- `disk_read_only{scope="data_dir"} == 1`
- `disk_metric_collection_errors_total{scope="data_dir"}` or
  `disk_metric_collection_errors_total{scope="backup_dir"}` increasing
- request and replica errors such as:
  - `replica_operation_duration_seconds{result="error"}` increasing
  - `http_requests_total{status=~"5.."}` increasing on write paths
- logs such as `Failed to commit` or `Failed to open replica`

**Recovery:**

```bash
# 1. Check disk usage
df -h /opt/cmdock-server/

# 2. Free space — remove old backups, logs, or WAL files
# WAL files can be large — checkpoint them:
sqlite3 data/config.sqlite "PRAGMA wal_checkpoint(TRUNCATE);"
for db in data/users/*/taskchampion.sqlite; do
    sqlite3 "$db" "PRAGMA wal_checkpoint(TRUNCATE);"
done

# 3. Restart to clear any cached errors
systemctl restart cmdock-server
```

**Prevention:** Monitor server-owned filesystem capacity via Prometheus and
alert on both low absolute free space and low remaining percentage.

### Scenario 5: Server crash / power loss

**Situation:** Server process dies unexpectedly or the machine loses power.

**Recovery:** Just restart. SQLite's WAL mode provides crash recovery automatically — uncommitted transactions are rolled back, committed transactions are preserved.

```bash
systemctl start cmdock-server
```

**Impact:** Any in-flight request at crash time is lost (client gets a timeout/connection reset). No data corruption — SQLite guarantees this with WAL mode.

---

## Degradation Handling

The server includes built-in mechanisms for graceful degradation:

### Request Timeout (30s)

All requests are subject to a 30-second timeout. If a request is still processing after 30s (e.g., queued behind a saturated replica Mutex), the server returns `408 Request Timeout` instead of making the client wait indefinitely.

This prevents cascading failures where slow requests consume all available connections and block healthy requests.

### Performance Degradation Detection

Signs of degradation (from Prometheus metrics):

| Signal | Meaning | Action |
|--------|---------|--------|
| `http_request_duration_seconds{path="/api/tasks"} p95 > 2s` | Replica contention | Check per-user stats, consider splitting hot shared replicas |
| `http_requests_in_flight > 50` sustained | CPU saturation | Scale horizontally or optimise filters |
| `replica_operation_duration_seconds{result="error"}` increasing | SQLite issues | Check disk I/O, WAL size |
| `auth_cache_total{result="miss"}` > 20% | Cache churn | Increase cache size or TTL |

## Admin Endpoints

Admin endpoints provide operational visibility and control. They are protected
by a dedicated operator bearer token configured via `[admin].http_token` or
`CMDOCK_ADMIN_TOKEN`.

Do not reuse an ordinary user API token for `/admin/*`.

Manage that operator token as an environment-scoped secret:

- one token for staging, one token for production, and so on
- inject the environment's token into the server runtime
- inject the same environment's token separately into the trusted control plane
  if it calls `/admin/*`
- provide it explicitly to operator-side tooling and smoke tests when needed
- never hand it to end-user clients

### GET /admin/status — Server diagnostics

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" http://localhost:8080/admin/status
```

The status payload now includes:

- uptime
- cached replica count
- current quarantined-user count
- latest startup recovery summary when the process has already completed boot assessment
- operator-facing process details such as `llm_circuit_breaker`

```json
{
  "status": "ok",
  "uptime_seconds": 86400.5,
  "cached_replicas": 42,
  "auth_cache_size": "LRU/1024",
  "config_db": "ok",
  "llm_circuit_breaker": "closed"
}
```

### GET /admin/user/{id}/stats — Per-user diagnostics

When a user reports slowness, check their specific replica:

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" http://localhost:8080/admin/user/user-abc/stats
```

```json
{
  "user_id": "user-abc",
  "replica_cached": true,
  "task_count": 1247,
  "pending_count": 553,
  "replica_dir_exists": true,
  "replica_dir_size_bytes": 2456789
}
```

**What to look for:**
- `task_count > 5000` — large replica, filters will be slow. Suggest archiving completed tasks.
- `replica_cached: false` — user's connection isn't warm. Next request will be slower.
- `replica_dir_size_bytes > 50MB` — WAL may need checkpointing.

### POST /admin/user/{id}/evict — Evict cached replica

Use this when restoring a user's data from backup:

```bash
# 1. Evict the cached connection
curl -X POST -H "Authorization: Bearer $TOKEN" http://localhost:8080/admin/user/user-abc/evict

# 2. Restore from backup
cp -r /backup/users/user-abc/ data/users/user-abc/

# 3. Next request will open the restored replica automatically
```

No server restart required.

### POST /admin/user/{id}/checkpoint — WAL checkpoint

Force a WAL checkpoint on a specific user's replica:

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" http://localhost:8080/admin/user/user-abc/checkpoint
```

Use when `replica_dir_size_bytes` is large due to WAL growth.

## Monitoring

### Health Check

```bash
curl http://localhost:8080/healthz
# {"status":"ok","pending_tasks":"42"}
```

Use this for load balancer health checks. Cached for 30 seconds — very cheap to poll frequently.

### Prometheus Metrics

Scrape `http://localhost:8080/metrics` every 15-30 seconds.

**Key dashboards to build:**

1. **Request rate and latency** — `http_request_duration_seconds` by path
2. **Error rate** — `http_requests_total` where status >= 400
3. **Active connections** — `http_requests_in_flight`
4. **Replica health** — `replica_cached_count`, `replica_open_duration_seconds`
5. **SQLite contention** — `sqlite_busy_errors_total` (should always be 0)
6. **Auth cache** — `auth_cache_total{result="hit"}` vs `miss`
7. **Disk headroom** — `disk_available_bytes{scope="data_dir"}`,
   `disk_available_bytes{scope="backup_dir"}`,
   `disk_read_only{scope="..."}`,
   `disk_metric_collection_errors_total{scope="..."}`

Recommended low-space alerting:

- page if `disk_available_bytes{scope="data_dir"}` falls below your minimum
  emergency write headroom
- warn earlier on low percentage remaining for `data_dir`
- warn or page separately for `backup_dir`, because backup/restore can fail
  before normal runtime writes fail
- treat `disk_read_only == 1` as an immediate incident
- treat any increase in `disk_metric_collection_errors_total` as a monitoring
  problem that needs investigation

### Recovery Alerting Guidance

Use different severities for different recovery signals.

**Page immediately:**

- any increase in `sqlite_corruption_detected_total`
- unexpected `recovery_transitions_total{action="offline",source="startup",changed="true"}`
- `recovery_quarantined_users > 0` outside a planned maintenance/recovery window
- sustained `quarantine_blocked_total` when clients are actively being rejected

**Warn and investigate, but do not page by default:**

- `recovery_assessments_total{status="rebuildable"}`
- `recovery_startup_users_rebuildable > 0`
- moderate `sqlite_busy_errors_total` growth without corruption or quarantine

**Downgrade or silence during planned operator work:**

- `recovery_quarantined_users`
- `recovery_transitions_total{action="offline"}`
- `quarantine_blocked_total`

This is important during selective restore or controlled offline windows,
because the operator may intentionally trigger those signals.

Recommended operator rule:

- page on unexpected offline/quarantine
- warn on rebuildable/degraded states
- use audit logs to identify the affected user and transition source

### Log Monitoring

The server logs structured output via `tracing`. Key patterns to alert on:

| Log pattern | Meaning | Action |
|-------------|---------|--------|
| `Failed to open replica` | User's SQLite can't be opened | Check file permissions, disk space |
| `Failed to commit` | Write failed | Check disk space, SQLite integrity |
| `SQLite BUSY` | Lock contention | Should not happen with ReplicaManager |
| `Missing Authorization header` | Unauthenticated request | Normal if from scanners; alert if from your app |

---

## Maintenance Tasks

### WAL checkpoint

SQLite WAL files can grow large under sustained writes. Checkpoint them periodically:

```bash
# Run weekly or when WAL files exceed 100MB
for db in data/users/*/taskchampion.sqlite data/config.sqlite; do
    sqlite3 "$db" "PRAGMA wal_checkpoint(TRUNCATE);"
done
```

### Expired token cleanup

Tokens with `expires_at` in the past should be cleaned up:

```bash
sqlite3 data/config.sqlite \
    "DELETE FROM api_tokens WHERE expires_at IS NOT NULL AND expires_at < datetime('now');"
```

### Database integrity check

Run periodically (weekly) to detect corruption early:

```bash
for db in data/config.sqlite data/users/*/taskchampion.sqlite; do
    result=$(sqlite3 "$db" "PRAGMA integrity_check;" 2>&1)
    if [ "$result" != "ok" ]; then
        echo "CORRUPT: $db — $result"
        # Send alert
    fi
done
```

### Log rotation

If using systemd, journal handles rotation. For file-based logs:

```
/var/log/cmdock-server/*.log {
    daily
    rotate 30
    compress
    delaycompress
    missingok
    notifempty
}
```

---

## Server Upgrades

### Upgrade procedure

For live environments, prefer a conservative upgrade sequence:

```bash
# 1. Take a hot backup
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" backup

# 2. Take a cold rollback snapshot before the change
#    (host-level tar/image/config snapshot, or your environment equivalent)

# 3. Deploy the new image or binary

# 4. Verify health and operator status
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" doctor
```

Recommended operator posture:

1. take a fresh hot backup
2. take a cold rollback snapshot before risky live changes
3. deploy one known candidate version
4. verify `healthz`, `/admin/status`, and `cmdock-admin doctor`
5. roll back quickly if the new version is not healthy

The exact deploy mechanics depend on whether you run Docker Compose, systemd,
or another host-level service manager. Keep the deploy step simple and keep the
rollback path explicit.

### Graceful shutdown

The server handles SIGTERM and SIGINT gracefully:
- Stops accepting new connections
- Drains in-flight requests (allows them to complete)
- Shuts down cleanly with log message

systemd sends SIGTERM on `systemctl stop`. The default `TimeoutStopSec=90s` gives plenty of time for drain (our request timeout is 30s, so worst case is 30s drain).

### Zero-downtime upgrades (future)

For operators who eventually need zero-downtime upgrades:

**Option 1: Blue-green with reverse proxy**
```
Caddy/nginx → Server A (old) → drain → stop
            → Server B (new) → start → health check → receive traffic
```

**Option 2: Socket activation (systemd)**
```ini
# systemd socket holds the listening port
# New process inherits the socket — no gap in accepting connections
[Service]
ExecStart=/usr/local/bin/cmdock-server --config ...
FileDescriptorStoreMax=1
```

**Option 3: Multiple instances with sticky routing**
Rolling restart across instances — each one drains and restarts while others serve traffic.

### Schema migration strategy

Migrations run automatically on startup. The server:
1. Reads all `.sql` files from `migrations/` directory
2. Tracks applied migrations in a `_migrations` table
3. Runs unapplied migrations in a transaction (atomic — all or nothing)
4. Only starts serving requests after migrations succeed

**If a migration fails:**
- Server won't start (it exits with an error)
- The automatic rollback in the upgrade script reverts to the previous binary
- Data is unchanged (failed migration rolled back by SQLite transaction)

**Adding new migrations:**
- Create a new numbered SQL file (e.g., `008_add_feature.sql`)
- Migrations are idempotent by convention (use `CREATE TABLE IF NOT EXISTS`, etc.)
- Test by running `--migrate` flag before deploying

For the deeper engineering guidance on safe schema evolution, compatibility
windows, backfills, and live-migration patterns, see
[Schema and Live Migration Reference](../reference/schema-and-live-migration-reference.md).

### Upgrade checklist

- [ ] Read the changelog for breaking changes
- [ ] Take a fresh hot backup
- [ ] Take a cold rollback snapshot before the change
- [ ] Deploy one known candidate image or binary
- [ ] Verify `/healthz` returns OK
- [ ] Check `/admin/status` for expected uptime reset, cache counts, and quarantined-user count
- [ ] Run `cmdock-admin doctor`
- [ ] Monitor Prometheus for error rate spikes in the first 5 minutes
- [ ] If issues: stop the rollout early and restore the previous runtime plus cold snapshot as needed

## Deployment Checklist

### Before first deployment

- [ ] Create `config.toml` from `config.example.toml`
- [ ] Create data directory with appropriate permissions
- [ ] Run migrations: `cmdock-server --config /etc/cmdock-server/config.toml --migrate`
- [ ] Create initial user and API token: `cmdock-server admin user create --username <name>`
- [ ] Set up TLS (Smallstep CA or reverse proxy)
- [ ] Configure systemd service
- [ ] Set up backup automation (`systemd` timer recommended)
- [ ] Configure Prometheus scraping
- [ ] Test with `just smoke`

### systemd service file

```ini
[Unit]
Description=cmdock sync server
After=network.target

[Service]
Type=simple
User=cmdock
ExecStart=/usr/local/bin/cmdock-server --config /etc/cmdock-server/config.toml
Restart=always
RestartSec=5
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

### After deployment

- [ ] Verify `/healthz` returns OK
- [ ] Verify Swagger UI loads at `/swagger-ui/`
- [ ] Verify Prometheus metrics at `/metrics`
- [ ] Run smoke tests: `just smoke http://your-server:8080`
- [ ] Seed initial config data (contexts, views, presets) for iOS app

---

## Capacity Planning

### Storage estimates

| Component | Size per unit | Growth rate |
|-----------|--------------|-------------|
| Config DB | ~100KB base | +1KB per user |
| User replica (empty) | ~50KB | — |
| User replica (100 tasks) | ~200KB | ~2KB per task |
| User replica (1000 tasks) | ~2MB | ~2KB per task |
| WAL file (active writes) | 0-100MB | Checkpointed periodically |

**Example:** 100 users, 500 tasks each = ~100MB total data. Fits easily on any server.

### Resource requirements

Based on measured load test data (~1 MB RSS per concurrent user, multi-threaded Tokio runtime):

| Scale | CPU | Memory | Disk | Throughput |
|-------|-----|--------|------|------------|
| 1-10 users | 1 core | 256MB | 1GB | Overkill |
| 10-100 users | 2 cores | 512MB | 5GB | ~10K tx/s |
| 100-500 users | 4 cores | 2GB | 20GB | ~20K tx/s |

**CPU notes:** The server uses Tokio's multi-threaded runtime (1 async worker per core) plus a blocking thread pool for SQLite I/O. 2 cores is the sweet spot for < 200 users; beyond 4 cores, SQLite's single-writer lock becomes the bottleneck. See `performance-and-scaling-guide.md` for the full threading model.

---

## Disaster Recovery

### RTO/RPO targets

| Tier | RTO (recovery time) | RPO (data loss window) |
|------|--------------------|-----------------------|
| **Personal use** | Hours | 24 hours (daily backup) |
| **Team** | 30 minutes | 1 hour (hourly backup) |
### Recovery procedure

1. **Provision new server** (or fix existing)
2. **Restore data directory** from most recent backup
3. **Restore config.toml** from version control
4. **Start server** — migrations run automatically, no manual steps
5. **Verify** — `/healthz`, smoke tests, user validation
6. **Update DNS** if server address changed

### What's NOT recovered

- In-flight requests at crash time (client retries handle this)
- Tasks created/modified since last backup (RPO window)
- Cached state (auth cache, health cache — rebuilt automatically on first access)
