---
created: 2026-04-01
status: accepted
tags: [architecture, devices, crypto, sync, security]
---

# ADR-0003: Device Registry with Per-Device Encryption

## Status

**Accepted**

## Context

cmdock-server needs to track which physical devices sync with each user account. The current model has one `client_id` + one `encryption_secret` shared across all of a user's Taskwarrior CLI instances and iOS devices. This creates an operational problem:

**Lost device scenario:** If a user loses their phone or laptop, they must:
1. Revoke the shared `client_id` (breaks ALL devices)
2. Generate a new `encryption_secret`
3. Manually update `.taskrc` on every remaining TW CLI instance

We don't control the TW CLI — users configure `.taskrc` themselves. Requiring manual reconfiguration of N devices when one is lost is unacceptable UX.

### Constraints

**TaskChampion encryption:** The TC protocol derives the encryption key using `PBKDF2(encryption_secret, salt=client_id)`. Different `client_id` values produce different encryption keys, even with the same `encryption_secret`. This means:

- Devices with different `client_id`s **cannot decrypt each other's sync data**
- A shared version chain encrypted by Device A is unreadable by Device B if they have different keys
- This is fundamental to TC's design, not something we can work around client-side

**TW CLI auth:** The only device identifier in the TC sync protocol is the `X-Client-Id` header. TW 3.4 has no support for custom headers, bearer tokens, or any other auth mechanism. `client_id` is simultaneously the device identity AND the crypto salt.

**Server already sees plaintext:** The sync bridge (`sync_bridge.rs`) decrypts TC versions into a plaintext REST replica for the iOS app. The server is already a trust boundary — per-device encryption doesn't weaken the security model, it strengthens it by limiting blast radius.

### Options Evaluated

| Option | Description | Concerns Entangled (ADR-0002) | Lost Device UX |
|--------|-------------|-------------------------------|----------------|
| **A: Auth-only** | Shared crypto, revoke blocks auth | 2 (auth + devices) | Must rotate secret on all devices |
| **B: Re-encryption proxy** | Per-device keys, server translates | 5 (auth + devices + crypto + sync storage + bridge) | Revoke one device, others unaffected |
| **C: Hybrid canonical key** | Server-side key for storage + per-device transport | 6+ (everything in B + canonical key management) | Same as B, more complexity |
| **D: Per-device auth token** | Shared crypto, custom auth header | 2 (auth + devices) | Must rotate secret on all devices |

Option D is ruled out because TW CLI doesn't support custom auth headers.

Options A and D solve the tracking problem but not the lost-device rotation problem.

Option C adds complexity over B with marginal benefit (server already stores plaintext).

## Decision

**Option B: Server re-encryption proxy with per-device credentials.**

### Architecture

```
                    ┌──────────────────────┐
                    │   Canonical Replica   │
                    │  (plaintext SQLite)   │
                    │   REST API reads/     │
                    │   writes here         │
                    └──────┬───────────────┘
                           │
              ┌────────────┼────────────────┐
                           │
                    ┌──────▼────────┐
                    │ Shared sync DB │
                    │ (enc w/        │
                    │ canonical key) │
                    └──────┬────────┘
                           ↕
     TW CLI (A)      TW CLI (B)      iOS app (C)
```

**The server is the hub.** It maintains one canonical plaintext replica per user
(already exists for the REST API) plus one shared TaskChampion sync DB per
user. Each device still gets its own credentials. The sync bridge translates
between the canonical replica, the shared sync DB, and each device's envelope.

### Data Model

**`devices` table:**

| Column | Type | Description |
|--------|------|-------------|
| `client_id` | TEXT PK | UUID — the TC `X-Client-Id` for this device |
| `user_id` | TEXT FK → users | Which user owns this device |
| `name` | TEXT | Human-readable ("Simon iPhone", "Work MacBook") |
| `encryption_secret_enc` | TEXT | Per-device encryption secret, encrypted with master key |
| `registered_at` | TEXT | When the device was registered |
| `last_sync_at` | TEXT | Updated on every successful sync |
| `last_sync_ip` | TEXT | For anomaly detection |
| `status` | TEXT | `active` or `revoked` |

