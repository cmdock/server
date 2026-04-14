# Documentation Library

This documentation set is organised as a small manual library in the style of a traditional server product.

This manual set is the authoritative standalone documentation surface for
`cmdock/server` itself.

## Quick Lookup

Common operator/admin tasks:

- deploy a self-hosted server
  - [Installation and Setup Guide](installation-and-setup-guide.md#docker-compose)

- create users and perform initial operator bootstrap
  - [Installation and Setup Guide](installation-and-setup-guide.md#4-create-your-first-user)
  - [Administration Guide](administration-guide.md#self-hosted-operator-quick-reference)

- install and use the optional standalone operator CLI
  - [Installation and Setup Guide](installation-and-setup-guide.md#optional-standalone-admin-cli)
  - [Administration Guide](administration-guide.md#optional-standalone-admin-cli)

- register, inspect, revoke, unrevoke, or delete devices
  - [Administration Guide](administration-guide.md#self-hosted-operator-quick-reference)
  - [API Reference](api-reference.md#37-devices)

- set up user or admin/per-server webhooks
  - [Administration Guide](administration-guide.md#webhook-operations)
  - [API Reference](api-reference.md#310-webhooks)

- create backups, restore snapshots, and verify recovery
  - [Backup and Recovery Guide](backup-and-recovery-guide.md#42-quick-start-for-the-standard-docker-compose-bundle)
  - [Backup and Recovery Guide](backup-and-recovery-guide.md#9-restore-procedure)

- respond to recovery or corruption notices
  - [Administration Guide](administration-guide.md#startup-recovery-assessment)
  - [Administration Guide](administration-guide.md#recovery-scenarios)
  - [Recovery Reference](../reference/recovery-reference.md)

- respond to low disk space
  - [Administration Guide](administration-guide.md#scenario-4-disk-full)
  - [Metrics and Observability Reference](../reference/metrics-and-observability-reference.md#9-alerting-guidance)

- prepare and verify an upgrade rollout
  - [Administration Guide](administration-guide.md#server-upgrades)
  - [Backup and Recovery Guide](backup-and-recovery-guide.md#12-common-mistakes)

- understand canonical state vs sync state
  - [Concepts Guide](concepts-guide.md#3-canonical-replica-vs-shared-sync-db)

- monitor runtime health, disk headroom, and recovery state
  - [Administration Guide](administration-guide.md#monitoring)
  - [Metrics and Observability Reference](../reference/metrics-and-observability-reference.md#9-alerting-guidance)
  - [Metrics Catalog Reference](../reference/metrics-catalog-reference.md)

- troubleshoot performance or size a host
  - [Performance and Scaling Guide](performance-and-scaling-guide.md#small-self-hoster-baseline)

## Manuals

The primary manuals are:

1. [Concepts Guide](concepts-guide.md)
   Explains the architecture, data model, sync model, device registry, bridge scheduler, and the main runtime boundaries.

2. [Installation and Setup Guide](installation-and-setup-guide.md)
   Covers deployment, initial configuration, reverse proxy setup, data layout, and first-time server bootstrap.

3. [Administration Guide](administration-guide.md)
   Covers day-to-day operator workflows, device lifecycle, monitoring, webhook operations, maintenance, and operational runbooks.

4. [Backup and Recovery Guide](backup-and-recovery-guide.md)
   Covers backup scope, Docker and bare-metal backup quick starts, restore procedures, and post-restore validation.

5. [Performance and Scaling Guide](performance-and-scaling-guide.md)
   Covers load-test profiles, bottleneck analysis, runtime scaling behaviour, and operational sizing notes.

6. [API Reference](api-reference.md)
   Covers the public HTTP surface, auth modes, OpenAPI/Swagger locations, and endpoint groupings.

7. [Developer Guide](developer-guide.md)
   Covers core design boundaries, extension rules, ADR-0002-driven review criteria, and how to add features without regressing the architecture.

## Reference

Supplementary technical references:

- [Disaster Recovery Architecture](../reference/disaster-recovery-reference.md)
- [Filter Expression Engine](../reference/filter-expression-engine-reference.md)
- [Invite Code Onboarding Reference](../reference/invite-code-onboarding-reference.md)
- [Schema and Live Migration Reference](../reference/schema-and-live-migration-reference.md)
- [Schema Development Guidelines](../reference/schema-development-reference.md)
- [Sync Bridge Reference](../reference/sync-bridge-reference.md)
- [Recovery Reference](../reference/recovery-reference.md)
- [Audit Reference](../reference/audit-reference.md)
- [Coding Style Guide](../reference/coding-style-guide.md)
- [API Interaction Flows Reference](../reference/api-interaction-flows-reference.md)
- [Storage Layout Reference](../reference/storage-layout-reference.md)
- [Admin Surfaces Reference](../reference/admin-surfaces-reference.md)
- [Issue Triage Labels Reference](../reference/issue-triage-labels-reference.md)
- [TaskChampion Integration Reference](../reference/taskchampion-integration-reference.md)
- [Metrics and Observability Reference](../reference/metrics-and-observability-reference.md)
- [Metrics Catalog Reference](../reference/metrics-catalog-reference.md)
- [Testing Strategy Reference](../reference/testing-strategy-reference.md)
- [ADR Directory](../adr/)

The split is intentional:

- the manuals tell operators and developers what to do
- the reference docs capture deeper architecture and specialised technical detail

## Implementation Notes

Public technical implementation notes:

- [Per-User Schema Uplift Research Note](../implementation/per-user-schema-uplift-research-note.md)
- [Push-Triggered Mobile Sync Research Note](../implementation/push-triggered-mobile-sync-research-note.md)
- [Geofence Typed API Implementation Note](../implementation/geofence-typed-api-implementation-note.md)
- [Operator Bootstrap API Implementation Note](../implementation/operator-bootstrap-api-implementation-note.md)
- [HTTP DTO Validation With `garde`](../implementation/http-dto-validation-with-garde-note.md)

Suggested reading order for a new operator:

1. [Concepts Guide](concepts-guide.md)
2. [Installation and Setup Guide](installation-and-setup-guide.md)
3. [Administration Guide](administration-guide.md)
4. [Backup and Recovery Guide](backup-and-recovery-guide.md)

Suggested reading order for a developer:

1. [Concepts Guide](concepts-guide.md)
2. [Developer Guide](developer-guide.md)
3. [Coding Style Guide](../reference/coding-style-guide.md)
4. [API Reference](api-reference.md)
5. [Performance and Scaling Guide](performance-and-scaling-guide.md)
6. [ADR Directory](../adr/)
7. [Testing Strategy Reference](../reference/testing-strategy-reference.md)
