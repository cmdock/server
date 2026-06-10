# Issue Triage Labels Reference

This document defines the recommended issue-label workflow for `cmdock-server`.

It exists to support product-owner triage and architecture review against:

- [ADR-0002: Design Simplicity Principles](../adr/ADR-0002-design-simplicity-principles.md)
- [Developer Guide](../manuals/developer-guide.md)

The main goal is to stop architectural drift early rather than after
implementation has already started.

## 1. Why Use Labels

Yes, the repo should use issue labels.

For this repo, labels are useful for three things:

- making issue state visible
- making ownership visible
- showing whether an issue is safe to implement without boundary cleanup first

Issue labels should be used as workflow state, not as decorative metadata.

## 2. Use Scoped Labels

Prefer **scoped labels** with a `scope/value` shape such as:

- `status/ready-to-implement`
- `team/server`
- `type/feature`

Scoped labels are better than flat labels because they:

- keep the issue state easier to scan
- reduce label sprawl
- make mutually exclusive categories clearer

The important workflow scope is `status/`.

## 3. Do Not Use A Bare `triaged` Label

A plain `triaged` label is too vague.

It answers:

- someone looked at this

but it does **not** answer:

- is it ready for implementation?
- does it need refinement?
- does it need rerouting to another team?
- does it need an ADR or boundary decision first?

That usually turns `triaged` into a graveyard state.

Prefer:

- `status/needs-triage`
- `status/needs-refinement`
- `status/ready-to-implement`
- `status/blocked`

If an issue has been reviewed, its next state should be explicit.

## 4. Recommended Minimum Taxonomy

Keep the initial label set small.

### 4.1 Status

Use these as the main workflow states:

- `status/needs-triage`
  - new issue, not yet reviewed against repo boundaries
- `status/needs-refinement`
  - reviewed, but scope, ownership, or acceptance criteria still need work
- `status/ready-to-implement`
  - reviewed and safe to hand to engineering
- `status/blocked`
  - cannot proceed due to an external dependency or unresolved decision

### 4.2 Team

Use these to show who owns the next meaningful action:

- `team/server`
- `team/ios`
- `team/control-plane`
- `team/user-pwa`

### 4.3 Type

Use a small type set only:

- `type/feature`
- `type/bug`
- `type/tech-debt`

### 4.4 Coordination / Architecture Flags

Use a very small number of extra flags:

- `coordination/multi-team`
  - issue spans more than one repo or team and likely needs splitting or explicit sequencing
- `arch/adr-required`
  - issue changes a long-lived boundary or is likely to cause architectural drift unless an ADR or equivalent design decision is written first

## 5. What `ready-to-implement` Means

`status/ready-to-implement` should have a strict meaning.

An issue is ready only when:

- the owning surface is clear
- the owning team is clear
- the issue does not quietly blur operator, user, sync, CLI, or runtime boundaries
- the acceptance criteria fit the server's open-core scope
- any required split with `ios`, `control-plane`, or `user-pwa` has already happened
- the issue does not require a missing ADR-level decision

For this repo, "ready" implicitly means "reviewed against ADR-0002 and the
current surface boundaries."

## 6. Lightweight Workflow

The workflow should stay simple.

### 6.1 New Issue

When a new issue arrives:

- add `status/needs-triage`
- add one `type/*` label if obvious

### 6.2 Product-Owner / Architecture Gate Review

Review the issue through these questions:

- which surface owns this change?
- does it belong in `cmdock-server` at all?
- does it preserve independence and change locality?
- does it introduce product UX or orchestration concerns that belong outside the open-core server?
- does it really belong to `server`, or to `ios`, `control-plane`, or `user-pwa`?

Then move it to one of these outcomes:

- `status/ready-to-implement`
  - use when the issue is local, coherent, and properly bounded
- `status/needs-refinement`
  - use when scope or acceptance criteria need rewriting
- `status/blocked`
  - use when the issue is waiting on another decision or dependency

Add:

- one `team/*` label for the next owner
- `coordination/multi-team` when the issue spans more than one team
- `arch/adr-required` when the issue needs an explicit architecture decision first

### 6.3 Multi-Team Or Wrong-Repo Issues

If an issue spans multiple teams, do not leave it as one vague omnibus issue.

Instead:

- split it into smaller repo-appropriate issues where possible
- keep the server issue focused on server primitives and boundaries
- move UX, orchestration, or client-only acceptance criteria into the relevant sister-team repo

This is especially important for:

- `ios-app`
- `control-plane`
- `user-pwa`

### 6.4 Engineering Start Rule

Engineers should normally only start implementation from issues marked:

- `status/ready-to-implement`

That keeps architecture review in front of implementation instead of behind it.

## 7. Boundary Guidance For Triage

When triaging, apply the current architecture boundaries directly.

Good server-owned issue shapes:

- narrow REST or operator HTTP primitives
- storage/model changes that stay inside server concerns
- runtime coordinator work owned by the server
- docs, tests, OpenAPI, audit, and metrics changes that follow a coherent server behaviour change

Suspicious issue shapes:

- hosted onboarding UX mixed into core server runtime work
- billing, subscriptions, or control-plane orchestration bundled into unrelated API changes
- client presentation settings pushed into the server without a roaming-state reason
- one issue that simultaneously changes server, mobile UX, and hosted-control-plane workflow

If an issue starts to mix those concerns, it usually belongs in
`status/needs-refinement`.

## 8. Pitfalls To Avoid

Avoid these label-design mistakes:

- too many labels in v1
  - start small and add only when there is repeated evidence a new label is needed
- a vague `triaged` state
  - it obscures the actual next action
- using labels as priority soup
  - labels should primarily describe workflow state and ownership, not become an ad hoc ranking system
- leaving "ready" undefined
  - document and enforce what readiness means
- keeping cross-team issues unsplit
  - `coordination/multi-team` should trigger decomposition, not become a parking label
- letting labels replace written review comments
  - labels show state; comments should explain the architectural reasoning

## 9. Suggested Initial Set

If the repo starts from zero, the recommended first set is:

- `status/needs-triage`
- `status/needs-refinement`
- `status/ready-to-implement`
- `status/blocked`
- `team/server`
- `team/ios`
- `team/control-plane`
- `team/user-pwa`
- `type/feature`
- `type/bug`
- `type/tech-debt`
- `coordination/multi-team`
- `arch/adr-required`

This is enough to support triage, routing, and architecture governance without
creating label clutter.
