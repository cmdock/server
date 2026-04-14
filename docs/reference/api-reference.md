# API Reference

This guide is the operator and developer reference for the public HTTP surface exposed by `cmdock-server`.

Interactive and machine-readable reference:

- Swagger UI: `/swagger-ui/`
- OpenAPI JSON: `/api-doc/openapi.json`
- local export: `just openapi-export`

This document is intentionally high level. It explains the endpoint groups, auth model, and compatibility expectations, while the generated OpenAPI document remains the protocol-level reference.

Recent operator-management additions:

- `GET /admin/users` for operator user discovery
- `DELETE /admin/user/{id}` for destructive user removal
- `/api/webhooks` for user-scoped webhook CRUD, recent delivery history, and
  test delivery
- `/admin/webhooks` for admin/per-server webhook CRUD, delivery history, and
  enable/disable/test operations

For request sequencing and state-touch diagrams, see
[API Interaction Flows Reference](../reference/api-interaction-flows-reference.md).

See also:

- [Concepts Guide](concepts-guide.md)
- [Administration Guide](administration-guide.md)
- [API Interaction Flows Reference](../reference/api-interaction-flows-reference.md)
- [Documentation Library](index.md)

## Validation And Error Shape

Request parsing and lightweight validation happen at the HTTP boundary.

Current rule:

- prefer typed extraction first
- reject malformed or invalid request payloads early
- keep the public error shape simple unless an endpoint explicitly documents
  something richer

For most user-facing endpoints today, validation failures still surface as
simple `400 Bad Request` responses rather than a shared JSON validation
envelope.

## 1. Authentication Modes

The server currently exposes two auth models.

### 1.1 Bearer Token Auth

Used by most REST endpoints.

- header: `Authorization: Bearer <token>`
- scoped to one user
- backed by the config database

### 1.2 Operator HTTP Auth

Used by `/admin/*` endpoints.

- header: `Authorization: Bearer <operator-token>`
- configured via `[admin].http_token` or `CMDOCK_ADMIN_TOKEN`
- separate from user API tokens stored in the config database
- intended for operators, not end users

Environment-management rule:

- manage one operator token per environment (`staging`, `prod`, and so on)
- store it in infra/runtime secret management, not in the repo
- provide it only to trusted operator tooling and explicit operator smoke
  tests
- do not give it to end-user clients, user-facing web apps, or
  other ordinary bearer-token consumers

### 1.3 Device Sync Auth

Used by TaskChampion sync endpoints.

- header: `X-Client-Id: <device-client-id>`
- mapped through the device registry
- revoked devices are rejected

## 2. Unauthenticated Endpoints

These endpoints do not require auth:

- `GET /healthz`
- `GET /metrics`
- `GET /swagger-ui/`
- `GET /api-doc/openapi.json`

## 3. Endpoint Groups

### 3.1 Health

- `GET /healthz`

Returns service health and a lightweight pending-task summary.

### 3.2 Tasks

- `GET /api/tasks`
- `POST /api/tasks`
- `POST /api/tasks/{uuid}/done`
- `POST /api/tasks/{uuid}/undo`
- `POST /api/tasks/{uuid}/delete`
- `POST /api/tasks/{uuid}/modify`

This is the main REST task surface used by bearer-token client applications.

Default list semantics:

- omitting `view` returns the normal pending-task list
- sending `view=` with an empty value is treated the same as omitting `view`
- deleted tasks are not part of that default list
- each task item now includes computed `blocked` and `waiting` booleans
- builtin `duesoon` and `action` views exclude blocked and waiting tasks, while
  named-context and custom views keep those tasks visible and expose the
  computed flags

Modify semantics:

- `POST /api/tasks/{uuid}/modify` can replace the task dependency set through a
  `depends` array of task UUIDs
- send an empty `depends` array to clear all dependencies

### 3.3 Views

- `GET /api/views`
- `PUT /api/views/{id}`
- `DELETE /api/views/{id}`

### 3.4 App Config, Geofences, and Legacy Config

- `GET /api/app-config`
- `PUT /api/shopping-config`
- `DELETE /api/shopping-config`
- `GET /api/geofences`
- `PUT /api/geofences/{id}`
- `DELETE /api/geofences/{id}`
- context, store, and preset CRUD endpoints
- legacy generic config endpoints under `/api/config/...` for older clients
  that have not moved to typed resources

Retention note:

- do not add new product capability only to the generic config surface
- keep it only while supported clients still depend on it

Geofence `PUT` is a full typed upsert, not a partial patch.
If the client omits optional fields such as `radius` or `type`, the server
applies its defaults for those fields.

### 3.5 Summary

- `GET /api/summary`

Reserved summary endpoint. It is not part of the recommended public surface yet.

### 3.6 Devices

