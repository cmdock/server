# Backup and Recovery Guide

This guide covers backup, restore, and validation for `cmdock-server`.

It is focused on operator procedure:

- what must be backed up
- how to take safe backups
- how to restore
- how to verify the result

For runtime architecture and terminology, see the [Concepts Guide](concepts-guide.md). For broader operational runbooks, see the [Administration Guide](administration-guide.md). For disaster recovery strategy, see [Disaster Recovery Architecture](../reference/disaster-recovery-reference.md). For the full manual set, see [Documentation Library](index.md).

## 1. Choose A Backup Path

There are two main backup paths, and they solve different problems.

### 1.1 Hot Backups

Hot backups are taken while the server is running.

Use them for:

- routine scheduled protection
- normal restore rehearsals
- standard self-hoster backup policy

The supported hot-backup path is the built-in snapshot workflow:

- `cmdock-admin backup`
- `cmdock-admin backup --include-secrets`
- `cmdock-admin backup restore <timestamp>`

### 1.2 Cold Backups

Cold backups are taken with the service stopped.

Use them for:

- belt-and-braces rollback before risky upgrades
- first live rollout of a new build
- storage migrations or manual file repair
- situations where you want a full host/runtime rollback point, not just a logical snapshot

A cold backup usually means archiving:

- the live `data/` directory
- `config.toml`
- deployed compose/runtime files
- reverse proxy config and certificates if full host rebuild convenience matters

### 1.3 Which One Should You Use?

Recommended default:

- hot backups for routine scheduled protection
- cold backups before risky changes or live rollouts

If you want the safest posture:

- do both

Hot backups are the normal recovery contract. Cold backups are the safest pre-change rollback tool.

## 2. What Must Be Backed Up

You need all of the following:

- `data/config.sqlite`
- `data/config.sqlite-wal` and `data/config.sqlite-shm` if present
- `data/users/<user-id>/taskchampion.sqlite3`
- `data/users/<user-id>/taskchampion.sqlite3-wal` and `-shm` if present
- `data/users/<user-id>/sync.sqlite`
- matching `-wal` and `-shm` files for the shared sync DB if present
- optional legacy `data/users/<user-id>/sync/<client-id>.sqlite` files if they exist in your environment
- your `config.toml`
- reverse proxy configuration and certificates if you want full host rebuild convenience

Minimum logical backup set:

```text
data/
├── config.sqlite*
└── users/
    └── <user-id>/
        ├── taskchampion.sqlite3*
        └── sync.sqlite*
```

If you lose `config.sqlite`, task files may still exist on disk but you lose the metadata that makes the server usable:

- user accounts
- API tokens
- views and config
- device registry
- canonical sync identity metadata

If you lose `users/`, you lose the user’s task data.

## 2.1 Can The Sync DB Be Reconstructed From Canonical State?

Yes, in a limited but useful sense.

If you still have:

- the canonical user replica
- the config database
- the user’s canonical sync identity
- the device registry row for that device
- the server master key

then the server can usually reconstruct a *working* shared TaskChampion sync DB
for that user by syncing canonical state back into a fresh `sync.sqlite`.

What this means in practice:

- the device can become functional again
- the current logical task state can be pushed back out through the sync surface
- devices can resume TaskChampion sync without rotating credentials

What this does **not** mean:

- you get the exact original sync DB back
- you preserve the exact old version graph, snapshot history, or byte-for-byte contents
- you preserve any old transport history exactly as it was

So reconstruction is best understood as:

- logical recovery of a usable sync DB

not:

- exact restoration of the original sync database

## 2.2 Why This Is Still Not a Substitute for Backing Up Device Sync DBs

Even though the sync DB can often be rebuilt from canonical state, the server should still back it up.

Reasons:

- restore is faster and simpler if the files already exist
- you preserve continuity of protocol-facing sync state
- you avoid relying on post-restore rebuild work
- you preserve more of the transport history
- you reduce surprise during recovery

So the recommended policy remains:

- back up the canonical replica
- back up `sync.sqlite`
- treat reconstruction as a fallback recovery path, not the primary design point

## 3. Hot Backup Principles

The supported backup model is:

1. `cmdock-server` writes consistent snapshots into a local `backup_dir`
2. the self-hoster copies that staging directory off-host with their own tooling
3. restores read those snapshots back from the same local staging directory

This keeps backup transport out of the server itself.

The server is responsible for:

