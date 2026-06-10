---
created: 2026-04-03
status: accepted
tags: [architecture, security, ingress, proxy, audit, hosted]
---

# ADR-0007: Trusted Proxy and Forwarded-Header Model

## Status

**Accepted**

## Relationship To Architecture ADRs

This ADR is a repo-local application of:

- `cmdock/architecture ADR-0003: Logical Environment And Origin Separation`
- `cmdock/architecture ADR-0005: Open-Core And Managed-Service Boundary`

It defines the server-side deployment and trust assumptions that remain
platform-neutral while fitting the broader cmdock environment model.

## Context

`cmdock-server` binds plain HTTP and is designed to sit behind a reverse proxy
or load balancer in normal deployment shapes.

That has already been operationally true, but the security boundary was only
partially implicit in the manuals:

- hosted deployments expect a trusted ingress layer
- `/admin/*` is an operator/control-plane surface
- audit and auth diagnostics may depend on forwarded client-IP headers

Without an explicit ADR, there is room for dangerous deployment drift:

- the server might be made directly reachable while also receiving proxied
  traffic
- forwarded headers might be treated as trustworthy even when they come from
  untrusted clients
- hosted environments might conflate public user ingress with trusted
  operator/control-plane ingress

That would weaken both:

- the operator-surface boundary from ADR-0005
- the hosted control-plane boundary from ADR-0004

## Decision

`cmdock-server` adopts a **trusted-proxy deployment model** for hosted and
enterprise environments.

### Core assumption

In hosted deployments, the server is expected to run behind a trusted ingress
layer such as:

- reverse proxy
- load balancer
- API gateway
- internal edge tier

The ADR is platform-neutral. It does not require a specific cloud provider or
proxy product.

### Forwarded-header trust rule

Forwarded headers are only trustworthy when they are added by that trusted
ingress layer.

In particular:

- forwarded client-IP headers are not a general-purpose end-user input
- they should be relied on only when the deployment ensures they come from
  trusted ingress
- deployment shapes that allow direct client reachability in parallel with
  proxy-reachable traffic are not an acceptable hosted-security baseline if
  audit or policy decisions depend on forwarded headers

### Hosted ingress split

Hosted deployments should keep two ingress classes distinct:

- public user ingress
  - normal REST and sync traffic
- trusted operator/control-plane ingress
  - `/admin/*`

`/admin/*` is not a public user API and should not be reachable from normal
public user ingress by default.

### Operator auth remains required

Trusted ingress is not a substitute for operator authentication.

`/admin/*` still requires the environment-scoped operator bearer token from
ADR-0005:

- configured via `[admin].http_token`
- or injected as `CMDOCK_ADMIN_TOKEN`

### What this ADR does not require

This ADR does not require:

- a specific vendor ingress product
- direct TLS termination inside the Rust server
- richer RBAC
- automatic proxy discovery
- hard-coded IP allowlists in the open-core repo

Those are deployment-specific concerns.

## Consequences

### Positive

- The hosted-security boundary becomes explicit instead of folklore.
- `/admin/*` now has both an auth boundary and an ingress boundary.
- Audit/IP semantics are easier to reason about.
- Control-plane integration guidance is clearer for hosted environments.

### Negative

- Hosted operators must actually enforce ingress separation outside the Rust
  process.
- Some deployment shapes that are convenient for testing are explicitly not
  acceptable as a hosted-security baseline.

### Neutral

- Self-hosted operators can still run the server simply behind Caddy, nginx, or
  another reverse proxy.
- The runtime now defaults to ignoring forwarded client-IP headers unless
  forwarded-header trust is enabled explicitly in server configuration.

## Implementation Notes

This ADR should be reflected in:

- deploy docs
- administration/security docs
- admin/control-surface references
- any future hosted control-plane integration docs
- the server's forwarded-header trust toggle

The server repo should continue to stay platform-neutral while making the
security assumptions explicit.

## Non-Goals

This ADR does not define:

- cloud-specific network topology
- a hosted control-plane product architecture
- end-user auth or RBAC
- a full reverse-proxy configuration matrix