**Relationship to `replicas` table:** The `replicas` table continues to exist as the per-user master encryption identity (stores the master secret for the sync bridge's REST-side operations). Devices derive their individual secrets from this master secret.

### Sync Flow

**Device pushes a version (TW CLI `task sync`):**
1. TW CLI encrypts operations with `PBKDF2(device_secret, salt=device_client_id)`
2. Sends encrypted blob to server via TC sync protocol
3. Server looks up device by `client_id` → gets `user_id` + `encryption_secret_enc`
4. Server decrypts blob using the device's key
5. Server applies mutations to canonical replica (plaintext)

**Device pulls versions:**
1. Server reads pending changes from canonical replica
2. Server encrypts using the requesting device's key
3. Sends encrypted blob via TC sync protocol
4. TW CLI decrypts with its own key

**REST API (iOS app):**
- Reads/writes directly to the canonical replica (no encryption involved)
- iOS app is also registered as a "device" for tracking, but uses bearer token auth, not TC sync

### Device Registration Flow

```
POST /api/devices { "name": "Work MacBook" }
→ Server generates: client_id (UUID v4)
→ Server derives: device_secret = HKDF(master_secret, info=client_id)
→ Server stores: devices row (client_id, encrypted device_secret, etc.)
→ Server ensures: shared sync storage exists
→ Returns: { client_id, encryption_secret } (shown once, like API tokens)

User adds to .taskrc:
  sync.server.url=https://sync.example.com
  sync.server.client_id=<returned client_id>
  sync.encryption_secret=<returned encryption_secret>
```

Registration may be initiated either:

- by an authenticated user via the API, or
- by an operator via the admin CLI on a self-hosted server.

In both cases the server remains authoritative for generating `client_id` and
the per-device secret.

### Provisioning Boundary

The architectural intent is that device provisioning is one shared business
capability, not two duplicated flows.

That means:

- HTTP device registration
- local admin CLI device creation

should converge on one provisioning service boundary that owns:

- precondition checks
- per-device secret derivation
- secret escrow/storage
- shared sync-storage initialization

The CLI and HTTP layers should remain thin surfaces over that shared service
rather than each reimplementing the provisioning steps.

### Device Revocation

```
DELETE /api/devices/{client_id}
→ Server sets status = 'revoked'
→ Server evicts client_id from sync auth cache
→ Next sync attempt from this client_id → 403

No other devices are affected. No secret rotation needed.
```

### Operational UX

This ADR intentionally treats **device lifecycle** and **device onboarding UX**
as separate concerns.

**Self-hosted/manual provisioning is a first-class supported path.**

- The open core server must support device creation, listing, revocation, and
  secret retrieval via the admin CLI.
- Device creation should print a `.taskrc`-compatible snippet so an operator can
  copy values directly into a client.
- Manual entry of `server_url`, `client_id`, and `encryption_secret` is an
  acceptable onboarding flow for self-hosters.

**CLI command surface:**

- `admin sync` manages the user's canonical sync identity (`replicas` table).
- `admin device` manages per-device lifecycle (`devices` table).

**Lifecycle semantics:**

- `create` registers a new device and returns its credentials.
- `revoke` is the default "remove this device" action. It is a soft disable and
  preserves audit history.
- `unrevoke` reverses an accidental or temporary revocation.
- `delete` is destructive cleanup and should not be the default operator action.

Managed-service invite links, short codes, deep links, and QR-based onboarding
are explicitly out of scope for this ADR. They may be layered on top of the same
device registry later; see ADR-0004.

### Implementation Note: Server-Side Sync Storage Shape

During implementation, the server-side storage topology was corrected.

The accepted security and lifecycle outcome remains:

- one `client_id` per device
- one derived secret per device
- per-device revoke without rotating every other client

But the runtime does **not** keep one live TaskChampion sync DB per device.

The server uses:

- one canonical plaintext replica per user
- one shared per-user TaskChampion sync DB
- per-device auth and envelope translation at the HTTP boundary

The reason is practical: a single canonical TaskChampion replica cannot
reliably relay device-originated changes across many independent per-device
server chains and still preserve correct convergence. The shared sync DB model
keeps the per-device security boundary without that broken relay behavior.

Current runtime storage:

```
data/
├── config.sqlite                          # Server config DB
├── users/{user_id}/
│   ├── taskchampion.sqlite3              # Canonical REST replica (plaintext)
│   ├── sync.sqlite                       # Shared TC sync DB
│   └── sync/{client_id}.sqlite           # Optional legacy/maintenance artifact
```

### Secret Derivation

Device secrets are derived from the user's master secret using HKDF:

```
device_secret = HKDF-SHA256(
    ikm = master_secret,          # From replicas.encryption_secret_enc (decrypted)
    salt = client_id.as_bytes(),   # Device's UUID bytes
    info = b"cmdock-device-v1"     # Domain separation
)
```

This ensures:
- Each device gets a unique, deterministic secret
- The server can re-derive any device's secret from the master (for the bridge)
- Compromising one device's secret doesn't reveal the master or other devices' secrets
- HKDF is the standard construction for this (RFC 5869)

### ADR-0002 Compliance

**Independence analysis:**

| Concern | Module | Knows about |
|---------|--------|-------------|
| Device CRUD | `src/devices/` | ConfigStore trait only |
| Device auth | `src/tc_sync/handlers.rs` | Device status + user_id |
| Per-device crypto | `src/tc_sync/crypto.rs` | Key derivation only |
| Per-device storage | `src/tc_sync/storage.rs` | File paths only |
| Sync bridge | `src/sync_bridge.rs` | Orchestrates (designated) |

The sync bridge is a **designated orchestrator** (ADR-0002 exception) — it coordinates device lookup, crypto, storage, and the replica. All other modules remain independent.

**Change locality (HC-3):** Adding a new device requires changes to:
1. `devices/handlers.rs` — registration endpoint
2. `store/sqlite.rs` — device record CRUD
3. Per-device sync storage directory (auto-created)

Revoking a device: 1 file (`devices/handlers.rs`). Meets HC-3.

## Implementation Phases

### Phase 1: Device Registry + Auth (current commit)
- `devices` table + CRUD endpoints
- Sync auth validates `client_id` against devices table
- All devices still share one encryption key (migration bridge)
- **Ship this — solves tracking and auth revocation immediately**

### Phase 2: Per-Device Crypto (this ADR)
- Add `encryption_secret_enc` to devices table
- HKDF secret derivation on device registration
- Per-device sync storage directories
- Sync bridge reads device-specific key for encrypt/decrypt
- `CRYPTOR_CACHE` keyed by `client_id` (not `user_id`)

### Phase 3: Migration Tooling
- `admin device migrate` — migrate existing single-key users to per-device model
- Automated migration on first device registration (create device from existing replica)

## Consequences

### Positive

- **Lost device = revoke + done.** No secret rotation, no reconfiguring other devices.
- **Per-device audit trail.** Know exactly which device synced, when, from where.
- **Blast radius containment.** Compromised device secret can't decrypt other devices' traffic.
- **Clean separation.** Device identity, encryption, and sync storage are independent concerns (connected only at the sync bridge orchestrator).

### Negative

- **Storage overhead.** Each device gets its own sync chain (SQLite file). For N devices, N sync storage files per user. Mitigated: sync chains are small (delta-compressed operations).
- **CPU overhead.** Server decrypts/re-encrypts on every sync. Mitigated: PBKDF2 keys are cached per device; only ChaCha20 seal/unseal per request (~microseconds).
- **Implementation complexity.** The sync bridge becomes more complex (device-aware crypto). Mitigated: it's a designated orchestrator, and per-device storage is simpler than shared-chain re-encryption.
- **Sync latency.** Bridge must process canonical replica ↔ shared sync DB traffic while selecting the correct device envelope at the protocol edge.

### Neutral

- Phase 1 ships without per-device crypto — existing users are unaffected
- iOS app registers as a device but uses bearer auth for REST (not TC sync)
- The `replicas` table continues to store the master secret; devices store derived secrets

## References

- ADR-0001: Sync Bridge Architecture (hub model for REST ↔ TC)
- ADR-0002: Design Simplicity Principles (independence analysis, designated orchestrators)
- RFC 5869: HKDF (HMAC-based Extract-and-Expand Key Derivation Function)
- TaskChampion sync protocol: https://gothenburgbitfactory.org/taskchampion/sync-protocol.html
- Codex + Gemini architecture review (2026-04-01): both recommended Option B given TC constraints
