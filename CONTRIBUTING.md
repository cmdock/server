# Contributing

Thanks for contributing to `cmdock-server`.

## Build

Prerequisites:

- Rust toolchain
- `just` for the common development commands

Typical commands:

```bash
just build
just build-release
```

You can also use plain Cargo:

```bash
cargo build
cargo build --release
```

## Test

Run the normal local checks before opening a PR:

```bash
just test
just check
```

`just check` runs format, lint, and test together. If you are touching
runtime behavior, prefer adding or updating tests in the same change.

For deployed verification, use the repo-owned harnesses when you have access to
an environment that runs the server plus its operator surfaces:

```bash
./scripts/staging-test.sh --help
```

The public entrypoint is `scripts/staging-test.sh`, which delegates into the
structured runner at `scripts/staging_verify.py`.

## Code Style

- Format with `cargo fmt`
- Lint with `cargo clippy`
- Keep changes aligned with the local ADR set under `docs/adr/`
- Follow the repo coding guidance in [docs/reference/coding-style-guide.md](docs/reference/coding-style-guide.md)
- Keep staging/load orchestration thin in shell; when scenario logic becomes
  JSON-heavy or stateful, prefer the structured Python runners documented in
  [docs/reference/testing-strategy-reference.md](docs/reference/testing-strategy-reference.md)

## PR Process

- Keep PRs scoped to one coherent change
- Update docs when behavior, workflow, or interfaces change
- Update [CHANGELOG.md](CHANGELOG.md) for user-visible changes under `[Unreleased]`
- Expect review on correctness, boundary discipline, tests, and docs impact

Small branch names are fine. Clear commit messages matter more than a rigid
branch naming convention.

## Issue Reporting

- Use the repository issue tracker for bugs, gaps, and documentation fixes
- Include reproduction steps, expected behavior, actual behavior, and relevant
  environment details when reporting a bug
- Link to related contract or ADR docs when the issue is about behavior or
  boundary drift

## Licence

By contributing to this repo, you agree that your contributions are provided
under the repository licence: [AGPL-3.0-or-later](LICENSE).
