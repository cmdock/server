# Geofence Typed API Implementation Note

Updated: 2026-04-02

Related:

- [API Reference](../reference/api-reference.md)
- [ADR-0002: Design Simplicity Principles](../adr/ADR-0002-design-simplicity-principles.md)
- the original geofence typed-API implementation issue in this repository

## Purpose

This note describes the implementation that promoted geofences from the generic
config API to a typed server resource.

The goal is to make geofences a clear first-class concern for first-party
clients without dragging the generic config surface forward longer than needed.

This is now a record of the implemented direction.

## Result

Geofences are now exposed as a typed resource:

- `GET /api/geofences`
- `PUT /api/geofences/{id}`
- `DELETE /api/geofences/{id}`

The aggregate app-config endpoint still includes `geofences`, but it now reads
them from typed store methods backed by the dedicated `geofences` table.

## Recommended Direction

Promote geofences to a typed CRUD resource and stop treating them as generic
config.

The intended model should be:

1. typed geofence endpoints become the supported write surface
2. `GET /api/app-config` continues to include `geofences`
3. app-config reads geofences through the new typed storage path
4. the old generic geofence route is not preserved unless a concrete migration
   need appears

This keeps the result aligned with ADR-0002:

- typed boundary
- localised change
- aggregate read-through preserved
- no long-term dependence on the generic config surface

## Design Goals

- give iOS and future Android a clear geofence API contract
- parse geofences into typed Rust structs at the HTTP boundary
- validate geofence shape before storage
- keep `GET /api/app-config` as the aggregate read surface
- migrate existing stored geofence data forward if it already exists
- keep the implementation local to store + geofence module + route wiring +
  tests/docs

## Non-Goals

- making geofences local-only
- folding geofences into one app-config blob
- preserving `/api/config/geofences` indefinitely
- building geofence execution, background automation, or location-processing
  logic in this change

## API Shape

The likely typed surface is:

- `GET /api/geofences`
- `PUT /api/geofences/{id}`
- `DELETE /api/geofences/{id}`

This matches the server's other typed resource patterns.

The resource shape should be explicit Rust types, not `serde_json::Value`.

At minimum the implementation should define:

- a typed geofence response struct
- a typed upsert request struct
- a typed store record

The exact field set should match the current TaskApp contract, but the key
point is architectural:

- field parsing and validation happen at the boundary
- internal code works on typed geofence records only

If the mobile payload still needs finalisation, that should be settled before
implementation starts. The server should not reintroduce an opaque JSON body
just because the schema is temporarily unsettled.

## Aggregate Read-Through Requirement

This is a hard requirement, not a nice-to-have.

Even after geofences move to a typed resource:

- `GET /api/app-config` must still include `geofences`

That means app-config should stop reading geofences via:

- `get_config(user_id, "geofences")`

and instead read them through typed geofence store methods.

This preserves the current aggregate client contract while improving the write
surface and the internal model.

## Storage Model

The preferred end state is a dedicated geofence table, not continued use of the
generic config blob.

Recommended approach:

- add a `geofences` table keyed by `(user_id, id)`
- store one row per geofence
- store explicit typed columns where the schema is stable
- if a small nested substructure still needs JSON encoding, keep that narrowly
  scoped rather than storing the whole resource as an opaque array blob

This gives:

- row-level CRUD instead of whole-array overwrite
- clearer validation and migrations
- a cleaner `ConfigStore` trait surface

## Migration Strategy

API compatibility and data migration are separate questions.

The current prerelease decision is:

- we do not preserve `/api/config/geofences` as a supported API surface
- we do not carry a compatibility migration shim for generic-config geofence
  rows before schema freeze

That keeps the steady-state model simple:

- typed geofence handlers
- typed geofence store methods
- dedicated `geofences` table
- `GET /api/app-config` read-through to typed geofences

If the current stored blob shape is not trustworthy enough for automatic
migration, the note should be updated before implementation with a stricter
operator-facing migration policy.

## ConfigStore and Module Shape

To keep the change local, the main code changes should look like:

1. `src/store/mod.rs`
   - add typed geofence methods:
     - `list_geofences`
     - `upsert_geofence`
     - `delete_geofence`

2. `src/store/models.rs`
   - add a typed `GeofenceRecord`

3. `src/store/sqlite.rs`
   - implement typed geofence queries

4. `src/geofences/`
   - new handler module with typed request/response models

5. `src/app_config/handlers.rs`
   - read geofences through the typed store path

6. `src/main.rs`
   - register typed routes and OpenAPI types

This is the ADR-0002 shape we want:

- local resource module
- typed store boundary
- one aggregate read integration point

## Validation Expectations

This change is not just route renaming.

The typed API should validate geofence input at the boundary.

Examples of the sort of validation the implementation should own:

- required `id`
- required label/name fields if the mobile contract requires them
- numeric latitude/longitude range checks
- any radius or enabled-state constraints required by the client contract

The exact rule set depends on the agreed mobile schema, but validation should
exist in principle and be covered by tests.

## Testing Requirements

The implementation should add or update:

- typed geofence CRUD integration tests
- app-config integration tests proving geofence read-through
- migration tests if existing generic-config geofence data is upgraded
- auth failure tests for the new routes
- OpenAPI coverage for the typed geofence resource

The old generic geofence config tests should be removed or rewritten once the
typed path becomes authoritative.

## Documentation Impact

The following docs were updated as part of implementation:

- API reference
- concepts guide if geofences are called out there
- OpenAPI examples
- any client onboarding or architecture note that still points to generic
  geofence config

## Recommended Implementation Order

1. lock down the geofence schema expected by first-party clients
2. add typed storage on the existing `geofences` table
3. add typed handlers and OpenAPI
4. switch app-config read-through to typed geofence storage
5. update tests and docs
6. remove geofence-specific generic config usage

## Recommended Acceptance Criteria

This issue should be considered complete when:

- typed geofence CRUD endpoints exist
- the server uses typed geofence structs instead of `serde_json::Value`
- `GET /api/app-config` still includes `geofences`
- app-config reads geofences through typed store methods
- OpenAPI, tests, and docs match the new model
