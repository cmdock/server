# Installation and Setup Guide

cmdock-server is a single Rust binary. It serves a REST API over plain HTTP on port 8080. **HTTPS requires a reverse proxy** (Caddy, nginx, Traefik, etc.) — the server does not terminate TLS itself.

See also:

- [Documentation Library](index.md)
- [Concepts Guide](concepts-guide.md)
- [Administration Guide](administration-guide.md)
- [Backup and Recovery Guide](backup-and-recovery-guide.md)

> **Note:** Direct TLS is not configured in `cmdock-server`. Always put a
> reverse proxy in front.
>
> **Deployment note:** If you run the server behind shared ingress or other
> environment-specific reverse-proxy infrastructure, keep the ingress and
> operator-surface security requirements in
> [Administration Guide](administration-guide.md) and
> this guide in mind. In particular,
> `/admin/*` should be treated as an operator surface, not a
> public user API. The trusted-proxy and forwarded-header assumption is
> formalised in
> [ADR-0007: Trusted Proxy and Forwarded-Header Model](../adr/ADR-0007-trusted-proxy-and-forwarded-header-model.md).
> If you want audit/auth diagnostics to use proxy-provided client IPs, set
> `[server].trust_forwarded_headers = true` or
> `CMDOCK_TRUST_FORWARDED_HEADERS=true` only when the server is actually behind
> trusted ingress.

## What works today

The server provides a bearer-token REST API for client applications. Clients
sync tasks over this REST API using normal user auth tokens.

**Compatibility-only:**
- `/api/sync` (REST endpoint) is a no-op retained only while supported clients
  still call it. Taskwarrior CLI sync uses the TaskChampion sync protocol at
  `/v1/client/*`, which is the real sync surface.

**Not yet implemented:**
- import/export and some broader deployment conveniences are still planned but not yet built.

---

## Option A: Docker Compose with Caddy (recommended)

Caddy handles HTTPS automatically. For public DNS names, use Let's Encrypt.
For private LAN or lab environments, point Caddy at your local ACME-compatible
CA such as Smallstep and distribute trust to your clients.

### Prerequisites

- Docker Engine 24+ and Docker Compose v2
- A domain name with DNS A record pointing to your server
- Ports 80 and 443 open

### 1. Create a working directory

```bash
mkdir cmdock-server
cd cmdock-server
```

### 2. Create `compose.yaml`

```yaml
services:
  server:
    image: ghcr.io/cmdock/server:latest
    container_name: cmdock-server
    restart: unless-stopped
    volumes:
      - server-data:/app/data
      - ./config.toml:/app/config.toml:ro
    expose:
      - "8080"
    environment:
      - RUST_LOG=cmdock_server=info
    healthcheck:
      test: ["CMD", "curl", "-sf", "http://localhost:8080/healthz"]
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 10s

  caddy:
    image: caddy:2-alpine
    container_name: cmdock-caddy
    restart: unless-stopped
    ports:
      - "80:80"
      - "443:443"
    environment:
      - DOMAIN=${DOMAIN:-localhost}
      - CADDY_TLS_SNIPPET=${CADDY_TLS_SNIPPET:-tls_automatic}
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy-data:/data
      - caddy-config:/config
    depends_on:
      server:
        condition: service_healthy

volumes:
  server-data:
  caddy-data:
  caddy-config:
```

### 3. Create `Caddyfile`

```caddyfile
(tls_automatic) {
}

(tls_internal) {
    tls internal
}

{$DOMAIN:localhost} {
    import {$CADDY_TLS_SNIPPET:tls_automatic}
    reverse_proxy server:8080
}
```

### 4. Create `config.toml`

```toml
backup_dir = "/app/data/backups"

[server]
host = "0.0.0.0"
port = 8080
data_dir = "/app/data"
public_base_url = "https://tasks.example.com"
trust_forwarded_headers = true

[admin]
http_token = "replace-with-a-long-random-operator-token"
```

This is the minimum public self-hoster shape.

### 5. Set your domain

Set the domain in your shell before starting Compose:

```bash
export DOMAIN=tasks.example.com
```

For localhost-only testing without public DNS, switch Caddy to its internal
development certificate mode:

```bash
export DOMAIN=localhost
export CADDY_TLS_SNIPPET=tls_internal
```

### 6. Start

```bash
docker compose up -d
```

With public ACME, first start takes ~30 seconds while Caddy provisions a TLS
certificate. With `tls_internal`, Caddy provisions a local development
certificate instead.

### 7. Create your first user

```bash
docker compose exec server cmdock-server admin user create --username alice
```

Save the printed API token. It is displayed once and cannot be retrieved later.

### 8. Verify

```bash
curl -s https://tasks.example.com/healthz | jq .
```

### 9. Connect a client

- **Server URL:** `https://tasks.example.com`
- **API Token:** the token from step 4

If you are using the first-party iOS client, enter those values in the app's
server settings.

### Optional Standalone Admin CLI

`cmdock-admin` is the optional standalone operator CLI for live systems. Use it
when you want a remote operator tool for backup, restore, doctor checks,
device and user administration, and admin webhook management over the server's
admin HTTPS API.

Install it from the public `cmdock/cli` release page:

- `https://github.com/cmdock/cli/releases`

Typical usage:

```bash
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" doctor
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" backup list
```

This CLI is optional. `cmdock-server` still ships a local on-host admin CLI for
local or offline break-glass work.

---

## Option B: Behind an existing reverse proxy

If you already run nginx, Traefik, or HAProxy, run the server without Caddy and point your proxy at port 8080.

### docker-compose.yml (no Caddy)

```yaml
services:
  server:
    image: ghcr.io/cmdock/server:latest
    container_name: cmdock-server
    restart: unless-stopped
    ports:
      - "127.0.0.1:8080:8080"   # Only expose to localhost
    volumes:
      - server-data:/app/data
      - ./config.toml:/app/config.toml:ro
    environment:
      - RUST_LOG=cmdock_server=info
    healthcheck:
      test: ["CMD", "curl", "-sf", "http://localhost:8080/healthz"]
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 10s

volumes:
  server-data:
```

Example nginx location block:

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Host $host;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
}
```

---

## Option C: Build from source (no Docker)

### Prerequisites

- Rust 1.80+ (install via [rustup](https://rustup.rs/))
- `just` command runner: `cargo install just`

### 1. Build

```bash
git clone https://github.com/cmdock/server.git
cd server
just build-release
```

The binary is at `target/release/cmdock-server`.

### 2. Configure

```bash
cp config.example.toml config.toml
```

### 3. Run

```bash
./target/release/cmdock-server --config config.toml
```

Put a reverse proxy (Caddy or nginx) in front for HTTPS.

### 4. systemd service

Create `/etc/systemd/system/cmdock-server.service`:

```ini
[Unit]
Description=cmdock sync server
After=network.target

[Service]
Type=simple
User=cmdock
Group=cmdock
WorkingDirectory=/opt/cmdock-server
ExecStart=/opt/cmdock-server/cmdock-server --config /opt/cmdock-server/config.toml
Restart=on-failure
RestartSec=5

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/opt/cmdock-server/data

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now cmdock-server
```

---

## Data and persistence

### What gets stored

```
data/
├── config.sqlite               # User accounts, tokens, views, contexts, presets
├── config.sqlite-wal           # SQLite write-ahead log
└── users/
    ├── <user-id>/
    │   ├── taskchampion.sqlite3  # Canonical plaintext replica for REST/iOS
    │   ├── sync.sqlite            # Shared encrypted TC sync chain for all devices
    │   └── sync/
    │       └── <client-id>.sqlite # Optional legacy/maintenance artifact
    └── ...