- checkpointing SQLite databases
- copying snapshot contents into a timestamped staging directory
- computing checksums
- writing `manifest.json` last
- ignoring incomplete snapshots that never wrote a manifest
- pruning old snapshots according to retention

The self-hoster is responsible for:

- copying `backup_dir` to NAS, S3, restic, Borg, rsync, or similar
- backing up `config.toml`
- backing up reverse proxy and TLS material if full host rebuild convenience matters

Do not use plain `cp` against live SQLite databases while the server is still writing to them.

The normal remote operator path for this workflow is the optional standalone
`cmdock-admin` CLI. For installation and release links, see
[Installation and Setup Guide → Optional Standalone Admin CLI](installation-and-setup-guide.md#optional-standalone-admin-cli).

This CLI is optional. `cmdock-server` still ships a local on-host admin CLI for
local or offline break-glass work.

## 4. Hot Backup Methods

### 4.1 Configure A Backup Staging Directory

Example:

```toml
backup_dir = "/data/cmdock/backups"
backup_retention_count = 7
```

`backup_retention_count = 0` disables automatic pruning.

### 4.2 Quick Start For The Standard Docker Compose Bundle

If you are using the published GHCR image with the standard `deploy/`
Docker Compose bundle, start with:

1. set `backup_dir = "/app/data/backups"` in `config.toml`
2. restart the stack after that config change
3. run one backup and copy the snapshots off-host

Example:

```bash
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" backup
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" backup list
docker compose cp server:/app/data/backups ./backups
```

This is the simplest supported self-hoster path for the standard Docker bundle.

### 4.3 Create Snapshots Through The Admin CLI

The standalone admin CLI now drives the supported backup flow:

```bash
cmdock-admin backup
cmdock-admin backup --include-secrets
cmdock-admin backup list
```

What happens on `cmdock-admin backup`:

1. The CLI calls `POST /admin/backup`
2. The server checkpoints `config.sqlite` and user SQLite databases
3. The server writes a timestamped snapshot into `backup_dir`
4. The server writes `manifest.json` last
5. The server prunes old snapshots if retention is enabled

What `--include-secrets` changes:

- includes the current operator token in the manifest
- leaves infrastructure-managed secrets such as TLS certificates out of scope
- allows a full operator-surface recovery when restoring onto another server

What `cmdock-admin backup list` shows:

- timestamp
- server version
- user count
- total snapshot size
- whether secrets were included
- whether the snapshot is a normal backup or an automatic `pre-restore-*` safety snapshot

The CLI runs outside the server process. In Docker deployments, run
`cmdock-admin` from the host or another operator machine that can reach the
published admin HTTPS endpoint.

### 4.4 Copy The Staging Directory Off-Host

After the server has produced a snapshot in `backup_dir`, copy it off-host with the tooling you already trust.

Examples:

```bash
rsync -a /data/cmdock/backups/ backup-nas:/srv/cmdock/backups/
```

```bash
restic backup /data/cmdock/backups /opt/cmdock-server/config.toml
```

### 4.5 Automate Snapshot Creation With `systemd`

For Linux self-hosters, `systemd` is the clearest default automation path for
scheduled backups because it keeps scheduling, logs, and manual reruns in one
place.

Example unit:

```ini
# /etc/systemd/system/cmdock-backup.service
[Unit]
Description=cmdock snapshot backup
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
EnvironmentFile=/etc/cmdock/cmdock-admin.env
ExecStart=/usr/local/bin/cmdock-admin --server https://tasks.example.com --token ${CMDOCK_ADMIN_TOKEN} backup
```

Example timer:

```ini
# /etc/systemd/system/cmdock-backup.timer
[Unit]
Description=Run cmdock snapshot backup daily

[Timer]
OnCalendar=*-*-* 02:00:00
Persistent=true

[Install]
WantedBy=timers.target
```

Example environment file:

```bash
# /etc/cmdock/cmdock-admin.env
CMDOCK_ADMIN_TOKEN=replace-with-a-long-random-operator-token
```

Enable it with:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now cmdock-backup.timer
systemctl list-timers cmdock-backup.timer
```

Run one backup immediately with:

```bash
sudo systemctl start cmdock-backup.service
journalctl -u cmdock-backup.service -n 50
```

Keep off-host copy as a separate follow-on step. Do not bundle `rsync`,
`restic`, or site-specific notification logic into the same unit unless you
deliberately want one environment-specific wrapper around both steps.

### 4.6 Filesystem Snapshot

If the host runs on ZFS, Btrfs, LVM, or similar snapshot-capable storage, taking a host-level snapshot that includes both `data/` and `backup_dir` is still a clean option.

## 5. Cold Backup Procedure

Cold backups are the rollback-oriented path for risky changes.

Typical flow:

1. stop the server cleanly
2. archive the live `data/` directory
3. archive `config.toml`
4. archive deployed compose/runtime files
5. optionally archive reverse proxy config and TLS material
6. verify the archive exists and can be listed before making changes

For internal staging or dogfood style rollouts, use the dedicated cold snapshot helper and rollout checklist. For self-hosters, the equivalent can be a `tar`, filesystem snapshot, VM snapshot, or image-level backup taken with the service stopped.

Cold backups are not a replacement for routine hot backups. They are the safer rollback tool before a live change.

## 6. Using the Admin CLI

Typical operator flow:

```bash
cmdock-admin backup
cmdock-admin backup list
cmdock-admin backup restore 2026-04-06T10-00-00
```

Operator guidance:

- use normal backups for scheduled snapshot creation
- use `--include-secrets` when you want the snapshot to carry the operator token as well
- copy `backup_dir` off-host separately; the server does not upload archives
- treat `pre-restore-*` snapshots as safety nets created by the restore path
- run `cmdock-admin doctor` after restore

## 7. Suggested Backup Scope

### 7.1 Daily

- full `config.sqlite`
- full `users/`
- `config.toml`

### 7.2 Before Risky Changes

Take a fresh backup before:

- server upgrades
- migration work
- restore rehearsals
- bulk admin actions
- storage moves
- manual file repair

For higher-risk changes on a live system, prefer both:

- a fresh hot backup
- a cold backup taken with the service stopped

### 7.3 Retention

A reasonable self-hosted starting point:

- daily backups for 30 days
- weekly backups for 8-12 weeks
- monthly backups for longer retention if desired

## 8. Example Procedures

### 8.1 Bare Metal Snapshot + Off-Host Copy

```bash
cmdock-admin backup
rsync -a /data/cmdock/backups/ backup-nas:/srv/cmdock/backups/
rsync -a /opt/cmdock-server/config.toml backup-nas:/srv/cmdock/config/
```

### 8.2 Bare Metal With Secrets Included

```bash
cmdock-admin backup --include-secrets
restic backup /data/cmdock/backups /opt/cmdock-server/config.toml
```

### 8.3 Docker

```bash
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" backup
docker compose cp server:/data/cmdock/backups ./backups
```

### 8.4 S3

```bash
cmdock-admin backup
aws s3 sync /data/cmdock/backups s3://my-cmdock-backups/backups/
aws s3 cp /opt/cmdock-server/config.toml s3://my-cmdock-backups/config/config.toml
```

### 8.5 Cold Backup Before A Risky Deploy

Example:

```bash
sudo systemctl stop cmdock-server
tar -czf cmdock-cold-backup-$(date +%F-%H%M%S).tar.gz \
  /var/lib/cmdock/data \
  /etc/cmdock/config.toml
tar -tzf cmdock-cold-backup-$(date +%F-%H%M%S).tar.gz | head
sudo systemctl start cmdock-server
```

Adjust the paths for your own install layout.

## 9. Restore Procedure

Snapshot restore is currently a whole-instance operation.

The supported operator flow is:

```bash
cmdock-admin backup restore <timestamp>
```

What the server does during restore:

1. Reads the chosen snapshot from `backup_dir`
2. Validates `manifest.json`
3. Verifies version compatibility and schema compatibility
4. Verifies checksums for all snapshot files
5. Creates an automatic `pre-restore-<timestamp>` safety snapshot of the current live state
6. Quarantines all users while the restore is in progress
7. Copies the chosen snapshot into a temporary staging area
8. Swaps the staged user data into place and restores `config.sqlite`
9. Re-runs migrations as needed for older compatible backups
10. Brings users back to their previous quarantine state

If a step fails after the pre-restore snapshot is created, the server restores that pre-restore snapshot automatically and reports a rollback error to the CLI.

Restore scope notes:

- current implementation is full snapshot restore only
- single-user snapshot restore is not part of the current backup contract
- `config.toml`, reverse proxy config, and TLS assets remain operator-managed files outside the snapshot
- if the restored snapshot did not include secrets, keep or reconfigure the operator token as needed for the target environment

After startup, the server now performs its own recovery assessment before
normal service. That means a restored user may come up as:

- `Healthy`
- `Rebuildable`
- `NeedsOperatorAttention`

If a restored user is placed offline automatically at startup, inspect that
user with the admin diagnostics surface, review the recovery assessment, and
only bring them back online after review.

Example:

```bash
cmdock-admin backup list
cmdock-admin backup restore 2026-04-06T10-00-00
cmdock-admin doctor
```

### 9.0.1 New Host Disaster Recovery

Typical DR flow onto a fresh host:

1. Retrieve `backup_dir` contents from NAS, S3, restic, or similar
2. Install `cmdock-server` and configure `backup_dir`
3. Restore `config.toml`, reverse proxy config, and TLS assets as needed
4. Run `cmdock-admin backup restore <timestamp>`
5. Reconfigure the operator token if the snapshot did not include secrets
6. Run `cmdock-admin doctor` and confirm task and sync health
## 9.1 Recovery Modes

There are two distinct recovery ideas operators should keep separate.

### Whole-server snapshot restore

This is the backup contract implemented by `cmdock-admin backup restore`.

### Per-user repair and recovery

This still exists as an operational concept for quarantine, device repair, and sync recovery, but it is not the current snapshot-restore contract. Treat it as a separate recovery workflow rather than as a variant of `backup restore`.

## 9.2 Restore Consistency Between Canonical and Sync State

Yes, this matters.

After restore, it is not enough to ask only:

- did the files restore successfully?

You also need to ask:

- are the canonical replica and the shared sync DB logically consistent enough to resume service safely?

### What “restore consistency” means here

In this server, there are two related task-state layers:

- the canonical per-user replica
- the shared per-user TaskChampion sync DB

An ideal backup/restore gives you both from the same logical point in time.

That is the cleanest case because:

- canonical state matches the protocol-facing sync state as of the same backup point
- the bridge has minimal repair work to do
- operators get predictable post-restore behavior

### The easy case: snapshot-consistent restore

If the restore came from:

- one filesystem snapshot
- one coordinated stop-and-copy backup
- one backup set produced as a whole

then canonical and device state are usually consistent enough to bring the system back up directly.

You should still validate sync, but you are not starting from an obviously mixed-time restore.

### The harder case: mixed-point restore

Problems arise when the restored files represent different moments in time.

Examples:

- `config.sqlite` is from one backup point but `users/` is from another
- canonical `taskchampion.sqlite3` is newer than `sync.sqlite`
- `sync.sqlite` is restored only partially
- one user directory is restored from a different date than the rest of the system

In those cases, the files may all be valid SQLite files, but the overall sync picture may be inconsistent.

### What the system can usually repair

If the canonical replica is intact and the device metadata/secrets are intact, the server can often repair logical divergence by:

- letting the bridge reconcile canonical state into stale sync state
- rebuilding a missing or unusable `sync.sqlite` from canonical state

That means post-restore inconsistency is not always catastrophic.

### What the system does not guarantee to repair exactly

The bridge is a logical reconciliation mechanism, not a byte-for-byte restore tool.

So even if the system returns to service, that does not mean:

- the sync DB retains its original exact version graph
- every snapshot remains identical to the pre-failure state
- every device-side history detail is preserved exactly

The guarantee is better understood as:

- restoration of working sync behavior

not:

- perfect replay of original transport history

### When the operator should intervene instead of trusting automatic convergence

Automatic convergence is not always the best recovery choice.

An operator should consider explicit intervention when:

- the restored sync DB is missing entirely
- the device was compromised
- the device metadata and chain look mismatched
- the user should not continue with the old device identity
- post-restore sync behavior is unclear or unstable

In those cases the cleaner path is often:

1. revoke the old device
2. create a new device
3. reconfigure the client

### Practical recovery rule

After restore:

- if canonical and sync state came from one consistent backup set, validate and continue
- if the sync DB is missing or obviously behind, allow rebuild from canonical if the device identities are still trusted
- if trust or consistency is doubtful, revoke and re-register the device instead of trying to preserve it

## 9.3 Partial Recovery if The Sync DB Is Missing

If a restore brings back:

- `config.sqlite`
- the canonical user replica

but `sync.sqlite` is missing, recovery is still often possible.

For an active device with intact registry metadata, the practical options are:

1. Let the server rebuild a fresh shared sync DB from canonical state.
2. If that is not desirable, revoke the old device and register a new one.

The first path preserves the same device identity when possible.

The second path is cleaner when:

- the old device state is suspect
- the old device was compromised
- the operator would rather rotate credentials than reuse them

Operationally, the second path is often simpler:

- `revoke` the old device if it still exists in metadata
- `create` a replacement device
- reconfigure the client

## 9.4 Preconditions for Sync DB Reconstruction

Reconstruction from canonical state depends on more than just the canonical task DB.

The server still needs:

- the device row
- the device `client_id`
- the encrypted device secret
- the user’s canonical sync identity
- the master key needed to decrypt secret material

If those are missing, the correct recovery path is usually to register a new device rather than trying to reconstruct the old one.

## 9.5 Online Selective Restore While The Server Remains Running

This is not part of the current snapshot-restore contract.

Today, `cmdock-admin backup restore` restores a whole snapshot and the server
handles the quarantine window internally.

If you need per-user repair while the server remains online:

- use the quarantine and diagnostics endpoints as recovery tools
- do not treat it as the same procedure as `backup restore`
- do not replace files under a live user while cached runtime state may still exist

Selective snapshot restore remains a future capability with different
consistency requirements.

## 10. Post-Restore Validation

After restore, verify all of these:

- `/healthz` returns success
- a known bearer token still authenticates
- `cmdock-admin user list` works
- the admin device diagnostics surface shows expected devices for at least one user
- REST task listing works for at least one known user
- TaskChampion sync works for at least one registered device
- logs show no quarantine/corruption events during startup
- `cmdock-admin doctor` reports a healthy operator view

## 11. Backup and Device Lifecycle

Device lifecycle affects backup interpretation.

### 11.1 Revoked Devices

Revoked device rows remain in metadata and may still appear in backups.

That is expected:

- revoke is a security control
- delete is cleanup

### 11.2 Deleted Devices

Deleted devices remove:

- the device row

They do not remove the shared `sync.sqlite`.

Older backups may still contain historical sync state or optional legacy
per-device files. That is normal.

### 11.3 Restore Implication

If you restore an older backup:

- previously deleted devices may reappear
- previously revoked devices may revert to their older status

So after restoring an old backup, device lifecycle should be reviewed.

### 11.4 Recovery Implication for Missing Device Chains

If an older or partial backup restores metadata for a device but not its device sync DB:

- the device may still be recoverable from canonical state
- but the recovered chain should be treated as newly rebuilt sync state, not as a perfect preservation of the old chain

That distinction matters if the operator is trying to reason about exact history rather than just a return to service.

## 12. Common Mistakes

Avoid these:

- copying live SQLite files with `cp`
- backing up only `config.sqlite` and forgetting `users/`
- backing up only `taskchampion.sqlite3` and forgetting `sync/`
- restoring over the top of current data without preserving a rollback copy
- forgetting to validate device sync after restore
- assuming revoked/deleted device state from today exists in an older backup

## 13. Suggested Operator Policy

For a self-hosted deployment, a sensible baseline policy is:

- daily full backup
- extra backup before upgrades
- a documented preferred recovery tier
- regular restore rehearsal for at least one user and one full-instance backup

## 14. Disaster Recovery Tiers for Operators

This guide is about backup and restore procedure, but operators also need a
simple mental model for which broader DR posture they are running.

For the detailed architecture analysis and trade-offs, see
[Disaster Recovery Architecture](../reference/disaster-recovery-reference.md).

### 14.1 Tier 1: Backup and Restore

This is the minimum viable DR posture:

- take regular backups
- restore to a replacement server when needed
- accept an RPO measured in backup frequency
- accept an RTO measured in restore and bring-up time

This guide fully documents that model.

### 14.2 Tier 2: Continuous SQLite Shipping

This is the next practical step for self-hosters who want a lower RPO without
changing the application architecture:

- keep SQLite
- continuously ship WAL state to external storage
- restore from that external state on failure

Operationally, this reduces the amount of data loss between backups, but it does
not replace the restore procedures in this guide. You still need backup
validation, restore validation, and operator runbooks.

### 14.3 Tier 2b: Warm Standby

This keeps the same storage model but shortens recovery time:

- maintain a standby environment
- keep it near-current from replicated SQLite state
- fail over to it when the primary is lost

The main operator difference is that failover becomes faster, but restore and
consistency validation still matter.

### 14.4 Tier 3 and Beyond

Beyond warm-standby SQLite, DR starts becoming an architecture and product
strategy topic rather than just a backup procedure topic:

- replicated metadata stores
- active/standby or active/active topology
- region-level failover
- stronger guarantees around auth consistency and task replication

That material belongs in the DR architecture reference rather than this manual.
- restore rehearsal at least once
- retain at least 30 days
- keep one off-host copy

If the server matters enough to care about recovery, it matters enough to test recovery.
