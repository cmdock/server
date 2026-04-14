# HTTP DTO Validation With `garde`

This note records the current request-validation direction for
`cmdock-server`.

It is an implementation convention, not a new ADR.

## Intent

The goal is not generic "input sanitisation".

The goal is:

- parse input at the boundary
- validate shape, size, and simple invariants close to the request DTO
- keep handlers thin
- preserve the current simple wire error model unless an endpoint already
  defines something richer

This follows ADR-0002's "parse at boundaries" rule.

## What `garde` Is Used For

`garde` is the default helper for lightweight HTTP DTO validation where that
keeps request rules local to the DTO:

- non-empty string checks
- max-length rules
- control-character rejection
- simple nested list validation
- similar shape/invariant checks on request structs

Examples in the current repo include:

- device registration / rename request DTOs
- app-config write DTOs such as contexts, stores, presets, and shopping config

The expected handler shape remains:

1. extract typed input
2. run lightweight validation
3. call one service/store/coordinator seam
4. map the result to the endpoint's existing response contract

## What `garde` Is Not For

`garde` is not being adopted to:

- introduce a new generic JSON validation envelope
- replace contextual output escaping
- sanitize data for HTML rendering
- enforce all path/query parsing when Axum or typed extractors can do that

The public API still mostly returns simple status codes and short text errors.
Validation should support that contract, not force a richer one.

## Typed Parsing Still Comes First

Use the narrowest boundary type that makes sense before reaching for validation
attributes.

Prefer:

- `Path<Uuid>` over `Path<String>` plus manual UUID parsing
- enums for constrained query values
- typed request structs over raw maps

Use `garde` after parsing for the remaining shape rules that typed extraction
does not express cleanly.

## Resource IDs

For string resource IDs that are intentionally not UUIDs, use shared boundary
validation helpers rather than duplicating ad hoc checks in handlers.

Current expectations for those IDs are:

- non-empty
- bounded length
- no control characters
- no path-like segments such as `/`, `\\`, or `..`

## Error Contract

Unless an endpoint already defines a richer error contract, validation failures
should continue to map to:

- `400 Bad Request`
- short stable message or the current plain-text/simple status behaviour

The validation library is an internal consistency tool, not a reason to change
the external API shape by itself.

## Testing Expectations

When adding validation on a write path, add negative integration coverage for
the invalid input classes you are blocking, for example:

- empty or whitespace-only values
- overlong strings
- control characters
- invalid resource IDs
- invalid enum/query values

Keep these tests at the route/integration layer when the behaviour is part of
the HTTP boundary contract.