```

See `docs/reference/storage-layout-reference.md` for the complete
layout including WAL/SHM sidecars and the `.offline` quarantine marker.

### Persistence

| Path | Must persist? | Notes |
|------|:---:|-------|
| `config.sqlite` | **Yes** | Accounts, tokens, all config. Loss = loss of user accounts and server config (task data in `users/` survives but is inaccessible without accounts). |
| `users/` | **Yes** | Task data. One canonical replica plus one shared per-user sync DB. |
| `config.toml` | Replaceable | Can be regenerated from `config.example.toml`. |

In Docker, always use a named volume or host bind mount. Never rely on container-local storage.

---

## Backup and restore

### Backup

The supported snapshot flow now uses the standalone `cmdock-admin` CLI over the
admin HTTPS API. The server writes a consistent snapshot into the configured
`backup_dir`; the operator then copies that staging directory off-host with
their normal tooling.

```bash
# Create a snapshot
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" backup

# Review available snapshots
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" backup list
```

For Docker deployments, run `cmdock-admin` from the host or another operator
machine that can reach the published admin endpoint, then copy the configured
`backup_dir` off-host as needed:

```bash
docker compose cp server:/data/cmdock/backups ./backups
```

For backup scope, retention, and off-host copy examples, see the
[Backup and Recovery Guide](backup-and-recovery-guide.md).

### Automated backup

For scheduled backups, the canonical pattern is a small `systemd` service
plus timer that runs `cmdock-admin backup`. The full unit files, timer
config, and enable steps live in
[Backup and Recovery Guide §4.5 Automate Snapshot Creation With `systemd`](backup-and-recovery-guide.md#45-automate-snapshot-creation-with-systemd).

After the timer runs, keep the off-host copy as a separate scheduled step
with whatever tooling you already trust (`rsync`, `restic`, or equivalent).

### Restore

Snapshot restore is a full-instance operation driven through the admin HTTPS
API:

```bash
cmdock-admin backup list
cmdock-admin backup restore 2026-04-06T10-00-00
cmdock-admin doctor
```

The server validates the snapshot, creates an automatic `pre-restore-*` safety
snapshot, stages the restore, and rolls back automatically if a later restore
step fails. See the [Backup and Recovery Guide](backup-and-recovery-guide.md)
for the full contract and disaster-recovery flow.

---

## TLS and certificates

The server binds plain HTTP. HTTPS is handled by the reverse proxy.

| Scenario | Approach |
|----------|----------|
| Fresh server, no existing proxy | Option A — Caddy auto-HTTPS |
| Already have nginx/Traefik | Option B — proxy to port 8080 |
| Development / testing | No TLS needed — `http://localhost:8080` |

**Caddy certificate renewal** is automatic — no action required.

### Self-signed certificates (testing)

```bash
openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem \
  -days 365 -nodes -subj '/CN=localhost'
```

Configure your reverse proxy to use these.

