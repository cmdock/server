# cmdock-server

AGPL-3.0 task sync server powered by [TaskChampion](https://github.com/GothenburgBitFactory/taskchampion). Provides a bearer-token REST API for client applications, with a native Taskwarrior filter engine and OpenAPI documentation.

**Status:** Admin-provisioned users with REST sync for first-party and self-hosted clients — shipped and tested. The server binds plain HTTP only, so put a reverse proxy (Caddy, nginx) in front for HTTPS.

## Quick Start

For the full manual set, start with the **[Documentation Library](docs/manuals/index.md)**.

```bash
mkdir cmdock-server && cd cmdock-server
cat > compose.yaml <<'YAML'
services:
  server:
    image: ghcr.io/cmdock/server:latest
    restart: unless-stopped
    volumes:
      - server-data:/app/data
      - ./config.toml:/app/config.toml:ro
    expose:
      - "8080"
  caddy:
    image: caddy:2-alpine
    restart: unless-stopped
    ports:
      - "80:80"
      - "443:443"
    environment:
      - DOMAIN=tasks.example.com
      - CADDY_TLS_SNIPPET=tls_automatic
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy-data:/data
      - caddy-config:/config
    depends_on:
      - server
volumes:
  server-data:
  caddy-data:
  caddy-config:
YAML

cat > Caddyfile <<'CADDY'
(tls_automatic) {
}

(tls_internal) {
    tls internal
}

{$DOMAIN:localhost} {
    import {$CADDY_TLS_SNIPPET:tls_automatic}
    reverse_proxy server:8080
}
CADDY

cat > config.toml <<'TOML'
backup_dir = "/app/data/backups"

[server]
host = "0.0.0.0"
port = 8080
data_dir = "/app/data"
public_base_url = "https://tasks.example.com"
trust_forwarded_headers = true

[admin]
http_token = "replace-with-a-long-random-operator-token"
TOML

docker compose up -d
docker compose exec server cmdock-server admin user create --username alice
# Save the printed token, then connect a client
```

This self-hoster path pulls the published server image from
`ghcr.io/cmdock/server:latest` and does not require cloning the repository.
For the fuller step-by-step version, see the
[Installation and Setup Guide](docs/manuals/installation-and-setup-guide.md).

## Operator Quick Start

Create one backup, list it, and register one admin webhook:

```bash
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" backup
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" backup list
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" webhook create \
  --url https://hooks.example.com/cmdock \
  --secret "<shared-secret>" \
  --events task.created,task.completed
```

If you are using the standard Docker Compose bundle on the same host and have
set `backup_dir = "/app/data/backups"` in `config.toml`, copy snapshots off-host with:

```bash
docker compose cp server:/app/data/backups ./backups
```

## Admin CLI

`cmdock-admin` is the optional standalone operator CLI for live systems. Use it
when you want a remote operator tool for backup, restore, health checks,
device and user administration, and admin webhook management over the server's
admin HTTPS API.

For a running deployment, prefer `cmdock-admin` over `docker compose exec ...`
or the local `cmdock-server admin ...` path. The server-local admin CLI is
still available and is mainly for local or offline break-glass work.

Install `cmdock-admin` from the public `cmdock/cli` release page:

- `https://github.com/cmdock/cli/releases`

Then run it with your server URL and operator token:

```bash
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" doctor
cmdock-admin --server https://tasks.example.com --token "$CMDOCK_ADMIN_TOKEN" backup
```

## Installation

Use the quick-start path above for the standard GHCR image plus Docker Compose.
For other installation options:

- bare metal with systemd
- existing nginx or Traefik
- custom reverse-proxy topologies

see the [Installation and Setup Guide](docs/manuals/installation-and-setup-guide.md).

## Configuration

The server binds plain HTTP and expects HTTPS termination at the reverse proxy.
The main runtime configuration lives in `config.toml`; start from
`config.example.toml` and then follow the deployment and operator docs for:

- storage paths
- reverse-proxy setup
- admin/operator token handling
- backup staging configuration

See [docs/manuals/installation-and-setup-guide.md](docs/manuals/installation-and-setup-guide.md) for setup and [docs/manuals/administration-guide.md](docs/manuals/administration-guide.md) for operator workflows.

## Usage

Interactive docs are exposed at `/swagger-ui/` when the server is running.

## What works today

| Feature | Status |
|---------|--------|
| Task CRUD (list, add, complete, undo, delete, modify) | ✅ Shipped |
| Native Taskwarrior filter engine (22 virtual tags, named dates) | ✅ Shipped |
| REST client sync via bearer-token API | ✅ Shipped |
| Admin CLI (user/token/backup management) | ✅ Shipped |
| Docker + Caddy deploy | ✅ Shipped |
| OpenAPI 3.1.0 + Swagger UI | ✅ Shipped |
| Prometheus metrics | ✅ Shipped |
| User-scoped webhooks (`/api/webhooks`) | ✅ Shipped |
| Admin/per-server webhooks | ✅ Shipped |
| Taskwarrior CLI sync (`task sync`) | ✅ Shipped |
| LLM task summaries (Anthropic with template fallback) | ✅ Shipped |

## API

REST and operator endpoints are grouped by concern. Most require bearer token or
operator bearer auth. Unauthenticated: `/healthz`, `/metrics`, `/swagger-ui/`,
`/api-doc/openapi.json`.

| Group | Endpoints | Description |
|-------|-----------|-------------|
| Health | 1 | Server status and pending task count |
| Tasks | 6 | CRUD operations on tasks |
| Views | 3 | Filter preset definitions |
| App Config | 14 | Aggregate app-config plus typed shopping, context, store, preset, and geofence CRUD |
| Config | 3 | Legacy generic config compatibility surface; typed config/resources are preferred |
| Summary | 1 | Task summaries (LLM if configured, template fallback) |
| Devices | 4 | User-scoped device registry and sync credential lifecycle |
| Runtime Identity | 1 | Authenticated core runtime identity for the current bearer token |
| Sync | 1 | Deprecated compatibility trigger (`/api/sync` no-op) kept only while older clients still call it |
| Webhooks | User-scoped webhook CRUD, recent delivery history, test delivery, and admin/per-server webhook management |
| Admin | Operator diagnostics, bootstrap, runtime policy, device management, backup, and admin webhook control |

### Admin CLI

Manage users, tokens, sync identities, devices, and backups directly. No running server required.

```bash
cmdock-server admin user create --username alice    # Create user + print token
cmdock-server admin user list                       # List users
cmdock-server admin user delete <user-id>           # Delete user + data
cmdock-server admin user offline <user-id>          # Hold one user offline for restore/recovery
cmdock-server admin user assess <user-id>           # Recovery assessment for one user
cmdock-server admin user online <user-id>           # Bring user back online
cmdock-server admin token create <user-id>          # New API token
cmdock-server admin sync create <user-id>           # Canonical sync identity
cmdock-server admin device create <user-id> --name "Work iPhone"
cmdock-server admin connect-config create <user-id> --server-url https://tasks.example.com
```

`admin connect-config create` is intentionally HTTPS-only. Self-hosters running
plain HTTP can still onboard clients by manually transcribing the values from
`admin device create`.

The supported operator-facing snapshot backup and restore flow now uses the
standalone `cmdock-admin` CLI over the admin HTTPS API. See the
[Backup and Recovery Guide](docs/manuals/backup-and-recovery-guide.md) for that
workflow.

The same operator surface now manages admin/per-server webhooks:

```bash
cmdock-admin webhook list
cmdock-admin webhook create --url https://hooks.example.com/cmdock --secret <secret> --events task.created,task.completed
cmdock-admin webhook deliveries <webhook-id>
cmdock-admin webhook test <webhook-id>
```

## Documentation

- **[Documentation Library](docs/manuals/index.md)** — Oracle-style manual set index
- **[Concepts Guide](docs/manuals/concepts-guide.md)** — Architecture and mental model
- **[Installation and Setup Guide](docs/manuals/installation-and-setup-guide.md)** — Deployment and bootstrap
- **[Administration Guide](docs/manuals/administration-guide.md)** — Day-to-day operator workflows
- **[Backup and Recovery Guide](docs/manuals/backup-and-recovery-guide.md)** — Backup, restore, and validation
- **[API Reference](docs/reference/api-reference.md)** — Public HTTP surface and auth modes
- **[Testing Strategy Reference](docs/reference/testing-strategy-reference.md)** — Unit/integration/system/load/fuzzing intent and deployed verification boundaries

## Development

```bash
just check        # Format + clippy + tests
just test         # Run tests only
just lint         # Clippy lints
just dev          # Run with auto-reload (requires cargo-watch)
```

The source of truth for checks is the repo-local `just` and script entrypoints.
GitHub workflow wiring should call into those entrypoints rather than redefining
the checks in provider-specific YAML.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build, test, style, and PR
expectations.

## Licence

AGPL-3.0 — see [LICENSE](LICENSE).
