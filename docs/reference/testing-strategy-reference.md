# Testing Strategy Reference

This document describes how the repository uses different test layers and what
each layer is supposed to prove.

It is not a replacement for reading individual tests, but it gives a map of
intent.

This reference remains the authoritative standalone explanation of how those
layers apply within `cmdock/server`.

## 1. Purpose

The codebase uses several distinct test layers because the failure modes are
different:

- pure logic errors
- storage/integration drift
- runtime coordination bugs
- operator workflow regressions
- performance bottlenecks

One test layer cannot realistically cover all of those well.

## 2. Unit Tests

Unit tests are used for:

- pure logic helpers
- crypto and derivation behaviour
- parser / filter behaviour
- cache and scheduler-local logic
- storage helper behaviour at a small scope

These should be:

- fast
- narrow
- deterministic

## 3. Integration Tests

Integration tests are used for:

- route + state + storage interaction
- device lifecycle behaviour
- TaskChampion sync behaviour
- bridge behaviour
- admin HTTP behaviour
- HTTP boundary validation regressions on write/query surfaces

These tests normally exercise real modules together, but still in a controlled
test harness.

## 4. System / UAT-Style Tests

System tests are used for:

- self-hosted onboarding flows
- admin CLI plus running server interaction
- restart persistence
- operator recovery workflows
- real Taskwarrior CLI interoperability checks

These exist because some failures only appear when:

- the CLI and server interact indirectly through disk/runtime state
- restart boundaries matter
- multiple surfaces are exercised in one scenario
- a real `task` binary exercises the TaskChampion protocol end to end

## 5. Load / Performance Tests

Load tests are used for:

- concurrency behaviour
- contention analysis
- identifying hot-path bottlenecks
- comparing user/device profile shapes

The current profile set matters because different workloads stress different
parts of the runtime:

- personal-only
- mixed
- team-contention
- multi-device-single-user

Current shell harnesses carry distinct intent too:

- `scripts/test-sync.sh`
  - local real-CLI interoperability and lifecycle checks
- `scripts/staging-test.sh`
  - thin compatibility entrypoint that delegates into
    `scripts/staging_verify.py`
- `scripts/staging_verify.py`
  - deployed-server end-to-end orchestration and preflight, including
    standalone `cmdock-admin` operator flows plus REST <-> Taskwarrior
    propagation
  - delegates structured full-run scenario slices into Python helpers where
    the state and assertions are JSON-heavy:
    `scripts/staging_admin_runtime.py`,
    `scripts/staging_product_runtime.py`,
    `scripts/staging_backup_restore.py` and
    `scripts/staging_webhooks_runtime.py`
  - the remaining shell body in `scripts/staging_verify_legacy.sh` is a
    transitional compatibility layer focused on env setup, legacy
    server-local maintenance flows, and result collation rather than the
    long-term home for new staging scenario logic
- `scripts/load-test.sh`
  - thin orchestration wrapper for profile-driven contention and throughput
    analysis
  - delegates profile-shaped config DB seeding and crypto-sensitive setup into
    `scripts/load_test_seed.py`
  - now also emits a machine-readable summary through
    `scripts/load_test_summary.py` so release qualification can evaluate
    startup, memory, disk, throughput, and contention budgets
- `scripts/release_qualification.py`
  - evaluates the current release budget matrix defined in
    `scripts/release_qualification_budgets.json`
  - treats load-test summaries as the release gate for first-pass runtime
    expectations on the maintained benchmark host class
  - keeps realistic mixed-usage latency gates separate from explicit
    shared-team contention robustness gates
  - supports a dedicated `team-contention` gate for hot shared-replica
    pressure without treating that profile as the normal mixed-usage UX budget
  - keeps `team-contention` outside the default qualification matrix because
    it is an explicit robustness gate, not part of the normal mixed-usage UX
    budget
  - supports optional non-default profiles such as longer soak and endurance
    runs without forcing them into every normal release-qualification pass
- `scripts/test_release_qualification.py`
  - protects the internal summary/evaluator contract for
    `scripts/load_test_summary.py` and `scripts/release_qualification.py`
  - specifically guards the newer contention-facing summary fields such as
    `replica.operations.*` and `replica.lock_wait.*`
- `tests/concurrency_integration.rs`
  - includes a narrow hot shared-user modify-pressure regression that proves
    the replica operation and lock-wait metrics are emitted under focused
    contention without requiring a full Goose run
- `scripts/release_endurance.py`
  - phased internal endurance runner for longer-lived stability checks
  - keeps one isolated server alive across warm-up, restart, and resume phases
  - complements the shorter `mixed-soak` budget by exercising restart recovery
    and longer-lived memory/fd drift on the same seeded data set

## 5.1 Fuzzing