- `GET /api/devices`
- `POST /api/devices`
- `PATCH /api/devices/{client_id}`
- `DELETE /api/devices/{client_id}`

This is the authenticated user-facing device registry surface.

Current semantics:

- `POST` registers a new device and returns per-device sync credentials
- device registration now requires a configured public server URL via
  `[server].public_base_url` or `CMDOCK_PUBLIC_BASE_URL`
- if a current applied runtime policy sets `runtimeAccess = block`, device
  provisioning is rejected across self-service, operator, bootstrap, and CLI
  flows using the same runtime-policy enforcement model
- if a runtime-policy record exists but desired/applied state is missing or
  stale, provisioning fails closed
- `DELETE` revokes the device rather than hard-deleting it

### 3.7 Sync Placeholder

- `POST /api/sync`

This is a legacy no-op compatibility surface. The server itself is the source
of truth for REST clients.

Retention note:

- keep this endpoint only while supported clients still call it
- do not build new runtime behavior on top of it

### 3.8 Runtime Identity

- `GET /api/me`

Returns the minimal authenticated core runtime identity for the bearer token:

- `id`
- `username`
- `createdAt`

This endpoint is intentionally narrow. It answers "who is this bearer token
authenticated as in the core task runtime?" and does not expand into broader
profile, entitlement, onboarding, or session state.

Current preference boundary:

- client-local first:
  locale/timezone/display preferences, first-day-of-week, default view, local
  composer/capture defaults, density and similar presentation settings
- server-backed later, only if they need to roam with the authenticated user:
  portable behavioural defaults, summary/privacy choices, and other genuine
  cross-device account state

### 3.8A Onboarding Surface Note

The current onboarding/runtime-delivery model is intentionally split:

- canonical sync identity
  - one per user
  - server-side runtime anchor
- device credential
  - one per physical client
  - created by device provisioning flows
- connect-config
  - short-lived delivery artifact that packages an onboarding credential into a
    QR or deep-link flow

This keeps connect-config as a delivery mechanism rather than a second
credential family.

### 3.9 TaskChampion Sync Protocol

- `POST /v1/client/add-version/{parent}`
- `GET /v1/client/get-child-version/{parent}`
- `POST /v1/client/add-snapshot/{version}`
- `GET /v1/client/snapshot`

These endpoints implement the TaskChampion sync protocol used by Taskwarrior-compatible clients.

### 3.10 Webhooks

- `GET /api/webhooks`
- `POST /api/webhooks`
- `GET /api/webhooks/{webhook_id}/deliveries`
- `POST /api/webhooks/{webhook_id}/test`
- `PATCH /api/webhooks/{webhook_id}`
- `DELETE /api/webhooks/{webhook_id}`

User-scoped webhooks let an authenticated user receive task lifecycle events
for their own account.

Current event families include:

- `task.created`
- `task.modified`
- `task.completed`
- `task.deleted`
- `task.due`
- `task.overdue`
- `sync.completed`

The server signs delivery attempts with the configured shared secret, records
delivery attempts, and retries failed deliveries with bounded backoff. The
delivery history endpoint is the supported way to inspect recent success/failure
for one webhook.

Admin/per-server webhooks live on the operator surface:

- `GET /admin/webhooks`
- `POST /admin/webhooks`
- `GET /admin/webhooks/{webhook_id}/deliveries`
- `POST /admin/webhooks/{webhook_id}/test`
- `POST /admin/webhooks/{webhook_id}/enable`
- `POST /admin/webhooks/{webhook_id}/disable`
- `DELETE /admin/webhooks/{webhook_id}`

Use these when the operator wants environment-wide event delivery rather than a
single user's webhook configuration.

### 3.11 Admin HTTP Endpoints

- `GET /admin/console`
  - serve the operator console shell for trusted operator ingress
- `GET /admin/users`
  - list users for operator discovery and admin tooling
- `DELETE /admin/user/{id}`
  - destructive whole-user removal, subject to runtime-policy safeguards
- `POST /admin/bootstrap/user-device`
  - operator bootstrap flow that resolves or creates a user, ensures canonical sync identity, provisions a per-device credential, and returns client-ready sync config
- `POST /admin/bootstrap/{bootstrap_request_id}/ack`
  - marks a delivered bootstrap credential as acknowledged by the operator workflow that requested it
- `GET /admin/user/{id}/sync-identity`
  - show the target user's canonical sync identity
- `POST /admin/user/{id}/sync-identity/ensure`
  - create the canonical sync identity if missing, otherwise return the existing identity
- `GET /admin/user/{id}/runtime-policy`
  - read back the target user's desired/applied runtime-policy state and current enforcement status (`unmanaged`, `current`, `missing_applied`, `stale_applied`)
