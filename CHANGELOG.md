# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

## [0.2.0] - 2026-04-27

### Added
- `scripts/seed-udas.sh` — applies UDAs to harness-seeded tasks on staging via
  TW CLI. Covers estimate (S/M/L), energy (low/medium/high), and area fields
  across 14 of 16 seeded tasks. Idempotent, deterministic assignment.
- `depends` field on `TaskItem` API response listing UUIDs of pending
  dependencies. Enables clients to show what a task is blocked by, not just
  that it is blocked. Sorted deterministically. On list/summary responses
  `blocked == (depends.len > 0)`. On mutation webhook payloads `depends` may
  be empty when blocked — treat `blocked` as authoritative. (#79)
- User-defined attributes (UDAs) are now emitted as top-level string keys on
  `TaskItem` API responses. All TaskChampion user-defined properties pass
  through automatically, matching the Taskwarrior JSON format. (#81)
- Optional `context_id` field on view objects in `GET /api/views` and
  `GET /api/app-config`. Project-scoped named views (personal, work, health)
  now use `context_filtered=true` with an explicit `context_id` binding;
  clients auto-apply the bound context's `projectPrefixes`. (#84)
- `--scheme` flag on `staging-qr.sh` for staging-specific connect URLs
  (`cmdock-staging://`). (#80)

### Changed
- `set_startup_recovery_summary` now accepts `&StartupRecoverySummary` struct
  instead of 7 positional parameters (ADR-0002 HC-1 compliance). (#78)
- Urgency calculation now matches stock Taskwarrior defaults. Due dates no
  longer produce negative urgency. Tags use stepped scaling (max 1.0, was 3.0).
  New factors: annotations, age, active, blocking, blocked, scheduled, waiting.
  Sort order of task lists will change for existing users. (#77)
- Switched the global allocator to `tikv-jemallocator` on Linux + glibc with
  tuned `malloc_conf` (`background_thread:true`, `dirty_decay_ms:1000`,
  `muzzy_decay_ms:0`, `narenas:2`). Reduces sustained-load RSS by ~40 % and
  stabilises the 1-hour endurance profile well inside its memory budgets.
  Operators can override any setting via the `MALLOC_CONF` env var at
  startup. See `docs/reference/release-qualification-reference.md §2.1`.
- Reworked the root documentation set to match the shared documentation
  standards.
- Added a root contribution guide and clarified the README landing-page
  structure.
- Split the public container image from the internal runtime image so the
  published self-host image no longer bakes in Kellgari-specific CA trust
  material.
- Simplified the public Docker Compose deploy path to one self-host variant
  using the stock Caddy image and generic TLS modes.
- Added a `cargo-deny` policy with a commercial-ready licence allowlist,
  advisory checking, and source gating. Exposed as `just deny`; wired into
  the internal Woodpecker security pipeline alongside `cargo-audit`.
- Added tracked git hooks (`pre-commit` = fmt+clippy, `pre-push` = full
  `just check`) installable via `just install-hooks`. These are local-only;
  no CI change.

### Fixed
- `scripts/load_test_summary.py::histogram_quantile_ms` now linearly
  interpolates within the bucket that crosses the quantile threshold,
  matching Prometheus's `histogram_quantile()` semantics. Previously it
  returned the bucket upper bound without interpolating, producing clamped
  p95 values like exactly 1000 ms for any observation in the (0.5, 1.0 s]
  HTTP bucket.
- `config::tests::test_env_overrides` and the four sibling `ServerConfig`
  load tests are now serialised through a module-local mutex so the
  parallel-test race on process-global `std::env::set_var` / `remove_var`
  no longer intermittently fails `just check` / `cargo test`.
- Patched a security advisory: bumped `rand` 0.9.2 → 0.9.3 and 0.10.0 →
  0.10.1 to resolve RUSTSEC-2026-0097 (unsound stacked-borrows in
  `ThreadRng` reseed path under custom loggers). Surfaced by the new
  `cargo-deny` gate.
- Bumped `rustls-webpki` 0.103.10 → 0.103.13 to resolve RUSTSEC-2026-0098
  (URI name constraints incorrectly accepted), RUSTSEC-2026-0099 (name
  constraints accepted for wildcard certs), and RUSTSEC-2026-0104 (reachable
  panic parsing CRLs). SemVer-compatible patch bump within the 0.103.x line.

### Removed
- Dropped the unused `jsonwebtoken` and `argon2` crate dependencies —
  scaffolded for Phase 2 but never wired up.

## [0.1.0] - 2026-04-06

### Added
- Initial open-source release of `cmdock-server`.
- Bearer-token REST API for task CRUD, views, config, and summaries.
- TaskChampion-compatible sync surface for Taskwarrior-class clients.
- Local admin CLI for user, token, sync identity, device, and maintenance workflows.
- Standalone documentation library under `docs/manuals`, `docs/reference`, `docs/adr`, and `docs/implementation`.