Fuzzing is used selectively for parser-heavy and protocol-boundary code where
arbitrary input should never panic, hang, or trigger pathological behaviour.

Current checked-in fuzz targets cover:

- Taskwarrior raw task parsing
- Task filter parsing
- TaskChampion sync content-type boundary matching
- Webhook request-body and normalization validation

These map to:

- `fuzz/fuzz_targets/task_raw_parse.rs`
- `fuzz/fuzz_targets/filter_parse.rs`
- `fuzz/fuzz_targets/tc_sync_content_type.rs`
- `fuzz/fuzz_targets/webhook_request_normalization.rs`

The intended scope is narrow:

- add fuzzing for parser-heavy or protocol-boundary logic
- do not add fuzz targets for full CRUD handler stacks, routine store queries,
  or orchestration-heavy admin paths unless a concrete bug justifies it
- prefer one small focused target over a broad "fuzz the whole subsystem"
  target

The current webhook target is the model to follow for newer surfaces:

- fuzz the pure JSON/deserialization and normalization seam
- keep HTTP framework wiring out of scope
- add narrow regression tests for crashes in the underlying parser or helper
  instead of relying on the fuzz target alone

The fuzz layer is intentionally separate from normal `cargo test` runs. It is a
periodic hardening tool, not a replacement for unit or integration coverage.

Use `cargo-fuzz` or the corresponding `just fuzz-*` recipes to run these
targets locally.

For the internal release-qualification script contract, use:

```bash
just test-release-qualification-scripts
```

That regression layer is intentionally separate from the Rust test suite
because the load-summary and budget-evaluator seams are implemented in Python.

For deployed verification against a pre-production environment, prefer the
repository-owned helper paths so operator-only coverage is not skipped
silently:

- `./scripts/staging-test.sh`
- `./scripts/staging-test.sh --full`

The verification script expects explicit environment information such as the
target server URL, SSH host, and operator token. It should not read operator
credentials back from the running server.

Where the standalone `cmdock-admin` binary is deployed on the verification
host, the harness prefers that binary for shipped operator flows such as
doctor, user listing/deletion, Taskwarrior bootstrap, backup creation/list,
real snapshot restore rehearsal, and post-restore doctor validation. The older
server-local admin CLI path remains only for maintenance subcommands that
`cmdock-admin` does not expose yet.

If admin HTTP coverage is required for a run, use the helper paths above or
pass `--require-admin-http` directly. That makes the script fail fast instead
of silently producing a green run with `/admin/*` coverage skipped.

That keeps the `/admin/*` and runtime-policy checks from being skipped
accidentally during routine verification.

Operational expectations:

- fuzz targets currently require nightly Rust because `cargo-fuzz` uses
  sanitizer flags that are not accepted on stable
- checked-in seed corpora under [`fuzz/corpus/`](../../fuzz/corpus)
  are intentional and should be treated as part of the hardening surface
- crash artifacts under `fuzz/artifacts/` are local debugging output and are
  not part of the normal repository state
- when fuzzing finds a crash, add a narrow regression test for the underlying
  parser/helper in addition to keeping or minimizing the reproducer

A typical local workflow is:

```bash
cargo install cargo-fuzz --locked
just fuzz-filter 10
just fuzz-task-raw 10
just fuzz-sync-content-type 10
just fuzz-webhook 10
```

To replay a reproducer directly:

```bash
cargo +nightly fuzz run filter_parse fuzz/artifacts/filter_parse/<artifact> -- -runs=1
```

Corpus guidance:

- start with empty or generated corpora for new targets
- check in minimized crash repros and especially valuable edge-case seeds
- do not bulk-check in large low-signal corpora by default

## 6. Why E2E Recovery Tests Matter

Recovery is a good example of why broad tests are required.

You can unit-test:

- recovery assessment logic
- offline marker helpers

You can integration-test:

- admin diagnostics

But only E2E/UAT-style tests really prove:

- offline marker coordination with a running server
- selective restore while other users remain online
- restart persistence of recovery state
- mixed REST + TaskChampion recovery behaviour

## 7. How to Choose a Test Layer

When adding coverage, ask:

- is this pure logic?
  - unit test
- is this one subsystem boundary?
  - integration test
- does this involve CLI + server + disk + restart + operator sequencing?
  - system / E2E test
- is the question about throughput / contention shape?
  - load test

## 8. Common Mistakes

Common testing mistakes in this codebase would be:

- using slow E2E tests for pure helper logic
- relying only on unit tests for operator workflows
- treating load tests as correctness tests
- failing to update UAT scripts after changing provisioning or lifecycle rules

## 9. Related Docs

- [Performance and Scaling Guide](../manuals/performance-and-scaling-guide.md)
- [Admin Surfaces Reference](admin-surfaces-reference.md)
- [Recovery Reference](recovery-reference.md)