For one-off testing this can work, but for any homelab or multi-device setup a
small private CA is a better path than copying a raw self-signed leaf
certificate to every device. We recommend
[`step-ca`](https://smallstep.com/docs/step-ca/) from Smallstep for this use
case because it gives you one CA root to trust and lets Caddy or other ACME
clients renew server certificates automatically.

#### Getting the CA certificate onto iPhone

The iPhone needs the CA certificate in PEM or DER form.

Common ways to deliver it:

- AirDrop the CA certificate file to the phone
- email the CA certificate file to yourself and open the attachment on iPhone
- host the CA certificate at a simple URL such as `https://tasks.example.com/ca.pem`

If you use the `/ca.pem` approach, serve that file from your reverse proxy or
another static file host. `cmdock-server` does not serve arbitrary files by
itself.

#### iOS install steps

After opening the CA certificate on iPhone:

1. Open `Settings`.
2. Go to `General` → `VPN & Device Management`.
3. Under `Downloaded Profile`, select the certificate profile.
4. Tap `Install` and follow the prompts.

#### iOS trust steps

Installing the profile is not enough on its own. You must also enable trust for
the CA:

1. Open `Settings`.
2. Go to `General` → `About` → `Certificate Trust Settings`.
3. Enable full trust for your installed root CA.
4. Confirm the trust warning.

After that, restart the cmdock iOS app and connect again.

#### Important notes

- The iOS app does **not** have a "skip certificate validation" option. This is
  intentional.
- For `GET /api/tasks`, `/api/me`, sync, and all other server traffic, the app
  expects normal TLS trust to succeed.
- If you expect to connect multiple phones, tablets, or laptops, prefer a
  Smallstep/private-CA setup over a single raw OpenSSL self-signed certificate.

---

## Configuration reference

### config.toml

```toml
[server]
host = "0.0.0.0"        # Bind address
port = 8080              # HTTP port
data_dir = "./data"      # Data directory — must be persistent

[admin]
http_token = "replace-with-a-long-random-operator-token"

```

There is no active `[sync]` config section. The TaskChampion sync protocol is
always on.

Older configs may still include historical `[tls]` or `[sync]` sections. The
current runtime ignores them; do not rely on them for deployment behavior.
If you want to use `/admin/*` over HTTP, configure `[admin].http_token` or set
`CMDOCK_ADMIN_TOKEN`. This token is separate from end-user API tokens.

Operator-token distribution pattern:

- generate one long random operator token per environment
- inject that same value into the server as `CMDOCK_ADMIN_TOKEN`
- inject that same value separately into any trusted operator automation that
  will call `/admin/*`
- provide it explicitly to operator tooling when needed, for example by
  exporting it in the shell before running deployed verification helpers
- do not read it back from the server at runtime
- do not give it to end-user clients or user-facing web apps

If you run deployed verification against a pre-production environment, prefer a
host that already has direct HTTPS trust for that environment rather than
working around trust locally. The repository-owned helper entrypoint is
`scripts/staging-test.sh`; see the
[Testing Strategy Reference](../reference/testing-strategy-reference.md) for
how that fits into the overall validation story.

### Environment variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `RUST_LOG` | Log level | `cmdock_server=info` |
| `CMDOCK_ADMIN_TOKEN` | Operator bearer token for `/admin/*` | (none) |

---

## Admin CLI

`cmdock-server` still ships a local admin CLI for direct on-host maintenance.
The standalone `cmdock-admin` repo owns the remote HTTPS admin CLI surface.
Use `cmdock-admin` as the recommended day-to-day operator tool for live
deployments over HTTPS. Keep `cmdock-server admin` available as the
self-sufficiency and break-glass/on-host maintenance surface.

```bash
# User management
cmdock-server admin user create --username <name>
cmdock-server admin user list
cmdock-server admin user delete <user-id> [-y]
cmdock-server admin user offline <user-id>
cmdock-server admin user assess <user-id>
cmdock-server admin user online <user-id>

# Token management
cmdock-server admin token create <user-id> [--label <label>]
cmdock-server admin token list <user-id>
cmdock-server admin token revoke <hash-or-prefix> [-y]

# Canonical sync identity (one per user)
cmdock-server admin sync create <user-id>
cmdock-server admin sync show <user-id>
cmdock-server admin sync delete <user-id>  # Destructive whole-user reset
# Hidden debug/migration helper:
# cmdock-server admin sync show-secret <user-id>

# Device lifecycle (one per physical device)
cmdock-server admin device list <user-id>
cmdock-server admin device create <user-id> --name "Work MacBook" [--server-url <public-url>]
cmdock-server admin connect-config create <user-id> [--server-url <public-url>] [--name <display-name>]
cmdock-server admin device taskrc <user-id> <client-id> [--server-url <public-url>]
cmdock-server admin device revoke <user-id> <client-id> [-y]
cmdock-server admin device unrevoke <user-id> <client-id> [-y]
cmdock-server admin device delete <user-id> <client-id> [-y]  # Revoke first, then delete

# Backup and restore
# Use the standalone `cmdock-admin` CLI and the Backup and Recovery Guide
# for the supported snapshot flow.
```

For self-hosted onboarding:

1. Run `admin sync create` once per user.
2. Run `admin device create` once per physical client.
3. For Taskwarrior/manual sync clients, copy the emitted `server_url`,
   `client_id`, and secret into the client.
4. For QR or deep-link onboarding, use `admin connect-config create` to
   print a short-lived `cmdock://connect?...` URL and terminal QR.
5. Use `revoke` for normal device removal. Use `delete` only to remove an already-revoked record permanently.

Design decision:

- `admin connect-config create` only supports `https://` server URLs
- self-hosters who do not run HTTPS cannot use the QR / deep-link onboarding
  flow
- those operators should use `admin device create` and manually transcribe the
  emitted values instead

Connect-config verification note:

- the QR/deep-link flow is considered successful once the imported credential
  completes its first successful authenticated API call, usually `GET /api/me`
- `admin token list <user-id>` shows `FIRST_USED`, `LAST_USED`, and `LAST_IP`
  for the short-lived `connect-config` token so operators can confirm the scan
  actually reached the server

Typical operator flow:

```bash
# 1. Create a user and save the API token
cmdock-server admin user create --username alice

# 2. Create the user's canonical sync identity once
cmdock-server admin sync create <user-id>

# 3. Create a device and emit onboarding values
cmdock-server admin device create <user-id> --name "Alice MacBook" --server-url https://tasks.example.com

# 3b. For QR/deep-link onboarding, emit a short-lived connect URL and terminal QR
cmdock-server admin connect-config create <user-id> --server-url https://tasks.example.com --name "Alice Tasks"

# 4. Later: review device state
cmdock-server admin device list <user-id>

# 5. Normal removal path
cmdock-server admin device revoke <user-id> <client-id> -y

# 6. Optional permanent cleanup after revocation
cmdock-server admin device delete <user-id> <client-id> -y
```

`admin device create` prints both a `.taskrc`-compatible snippet for Taskwarrior
and the same values in a manual-entry-friendly form for any client that uses
manual setup.

`admin connect-config create` is stricter by design: it only emits
`cmdock://connect?...` payloads for HTTPS server origins. Plain-HTTP
self-hosted setups must use the manual onboarding path from `admin device create`.

Note: sync/device provisioning commands require `CMDOCK_MASTER_KEY` so the
server can decrypt the canonical secret and derive per-device credentials.

In Docker:

```bash
docker compose exec server cmdock-server admin <command>
```

Use `--data-dir /app/data` to bypass the config file (useful for pre-provisioning on a fresh container).

---

## Monitoring

### Health check

```bash
curl -s http://localhost:8080/healthz
# {"status":"ok","pending_tasks":"0"}
```

No authentication required. Use for Docker healthchecks and load balancer probes.

### Prometheus metrics

```bash
curl -s http://localhost:8080/metrics
```

Key metrics: `http_requests_total`, `http_request_duration_seconds`,
`replica_operation_duration_seconds`, `auth_cache_total`,
`filter_evaluation_duration_seconds`, `disk_available_bytes{scope="data_dir"}`,
and `disk_available_bytes{scope="backup_dir"}`.

For self-hosted operators, set alerts on low `disk_available_bytes` for both
`data_dir` and `backup_dir`. `data_dir` protects normal runtime writes;
`backup_dir` protects backup and restore staging.

---

## Upgrading

### Docker

```bash
docker compose pull
docker compose up -d
```

Migrations run automatically on startup.

### Binary

```bash
git pull
just build-release
just upgrade target/release/cmdock-server
```

---

## Troubleshooting

### Server won't start

```bash
docker compose logs server
# Common: port conflict, missing config mount, permission denied on data dir
```

### Can't connect from client

```bash
curl -v https://your-domain/healthz
curl -H "Authorization: Bearer TOKEN" https://your-domain/api/tasks
```

### Database locked

```bash
# Checkpoint WAL via HTTP admin endpoint (requires auth)
curl -X POST -H "Authorization: Bearer TOKEN" http://localhost:8080/admin/user/<id>/checkpoint

# Or evict cached replica
curl -X POST -H "Authorization: Bearer TOKEN" http://localhost:8080/admin/user/<id>/evict
```

### Reset everything

```bash
docker compose down -v    # Removes volumes — ALL DATA LOST
docker compose up -d
```