- `PUT /admin/user/{id}/runtime-policy`
  - apply a generic per-user runtime policy version and mark the same version as applied for immediate readback
- `GET /admin/user/{id}/devices`
  - list devices for the target user, including bootstrap lifecycle state when present
- `POST /admin/user/{id}/devices`
  - create a new per-device credential for the target user and return client-ready sync config
- `GET /admin/user/{id}/devices/{client_id}`
  - show a single device record for the target user
- `PATCH /admin/user/{id}/devices/{client_id}`
  - rename a target-user device
- `POST /admin/user/{id}/devices/{client_id}/revoke`
- `POST /admin/user/{id}/devices/{client_id}/unrevoke`
- `DELETE /admin/user/{id}/devices/{client_id}`
  - delete a revoked device record permanently
- `GET /admin/status`
  - operator diagnostics including uptime, cached replicas, current quarantined-user count, and latest startup recovery summary when available
- `GET /admin/user/{id}/stats`
  - includes replica/cache diagnostics, integrity-check output, and recovery assessment (`healthy`, `rebuildable`, or `needs_operator_attention`)
- `POST /admin/user/{id}/evict`
- `POST /admin/user/{id}/checkpoint`
- `POST /admin/user/{id}/offline`
- `POST /admin/user/{id}/online`

These are server-operator endpoints rather than end-user endpoints.
They use the separate operator token described above, not normal user bearer
tokens.

The generated OpenAPI document now includes the operator bootstrap, sync
identity, runtime-policy, and per-user device lifecycle surfaces with a separate
`operatorBearer` security scheme. Those operator schemas now include concrete
examples plus UUID, enum, and RFC3339 timestamp hints to support generated
operator clients more safely. The generated spec now also includes the
diagnostic and recovery endpoints under `/admin/status` and the broader
`/admin/user/*` operator surface.

Operationally, these admin endpoints still split into two kinds:

- routine operator endpoints:
  `/admin/bootstrap/*`, `/admin/user/{id}/sync-identity*`,
  `/admin/user/{id}/runtime-policy`,
  `/admin/user/{id}/devices*`, `/admin/status`, `/admin/user/{id}/stats`
- break-glass / self-hosted recovery endpoints:
  `/admin/user/{id}/evict`, `/admin/user/{id}/checkpoint`,
  `/admin/user/{id}/offline`, `/admin/user/{id}/online`

Those recovery-oriented endpoints are now emitted in OpenAPI for completeness,
but they remain operator maintenance tools rather than normal end-user flows.
See the [Administration Guide](administration-guide.md) for operational context.

Runtime-policy note:

- no runtime-policy record means `unmanaged`, and normal self-hosted runtime
  access continues
- if a record exists but desired/applied state is missing or stale, the server
  fails closed for normal bearer-token and TaskChampion sync runtime access
- if the current applied policy sets `runtimeAccess = block`, bearer-token and
  TaskChampion sync access return a runtime-policy rejection without deleting
  the user's tokens or devices, and new device provisioning is also rejected
  across `/api/devices`, operator device create, bootstrap, and admin CLI
- if the current applied policy sets `deleteAction = forbid`, destructive user
  deletion is rejected explicitly until the applied policy changes

Operator contract note:

- `/admin/*` is a server-operator surface even when an external orchestrator is
  the caller
- `/admin/console` is only an operator-facing shell and does not replace the
  operator token boundary
- the caller must use the environment-scoped operator token
- public user clients must never receive that token
- the runtime-policy wire model is intentionally generic and does not encode
  deployment-specific lifecycle vocabulary such as retention windows, legal
  hold, or placement rules
- if you need exact request and response shapes for generated operator clients,
  prefer exporting the OpenAPI document from the checked-out repo state

## 4. Compatibility Notes

Important compatibility details:

- task mutation routes use `POST` rather than REST-pure verbs for legacy client compatibility
- dates use Taskwarrior format where required
- some older response shapes are intentionally preserved for compatibility
- error responses are generally plain text rather than fully structured JSON error envelopes

## 5. Where to Look for Precision

Use this guide for:

- endpoint discovery
- auth model understanding
- surface-area orientation

Use the OpenAPI document for:

- exact request and response shapes
- schema details
- interactive testing
- typed client generation

## 6. Local OpenAPI Export

For downstream typed-client generation, prefer exporting the spec locally from
the current repo state instead of scraping a running server.

Supported workflows:

- `just openapi-print`
- `just openapi-export`
- `cargo run --bin cmdock-server -- openapi`
- `cargo run --bin cmdock-server -- openapi --output target/openapi/cmdock-server-openapi.json`

This makes typed-client generation deterministic:

- the spec comes from the exact checked-out server code
- the export does not depend on a running environment
- generated clients can be pinned to a specific server commit
