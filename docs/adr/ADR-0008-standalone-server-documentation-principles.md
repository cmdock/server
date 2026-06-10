---
created: 2026-04-05
status: accepted
tags: [architecture, documentation, open-core, boundaries]
---

# ADR-0008: Standalone Server Documentation Principles

## Status

**Accepted**

## Relationship To Architecture ADRs

This ADR is a repo-local application of:

- `cmdock/architecture ADR-0001: Simplicity As A Cross-Repo Principle`
- `cmdock/architecture ADR-0002: Implementation-Agnostic Boundaries`
- `cmdock/architecture ADR-0005: Open-Core And Managed-Service Boundary`

It defines how those shared principles apply to the documentation owned by
`cmdock/server`.

## Context

`cmdock/server` is the standalone open-core runtime server. Its documentation
set is the primary source of truth for:

- installation
- configuration
- operation
- troubleshooting
- public and operator-facing server contracts

As the server grows features that are also useful to the managed service, the
docs can drift in two failure modes:

1. they start explaining features in terms of the wider cmdock ecosystem rather
   than the server itself
2. they require the reader to understand private or sibling repos in order to
   understand what the server does

That is a documentation boundary problem as much as an architecture one.

A self-hoster reading the server repo should never feel like they are reading
internal notes for someone else's product.

## Decision

`cmdock/server` documentation must remain **standalone, generic, and complete
for self-hosters**.

This means a self-hoster should be able to read the server docs and fully
understand how to install, configure, operate, and troubleshoot the server
without needing to read:

- `cmdock/control-plane`
- `cmdock/ops`
- private managed-service design notes
- architecture contracts that are not already reflected locally

The server docs may mention the wider cmdock ecosystem, but they must not rely
on it for comprehension of the server itself.

## Repo-Specific Interpretation

Within `cmdock/server`, this principle means:

- feature docs explain the server feature itself, not the product-program
  motivation that led to it
- API docs describe generic server contracts and roles, not one current caller
- configuration docs explain how an operator can evaluate a setting in generic
  terms
- server-owned manuals remain sufficient for self-hosted deployment and
  operation on their own
- private or sibling repos may be referenced for broader hosted orchestration
  context, but never as required reading for basic server understanding

Documentation simplicity in this repo therefore means:

- readers can understand a feature without hidden system knowledge
- terms remain role-based rather than implementation-specific
- the repo does not document private managed-service behavior as if it were the
  canonical identity of the server

## Repo-Specific Rules

### 1. Feature Docs Explain The Feature, Not The Ecosystem Motivation

When documenting a server feature:

- explain what the feature does
- explain how an operator or client uses it
- explain its server-local constraints and failure modes

Avoid framing such as:

- "this exists so the managed service can later..."
- "the control plane uses this to..."
- "this was added for hosted..."

Those may be historically true, but they are not the primary documentation
contract for this repo.

Preferred framing:

- "generate a QR code to configure a client"
- "create a short-lived onboarding token"
- "emit boundary events for observability correlation"

### 2. API Docs Describe Generic Contracts

API docs should describe:

- the endpoint's role
- the request and response contract
- the auth model
- the operational meaning

They should not define an endpoint by one current consumer.

Preferred language:

- `operator`
- `client`
- `bearer token`
- `runtime policy`

Avoid consumer-specific identity when generic language is sufficient, for
example:

- "the endpoint the control plane calls"
- "the iOS bootstrap endpoint"
- "the managed-service admin flow"

### 3. Configuration Docs Must Be Self-Contained

If a configuration value exists because a managed-service use case helped drive
it, the server docs must still explain it in generic operator terms:

- what the setting does
- when to enable it
- when not to enable it
- how a self-hoster can reason about it

A self-hoster should not need private context to decide whether a config option
applies to their deployment.

### 4. Cross-References Go Outward, Not Inward

Server docs may point outward for broader context, for example:

- hosted orchestration details
- private environment wiring
- wider cross-repo architecture

But those references must be optional.

The server docs must already stand on their own before the outward reference is
added.

Acceptable pattern:

- "For managed-service orchestration details, see `cmdock/control-plane`."

Unacceptable pattern:

- "See `cmdock/control-plane` to understand how this server feature works."

### 5. Terminology Must Stay Generic

Prefer role-based terminology that remains valid if the current consumer
changes:

- `operator`, not `control-plane admin`
- `client`, not `iOS app`
- `auth token`, `device credential`, or `connect-config token`, not
  consumer-specific provisioning slang
- `operator API`, not `control-plane API`

The goal is not to hide real consumers. The goal is to keep the server's
identity independent of any one of them.

### 6. Local Docs Must Reflect Shared Contracts Locally

If the server implements a shared cross-repo contract, the local docs must
state the server-side behavior in standalone terms.

It is acceptable to reference the parent contract, but the reader should still
be able to answer:

- what the server emits
- what the server expects
- what an operator sees locally

without leaving this repo.

### 7. Managed-Service Semantics Stay Out Unless They Are Truly Generic

Do not import private hosted concepts into server docs unless they are
expressed here as generic runtime behavior.

Examples:

- document `runtimeAccess = block`, not hosted billing lifecycle narratives
- document generic operator bootstrap/device flows, not private invite/product
  choreography
- document observability events and correlation fields, not one proprietary log
  pipeline

## Explicit Exceptions

The following are acceptable exceptions when handled carefully:

### 1. Optional Outward References

Server docs may link to:

- `cmdock/architecture`
- `cmdock/control-plane`
- `cmdock/ops`

when the purpose is additional context rather than required comprehension.

### 2. Implementation And ADR Notes

Implementation notes and ADRs may discuss:

- why a feature was introduced
- what cross-repo pressure motivated it
- where the wider system also uses it

But even there, the server-owned behavior must still be described in server
terms, not delegated away.

### 3. Consumer-Specific Examples When They Are Clearly Examples

It is acceptable to mention a real consumer when the text is obviously an
example rather than the definition of the contract, for example:

- "the iOS app is one client of this endpoint"
- "the control plane may call this operator API in hosted environments"

Those examples must not become the only way the docs explain the feature.

## Consequences

### Positive

- self-hosters can understand and operate the server without private repos
- server docs stay aligned with the open-core boundary
- cross-repo features can still land without turning server docs into hosted
  product notes
- the docs remain stable even if the current consumers change

### Negative

- authors must spend more effort translating implementation history into
  generic server language
- some cross-repo context will need to be documented twice:
  once generically here, once in richer private orchestration docs elsewhere

### Neutral

- this ADR does not hide the existence of the managed service
- it does not forbid outward references
- it does not require duplicating all architecture documentation locally

## Non-Goals

This ADR does not require:

- removing all mentions of the managed service from the server repo
- duplicating private orchestration docs into this repo
- refusing generic features that the managed service also consumes

It only requires that server documentation remain complete and intelligible on
its own terms.

## Review Lens

When writing or reviewing server docs, ask:

1. Could a self-hoster understand this without reading another cmdock repo?
2. Does this text describe the server's role, or one current consumer?
3. If the managed service disappeared tomorrow, would this documentation still
   read naturally?
