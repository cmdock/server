---
created: 2026-03-30
status: accepted
tags: [architecture, sync, encryption, mvp, teams]
---

# ADR-0001: Sync Bridge — Unifying REST API and TaskChampion Sync Protocol

## Context

The cmdock server has two independent data paths that must be unified:

| Path | Auth | Storage | Data |
|------|------|---------|------|
| iOS app → REST API (`/api/tasks`) | Bearer token | TaskChampion Replica (`taskchampion.sqlite3`) | Plaintext |
| TW CLI → TC sync protocol (`/v1/client/...`) | X-Client-Id | Sync storage (`sync.sqlite`) | Encrypted blobs |

**Problem:** Tasks created via the iOS app don't appear in `task sync`, and vice versa.

## Critical Discovery: TC Encryption Model

Confirmed by TC source code, Codex, and Gemini:

1. **Encryption salt = client_id.** Different client_ids derive different keys. Devices using different client_ids **cannot decrypt each other's blobs.**
2. **All replicas sharing a version chain MUST use the same `{client_id, encryption_secret}`.** The `client_id` is a sync pool ID, not a per-device ID.
3. **TC sync is single-chain.** One `task sync` = one endpoint, one key pair, one version chain. TC doesn't support merging multiple sync targets.

## Decision

### Architecture: Per-User Aggregate Feed (Option D)

Validated independently by both Codex and Gemini as the only viable path:

**Each user has ONE Replica containing ALL their tasks (personal + team).** The Replica is a **projection** — a materialised view of the user's authorised data. The TC sync protocol serves this projection to the TW CLI.

```
TW CLI (laptop) ─┐                                    ┌─ Personal tasks
TW CLI (phone)  ─┤── TC sync ──→ User's Replica ←─────┤
Server bridge   ─┘                     ↑               └─ Team tasks (fan-out)
                                       │
iOS app ─── REST API ──────────────────┘
```

**For MVP (single user):** Replica = source of truth. One user, one replica, one sync identity.

**For Teams:** Server maintains canonical shared data. When Alice edits a team task, the server fans out the operation into Bob's and Charlie's Replicas. Their TW CLI sees the change on next sync. Bob's CLI is unaware of teams — it just sees tasks with project prefixes like `ENGINEERING.Sprint42`.

### Why this works

- **Zero client complexity.** TW CLI syncs one endpoint, sees all tasks. No multi-config.
- **Trivial team removal.** Server injects Delete operations into departing member's Replica.
- **Conflict resolution.** TC's OT handles it within each Replica. Server resolves cross-replica conflicts canonically, then projects.
- **Task duplication is cheap.** 100 team members = 100 copies of team tasks. Task payloads are tiny; storage is not the bottleneck.
- **Same pattern as Linear, Todoist, TickTick.** One workspace, projects as namespaces, sharing as ACLs.

### Handler Boundary

The sync bridge is a designated orchestrator. That does **not** mean ordinary
task CRUD handlers should depend on bridge scheduling mechanics directly.

The intended boundary is:

- task CRUD mutates canonical state
- task CRUD emits "canonical changed" intent
- bridge scheduling policy remains behind a narrower coordinator/orchestrator
  boundary

This keeps REST task logic independent from bridge queue mechanics while still
preserving the bridge architecture itself.

### Encryption and Key Management

- **One `{client_id, encryption_secret}` per user.** All devices + server bridge share it.
- **Server escrows the key.** Required for REST API bridge and team fan-out.
- **NOT end-to-end encrypted.** Same trade-off as every collaboration tool with a web/mobile UI.
- **Envelope encryption for stored secrets.** `encryption_secret` encrypted with `MASTER_ENCRYPTION_KEY` (env var) before storing in SQLite. Server decrypts in memory at runtime.
- **Key rotation on team member departure:** auth revocation sufficient for MVP. Cryptographic rotation (re-key shared replicas) is Enterprise phase.

### Device Identification for Audit

This section is superseded in part by ADR-0003.

At the time of this ADR, the server used one shared `{client_id, encryption_secret}`
per user, so `client_id` could not identify physical devices. ADR-0003 changes that
model: each device now gets its own `client_id`, is tracked in the `devices` table,
and can be revoked independently.

What remains true from this ADR:

- Audit events should still record IP address, timestamp, and version identifiers.
- The server remains the trust boundary for sync bridging and key escrow.

See ADR-0003 for device identity and lifecycle, and ADR-0004 for the boundary
between the open core server and the managed control-plane UX.

### Implementation: Server Trait + Reimplemented Crypto

- Server implements `taskchampion::Server` trait directly against `sync.sqlite`
- Reimplements TC encryption envelope (~50 lines using `ring`):
  - ChaCha20-Poly1305 (AEAD)
  - PBKDF2-HMAC-SHA256, 600K iterations
  - Salt = client_id bytes, AAD = app_id + version_id
- No HTTP loopback — direct function call from Replica::sync()

## Schema (Forward-Compatible)

### Phase 1 (MVP — single user)

```sql
-- Per-user replica + sync identity
-- The replica IS the sync pool. client_id = replica id.
CREATE TABLE replicas (
    id TEXT PRIMARY KEY,                    -- UUID, also used as client_id for TC sync
    user_id TEXT NOT NULL REFERENCES users(id),
    encryption_secret_enc TEXT NOT NULL,     -- encrypted with MASTER_KEY
    label TEXT NOT NULL DEFAULT 'Personal',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(user_id)                         -- MVP: one replica per user
);
```

### Phase 3 (Teams — add these tables, relax UNIQUE)

```sql
-- Canonical data scopes (personal or team)
CREATE TABLE scopes (
    id TEXT PRIMARY KEY,
    scope_type TEXT NOT NULL CHECK(scope_type IN ('personal', 'team')),
    owner_id TEXT NOT NULL,                 -- user_id for personal, org_id for team
    label TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Who can access which scope
CREATE TABLE scope_members (
    scope_id TEXT NOT NULL REFERENCES scopes(id),
    user_id TEXT NOT NULL REFERENCES users(id),
    role TEXT NOT NULL DEFAULT 'member',
    PRIMARY KEY (scope_id, user_id)
);

-- replicas.user_id UNIQUE constraint dropped
-- Each user still has one replica (projection), but it now
-- contains tasks from multiple scopes via server fan-out
```

### Migration path

Phase 1 → Phase 3: `replicas` table unchanged. Add `scopes` + `scope_members`. Server fan-out logic routes operations between scopes and user projections. Existing personal replicas become scope-type='personal'.

## Consequences

### Positive
- iOS + TW CLI see same tasks (MVP)
- Forward-compatible for Teams (no schema break)
- TW CLI unaware of teams (zero client work)
- Audit trail via per-device tokens + IP
- Linear/Todoist-compatible architecture

### Negative
- Server holds encryption keys (not E2E)
- ~50 lines of crypto code to maintain
- Team fan-out adds complexity (Phase 3)
- Task duplication across user replicas (acceptable — tiny payloads)

### Security
- Envelope encryption for secrets at rest (MASTER_KEY from env)
- Auth revocation on team departure (sufficient for MVP)
- Self-hosters control their own master key
