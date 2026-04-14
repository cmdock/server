# Admin Surfaces Reference

This document describes the current operator surfaces exposed by the server.

It focuses on server-owned boundaries, not user/operator tutorials. For operational
procedures, see [Administration Guide](../manuals/administration-guide.md).

## 1. Current Admin Surfaces

There are two main admin surfaces today:

- local admin CLI
- admin HTTP endpoints

These are related, but not identical.

The intended layering is:

- `cmdock-admin`
  - the recommended day-to-day operator tool for live systems
- `cmdock-server admin`
  - the retained local self-sufficiency and break-glass/on-host maintenance
    surface

The current policy for what belongs on which surface is defined in
[ADR-0006: Operator Surface Scope](../adr/ADR-0006-operator-surface-scope.md).

## 2. Admin CLI

The admin CLI operates directly on the server data directory and config DB.

Typical uses:

- local user/token management
- sync identity bootstrap
- device provisioning and lifecycle
- CLI-first connect-config generation for QR/deep-link onboarding
- backup / restore
- local recovery work

Characteristics:

- does not require the HTTP server to be running
- acts as a local operator surface
- can be used in break-glass/offline situations
- remains the server's full self-sufficiency fallback even when remote
  operator tooling exists
- still owns the break-glass local connect-config flow
- keeps connect-config generation HTTPS-only; plain-HTTP self-hosted setups
  must use manual transcription instead
- exposes connect-config troubleshooting through the existing token surface:
  first successful authenticated use stamps token usage fields in the config DB,
  and `admin token list` renders `FIRST_USED`, `LAST_USED`, and `LAST_IP`
- is now split internally by concern:
  - `user`
  - `token`
  - `sync`
  - `device`
  - `connect_config`
  - `backup_restore`

Internal boundary note:

- the top-level CLI module is now intended to stay a thin command router
- shared services should carry reusable business rules
- the CLI modules should not grow back into a single coupling hub

## 3. Admin HTTP Endpoints

Current examples:

- `/admin/console`
- `/admin/status`
- `/admin/users`
- `/admin/user/{id}`
- `/admin/user/{id}/stats`
- `/admin/user/{id}/evict`
- `/admin/user/{id}/checkpoint`
- `/admin/user/{id}/offline`
- `/admin/user/{id}/online`

The remote operator HTTP surface now also includes:

- user discovery via `GET /admin/users`
- destructive user removal via `DELETE /admin/user/{id}`
- connect-config issuance via `POST /admin/user/{id}/connect-config`

Characteristics:

- operate against the running process
- can affect in-memory runtime state immediately
- are the natural remote operator surface for trusted automation or browser
  tooling
- are protected by a dedicated operator bearer token configured via
  `[admin].http_token` / `CMDOCK_ADMIN_TOKEN`

Operator console note:

- `/admin/console` is the first server-hosted operator console shell
- it is intended for trusted operator ingress only
- it is not itself the credential boundary
- browser-side calls from that console still use the same operator bearer token
  against the existing `/admin/*` API surface
- the token should be entered locally by the operator and kept only in the
  browser session, not embedded into the page or returned by the server

`/admin/status` is intentionally a small operator summary surface. It now
includes:

- uptime
- cached replica count
- current quarantined-user count
- latest startup recovery summary when the process has already completed boot assessment
- operator-facing process health details such as LLM circuit-breaker state

Important boundary note:

- these endpoints are an **operator** surface, not a normal end-user API
- they should not share the same auth model as ordinary bearer-token user REST traffic

Operator token management note:

- treat the operator bearer token as an environment-scoped secret
- external operator automation may hold that environment's token if it is the
  trusted caller of `/admin/*`
- explicit operator tooling can also use it, for example staging smoke tests
- end-user surfaces must not receive it
  - not native client apps
  - not user-facing web UIs
  - not normal bearer-token clients calling `/api/*`

Hosted deployment note:

- `/admin/*` should be reachable only through trusted operator ingress
- public ingress should stay limited to normal user REST and sync surfaces
- the operator token is a second boundary, not a substitute for ingress
  separation
- forwarded client-IP headers are only meaningful when supplied by trusted
  ingress

The trusted-proxy and forwarded-header assumption is now explicit in
[ADR-0007: Trusted Proxy and Forwarded-Header Model](../adr/ADR-0007-trusted-proxy-and-forwarded-header-model.md).

Operator contract summary:

- `/admin/*` is for trusted operator callers only
- callers must present the environment-scoped operator bearer token
- that token comes from secret management, not from the running server
- end-user clients and public user ingress must never receive or proxy it
- the server's runtime-policy surface stays generic:
  - `runtimeAccess` controls allow vs block
  - `deleteAction` controls destructive delete permission
  - `enforcementState` reports whether desired/applied state is current
- richer deployment-specific lifecycle terms stay outside this repo
  - retention windows
  - legal hold
  - hardening/account-capability rules
  - placement and relocation orchestration

## 4. Why There Are Two Surfaces

This split exists because self-hosting and live operations pull in different
directions.

The local CLI is valuable because:

- it works when the server is stopped
- it works during break-glass repair
- it supports self-hosters without requiring separate operator automation

The HTTP admin surface is valuable because:

- it affects the running process directly
- it is the natural remote surface for `cmdock-admin`, operator automation,
  and web-based admin tools

Current policy:

- grow `/admin/*` to support routine remote operator work
- keep the local admin CLI available for self-hosters who are on-host or
  working in break-glass/offline situations
- do not treat remote operator growth as a reason to deprecate the local CLI

## 5. Current Boundary

Roughly:

- local filesystem / config mutation belongs naturally to the CLI
- live in-memory process control belongs naturally to admin HTTP

The new offline marker narrows that gap because the CLI can now coordinate with
a running server through persisted per-user state.

The current auth boundary now matches the control boundary:

- user REST auth for end-user operations
- operator HTTP auth for `/admin/*`
- local CLI for offline/break-glass operator work

For connect-config onboarding specifically:

- generation is a CLI-local operator action
- success is proven by first successful authenticated use of the emitted
  short-lived token, not by a separate special-purpose endpoint
- the normal post-scan verification call should be `GET /api/me`

## 6. Recovery-Specific Surfaces

### CLI

- `admin user offline`
- `admin user assess`
- `admin user online`
- `admin restore --user-id`

### HTTP

- `POST /admin/user/{id}/offline`
- `POST /admin/user/{id}/online`
- `GET /admin/user/{id}/stats`

The CLI is the stronger self-hosted recovery surface today because it can both:

- manipulate on-disk state
- coordinate with the running server using the offline marker

## 7. Sync / Device Surfaces

The admin model intentionally separates:

- `admin sync`
  - canonical per-user sync identity
- `admin device`
  - per-device lifecycle
- `admin connect-config`
  - short-lived delivery artifact for app onboarding, built on top of the
    existing device credential model rather than a separate credential family
- operator HTTP runtime policy
  - generic desired/applied runtime-access policy for a target user
- operator HTTP bootstrap
  - thin remote operator bootstrap over the shared sync-identity and
    device services
- operator HTTP sync identity
  - show or ensure the canonical sync identity for a target user
- operator HTTP device lifecycle
  - list/create/show/rename/revoke/unrevoke/delete on target-user devices

That mirrors the runtime model:

- one canonical sync identity per user
- one device record per physical client
- one optional short-lived connect-config token used only to deliver a device
  credential into an onboarding flow
- one optional per-user runtime-policy record controlling runtime access and
  delete permission

Implementation note:

- canonical sync identity handling now has its own shared service boundary in
  `src/sync_identity.rs` and `src/admin/services/sync_identity.rs`
- device provisioning and device lifecycle now use the shared device service
- operator bootstrap orchestration now lives in
  `src/admin/services/bootstrap.rs`
- this keeps remote-admin growth from immediately duplicating the most
  security-sensitive bootstrap logic and per-device lifecycle rules

Recovery follows the same pattern:

- low-level runtime mechanics in `src/runtime_recovery.rs`
- operator-facing orchestration in `src/admin/services/recovery.rs`

Runtime-policy enforcement boundary:

- runtime-policy is separate from recovery `offline` / `online`
- `offline` is a recovery and maintenance control
- runtime-policy is the generic operator contract for runtime allow/block and
  delete permission
- no runtime-policy record means the user is unmanaged and normal runtime access
  is allowed
- once a runtime-policy record exists, stale or missing applied state fails
  closed for normal bearer-token and TaskChampion sync traffic
- bearer auth and TaskChampion sync auth now share the same runtime-access
  enforcement helper instead of hand-rolling parallel runtime-policy checks
- the same applied runtime-policy gate now blocks new device provisioning across
  operator bootstrap, operator device create, and admin CLI device create, so
  those paths cannot bypass the runtime policy

Current state:

- the generic runtime-policy contract is implemented
- runtime auth and device provisioning now enforce it consistently
- explicit placement-relocation cutover remains future work and belongs to a
  separate follow-up concern rather than this runtime-policy boundary

## 8. Delete vs Revoke

Current semantic split:

- `revoke`
  - soft disable
  - security / operational control
- `delete`
  - destructive cleanup
  - currently treated as an operator/admin action

This is important when reasoning about future remote operator UI behaviour. The server
does not currently treat live device delete as a normal self-service action.

## 9. Future Direction

The likely future direction is:

- keep local CLI for self-hosted and break-glass use
- formalise admin HTTP as a distinct remote operator surface
- let a future remote operator UI use the HTTP/admin side, not direct file
  access

That does not mean the CLI should disappear.

### Surface policy summary

- **CLI-first and likely to remain CLI-first**
  - backup
  - restore
  - selective restore
  - break-glass filesystem repair
- **Shared operator services now, remote candidates later**
  - recovery assessment
  - offline / online transitions
  - canonical sync identity lifecycle
  - device lifecycle
- **Already live as operator HTTP**
  - status
  - stats
  - evict
  - checkpoint
  - offline / online

This keeps dangerous filesystem-oriented work out of the running-process HTTP
surface by default.

## 10. Non-Goals

This document does not define:

- final remote-operator admin API shape
- RBAC / team-admin model
- invite/link/QR onboarding UX in full product detail
- broader product-layer orchestration outside the server runtime

Those belong to future operator or product-layer design work rather than this
server-owned surface reference.
