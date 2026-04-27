# cmdock-server justfile
#
# Staging and dogfood are internal ops-managed environments. These recipes ship
# the server artifact and run server-owned verification against the root stack
# at /opt/cmdock/docker-compose.yml; they do not own ingress or host runtime
# composition.
#
# Hostnames are loaded from .env.local (gitignored) so this file is safe to
# publish. Copy .env.local.example → .env.local and fill in real values.

set dotenv-filename := ".env.local"
set dotenv-required := false

staging_url := env_var_or_default("CMDOCK_STAGING_URL", "https://staging.example.com")
staging_ssh := env_var_or_default("CMDOCK_STAGING_SSH", "staging.example.com")
staging_tunnel_url := "http://127.0.0.1:18079"
staging_tunnel_port := "18079"
dogfood_url := env_var_or_default("CMDOCK_DOGFOOD_URL", "https://dogfood.example.com")
dogfood_ssh := env_var_or_default("CMDOCK_DOGFOOD_SSH", "dogfood.example.com")
dogfood_tunnel_url := "http://127.0.0.1:18080"
dogfood_tunnel_port := "18080"

# Default: show available recipes
default:
    @just --list

# Build debug
build:
    cargo build

# Build release
build-release:
    cargo build --release

# Run the server (debug)
run:
    cargo run --bin cmdock-server -- --config config.toml

# Run with auto-reload (requires cargo-watch)
dev:
    cargo watch -x 'run --bin cmdock-server -- --config config.toml'

# Run all tests
test:
    cargo test

# Fast parallel test run (requires cargo-nextest)
test-fast:
    cargo nextest run

# Check for outdated root dependencies
outdated:
    cargo outdated -R

# Detect unused dependencies (fast, stable-only)
machete:
    cargo machete

# Show binary size breakdown by crate
bloat:
    cargo bloat --release --crates

# Fuzz targets (requires cargo-fuzz: cargo install cargo-fuzz)
fuzz-list:
    cargo +nightly fuzz list

fuzz-filter secs="10":
    cargo +nightly fuzz run filter_parse -- -max_total_time={{ secs }}

fuzz-task-raw secs="10":
    cargo +nightly fuzz run task_raw_parse -- -max_total_time={{ secs }}

fuzz-sync-content-type secs="10":
    cargo +nightly fuzz run tc_sync_content_type -- -max_total_time={{ secs }}

fuzz-webhook secs="10":
    cargo +nightly fuzz run webhook_request_normalization -- -max_total_time={{ secs }}

# Run tests with output
test-verbose:
    cargo test -- --nocapture

# Run clippy lints
lint:
    cargo clippy -- -D warnings

# Format code
fmt:
    cargo fmt

# Check formatting
fmt-check:
    cargo fmt -- --check

# Run all quality checks
check: fmt-check lint machete test

# Supply-chain + licence audit (runs in .woodpecker/security.yml too).
# Not part of `just check` because it needs network + is slow; run before
# tagging a release or after Cargo.toml changes.
deny:
    cargo deny check

# Publication surface gate — deterministic regex scan for private
# references (FQDNs, RFC1918 IPs, absolute /home paths, internal tooling).
# Fast and free; stdlib-only. Run this before `docs-rubric`.
docs-surface:
    python3 scripts/publication_surface_check.py \
        --scope wide \
        --allowlist scripts/publication_surface_allowlist.txt

# RUB-0002 documentation rubric assessment via the Claude API.
# Expensive (~$0.72/run on Opus 4.6) — only run after `docs-surface`
# passes. Requires CMDOCK_DOCS_RUBRIC_ANTHROPIC_KEY exported, typically
# sourced from ~/.config/secrets/api-keys.sh via the one-liner in the
# script header.
docs-rubric:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -z "${CMDOCK_DOCS_RUBRIC_ANTHROPIC_KEY:-}" ]]; then
        echo "error: CMDOCK_DOCS_RUBRIC_ANTHROPIC_KEY not set." >&2
        echo "  export CMDOCK_DOCS_RUBRIC_ANTHROPIC_KEY=\$(bash -c 'source ~/.config/secrets/api-keys.sh && echo \$ANTHROPIC_API_KEY')" >&2
        exit 2
    fi
    uv run scripts/docs_rubric_assess.py --output /tmp/rubric-report.md

# RUB-0002 rubric assessment run against BOTH Opus 4.6 and Sonnet 4.6
# with a side-by-side consensus report. Use before tagging a public
# release candidate — the two models catch different classes of issues
# (see docs/internal/public-release-checklist.md). Cost ~$1.29 per run,
# ~8 minutes wall-clock because Sonnet is slower.
docs-rubric-compare:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -z "${CMDOCK_DOCS_RUBRIC_ANTHROPIC_KEY:-}" ]]; then
        echo "error: CMDOCK_DOCS_RUBRIC_ANTHROPIC_KEY not set." >&2
        echo "  export CMDOCK_DOCS_RUBRIC_ANTHROPIC_KEY=\$(bash -c 'source ~/.config/secrets/api-keys.sh && echo \$ANTHROPIC_API_KEY')" >&2
        exit 2
    fi
    uv run scripts/docs_rubric_assess.py \
        --compare-models claude-opus-4-6,claude-sonnet-4-6 \
        --output /tmp/rubric-compare.md

# Complete documentation gate: publication-surface check first (cheap,
# fails fast), then LLM rubric assessment only if surface is clean.
# Intended for pre-publication runs and the release checklist.
docs-ready: docs-surface docs-rubric

# Install tracked git hooks into .git/hooks/ as relative symlinks. Idempotent.
# Hooks themselves live in scripts/git-hooks/ so they're reviewable and shared.
install-hooks:
    #!/usr/bin/env bash
    set -euo pipefail
    hook_dir="$(git rev-parse --git-path hooks)"
    mkdir -p "$hook_dir"
    for hook in pre-commit pre-push; do
        src="../../scripts/git-hooks/$hook"
        dst="$hook_dir/$hook"
        ln -sf "$src" "$dst"
        echo "installed: $dst -> $src"
    done

# Run database migrations
migrate:
    cargo run --bin cmdock-server -- --migrate

# Print the generated OpenAPI JSON to stdout
openapi-print:
    cargo run --bin cmdock-server -- openapi

# Export the generated OpenAPI JSON to a file
openapi-export output="target/openapi/cmdock-server-openapi.json":
    cargo run --bin cmdock-server -- openapi --output {{ output }}

# --- Admin CLI ---

# Create a user and print their API token
admin-user-create username:
    cargo run --bin cmdock-server -- admin user create --username {{ username }}

# List all users
admin-user-list:
    cargo run --bin cmdock-server -- admin user list

# Delete a user (interactive confirmation)
admin-user-delete user_id:
    cargo run --bin cmdock-server -- admin user delete {{ user_id }}

# Create an API token for a user
admin-token-create user_id label="":
    cargo run --bin cmdock-server -- admin token create {{ user_id }} {{ if label != "" { "--label " + label } else { "" } }}

# List tokens for a user
admin-token-list user_id:
    cargo run --bin cmdock-server -- admin token list {{ user_id }}

# Revoke a token by hash prefix
admin-token-revoke hash:
    cargo run --bin cmdock-server -- admin token revoke {{ hash }}

# Create the canonical sync identity for a user
admin-sync-create user_id:
    cargo run --bin cmdock-server -- admin sync create {{ user_id }}

# Show the canonical sync identity for a user
admin-sync-show user_id:
    cargo run --bin cmdock-server -- admin sync show {{ user_id }}

# Show the canonical sync secret (hidden debug / migration helper)
admin-sync-show-secret user_id:
    cargo run --bin cmdock-server -- admin sync show-secret {{ user_id }}

# Delete the canonical sync identity for a user (destructive whole-user reset)
admin-sync-delete user_id:
    cargo run --bin cmdock-server -- admin sync delete {{ user_id }}

# List devices for a user
admin-device-list user_id:
    cargo run --bin cmdock-server -- admin device list {{ user_id }}

# Create a device and print onboarding credentials
admin-device-create user_id name server_url="":
    cargo run --bin cmdock-server -- admin device create {{ user_id }} --name {{ name }} {{ if server_url != "" { "--server-url " + server_url } else { "" } }}

# Convenience: onboard one physical client and print its copy/paste credentials
admin-onboard-device user_id name server_url="":
    cargo run --bin cmdock-server -- admin device create {{ user_id }} --name {{ name }} {{ if server_url != "" { "--server-url " + server_url } else { "" } }}

# Show a device record
admin-device-show user_id client_id:
    cargo run --bin cmdock-server -- admin device show {{ user_id }} {{ client_id }}

# Print a .taskrc snippet for an existing device
admin-device-taskrc user_id client_id server_url="":
    cargo run --bin cmdock-server -- admin device taskrc {{ user_id }} {{ client_id }} {{ if server_url != "" { "--server-url " + server_url } else { "" } }}

# Rename a device
admin-device-rename user_id client_id name:
    cargo run --bin cmdock-server -- admin device rename {{ user_id }} {{ client_id }} --name {{ name }}

# Revoke a device
admin-device-revoke user_id client_id:
    cargo run --bin cmdock-server -- admin device revoke {{ user_id }} {{ client_id }} -y

# Convenience: disable a device without deleting its record
admin-disable-device user_id client_id:
    cargo run --bin cmdock-server -- admin device revoke {{ user_id }} {{ client_id }} -y

# Unrevoke a device
admin-device-unrevoke user_id client_id:
    cargo run --bin cmdock-server -- admin device unrevoke {{ user_id }} {{ client_id }} -y

# Delete a revoked device permanently
admin-device-delete user_id client_id:
    cargo run --bin cmdock-server -- admin device delete {{ user_id }} {{ client_id }} -y

# Generate a short-lived cmdock:// connect-config URL and terminal QR for a user
admin-connect-config user_id server_url="" name="" expires_minutes="60":
    cargo run --bin cmdock-server -- admin connect-config create {{ user_id }} {{ if server_url != "" { "--server-url " + server_url } else { "" } }} {{ if name != "" { "--name " + name } else { "" } }} --expires-minutes {{ expires_minutes }}

# Back up config DB and replicas
admin-backup output="./backups":
    cargo run --bin cmdock-server -- admin backup --output {{ output }}

# Restore from backup directory
admin-restore dir:
    cargo run --bin cmdock-server -- admin restore --input {{ dir }}

# Create a new migration
migration name:
    @mkdir -p migrations
    @echo "-- {{ name }}" > "migrations/$(date +%Y%m%d%H%M%S)_{{ name }}.sql"
    @echo "Created migrations/$(date +%Y%m%d%H%M%S)_{{ name }}.sql"

# Clean build artifacts
clean:
    cargo clean

# Cross-compile for x86_64 Linux (for deployment)
build-deploy:
    cargo build --release --target x86_64-unknown-linux-gnu

# --- Test Harness (requires TC_TOKEN env var or --token) ---

# Run smoke tests against the server
smoke url="http://localhost:8080":
    cargo run --bin test-harness -- --url {{ url }} smoke

# Seed test data into the server
seed url="http://localhost:8080":
    cargo run --bin test-harness -- --url {{ url }} seed

# Remove seeded test data
unseed url="http://localhost:8080":
    cargo run --bin test-harness -- --url {{ url }} unseed

# Compare view filter results: server vs TW CLI
compare view url="http://localhost:8080":
    cargo run --bin test-harness -- --url {{ url }} compare {{ view }}

# Test filter against TW CLI locally (no server needed)
filter-test filter:
    cargo run --bin test-harness -- filter "{{ filter }}"

# Run side-by-side test bubble (isolated TW + server, compare filters)
test-bubble:
    ./scripts/test-bubble.sh

# Run real CLI sync integration tests (requires task 3.x)
test-sync:
    ./scripts/test-sync.sh

# Upgrade server with pre-flight check and automatic rollback
upgrade binary="":
    ./scripts/upgrade.sh {{ binary }}

# --- Deploy / Verify ---

# Deploy current local image to staging
deploy-staging:
    ./scripts/deploy.sh local staging

# Deploy current local image to dogfood
deploy-dogfood:
    ./scripts/deploy.sh local dogfood

# Create a cold pre-deploy snapshot bundle for dogfood.
snapshot-dogfood-predeploy output_dir="./snapshots":
    ./scripts/predeploy-snapshot.sh --target dogfood --output-dir {{ output_dir }}

# Create a cold pre-deploy snapshot bundle for staging.
snapshot-staging-predeploy output_dir="./snapshots":
    ./scripts/predeploy-snapshot.sh --target staging --output-dir {{ output_dir }}

# Recreate staging server service in the ops-managed root stack
restart-staging:
    ./scripts/deploy.sh staging

# Recreate dogfood server service in the ops-managed root stack
restart-dogfood:
    ./scripts/deploy.sh dogfood

# Show staging deployment status
status-staging:
    ./scripts/deploy.sh status staging

# Show dogfood deployment status
status-dogfood:
    ./scripts/deploy.sh status dogfood

# Tail staging logs
logs-staging:
    ./scripts/deploy.sh logs staging

# Tail dogfood logs
logs-dogfood:
    ./scripts/deploy.sh logs dogfood

# Run staging health check
health-staging:
    ./scripts/deploy.sh health staging

# Run dogfood health check
health-dogfood:
    ./scripts/deploy.sh health dogfood

# Run smoke tests against staging using a temporary server-side user/token
smoke-staging:
    #!/usr/bin/env bash
    set -euo pipefail

    SSH_HOST="{{ staging_ssh }}"
    SERVER_URL="{{ staging_url }}"
    DOCKER_ADMIN="sudo docker compose -f /opt/cmdock/docker-compose.yml exec -T server cmdock-server --config /app/config.toml"

    admin_cli() {
        local cmd="$DOCKER_ADMIN"
        local arg
        for arg in "$@"; do
            cmd+=" '${arg//\'/\'\\\'\'}'"
        done
        ssh -o ControlMaster=no -o ConnectTimeout=10 "$SSH_HOST" "$cmd" 2>&1 | grep -v '^{"timestamp"' || true
    }

    cleanup() {
        if [[ -n "${USER_ID:-}" ]]; then
            admin_cli admin user delete "$USER_ID" --yes >/dev/null 2>&1 || true
        fi
    }
    trap cleanup EXIT

    USERNAME="smoke-staging-$(date +%s)"
    CREATE_OUTPUT="$(admin_cli admin user create --username "$USERNAME")"
    USER_ID="$(echo "$CREATE_OUTPUT" | awk '
        /^[[:space:]]*ID:/ {
            print $2
            exit
        }
    ')"
    TOKEN="$(echo "$CREATE_OUTPUT" | awk '
        /API token/ { capture=1; next }
        capture {
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", $0)
            if ($0 != "") {
                print
                exit
            }
        }
    ')"

    if [[ -z "$USER_ID" || -z "$TOKEN" ]]; then
        echo "Failed to provision temporary staging smoke user" >&2
        echo "$CREATE_OUTPUT" >&2
        exit 1
    fi

    cargo run --bin test-harness -- --url "$SERVER_URL" --token "$TOKEN" smoke

# Run smoke tests against dogfood using a temporary server-side user/token
smoke-dogfood:
    #!/usr/bin/env bash
    set -euo pipefail

    SSH_HOST="{{ dogfood_ssh }}"
    SERVER_URL="{{ dogfood_url }}"
    DOCKER_ADMIN="sudo docker compose -f /opt/cmdock/docker-compose.yml exec -T server cmdock-server --config /app/config.toml"

    admin_cli() {
        local cmd="$DOCKER_ADMIN"
        local arg
        for arg in "$@"; do
            cmd+=" '${arg//\'/\'\\\'\'}'"
        done
        ssh -o ControlMaster=no -o ConnectTimeout=10 "$SSH_HOST" "$cmd" 2>&1 | grep -v '^{"timestamp"' || true
    }

    cleanup() {
        if [[ -n "${USER_ID:-}" ]]; then
            admin_cli admin user delete "$USER_ID" --yes >/dev/null 2>&1 || true
        fi
    }
    trap cleanup EXIT

    USERNAME="smoke-dogfood-$(date +%s)"
    CREATE_OUTPUT="$(admin_cli admin user create --username "$USERNAME")"
    USER_ID="$(echo "$CREATE_OUTPUT" | awk '
        /^[[:space:]]*ID:/ {
            print $2
            exit
        }
    ')"
    TOKEN="$(echo "$CREATE_OUTPUT" | awk '
        /API token/ { capture=1; next }
        capture {
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", $0)
            if ($0 != "") {
                print
                exit
            }
        }
    ')"

    if [[ -z "$USER_ID" || -z "$TOKEN" ]]; then
        echo "Failed to provision temporary dogfood smoke user" >&2
        echo "$CREATE_OUTPUT" >&2
        exit 1
    fi

    cargo run --bin test-harness -- --url "$SERVER_URL" --token "$TOKEN" smoke

# Run the current staging verification flow: smoke + direct HTTPS Taskwarrior E2E on the ops-managed host
verify-staging:
    #!/usr/bin/env bash
    set -euo pipefail
    just smoke-staging
    ./scripts/staging-test.sh --require-admin-http --url "{{ staging_url }}" --ssh "{{ staging_ssh }}"

# Run the current full staging verification flow: smoke + full direct HTTPS Taskwarrior E2E on the ops-managed host
verify-staging-full:
    #!/usr/bin/env bash
    set -euo pipefail
    just smoke-staging
    ./scripts/staging-test.sh --full --require-admin-http --url "{{ staging_url }}" --ssh "{{ staging_ssh }}"

# Run the workstation fallback staging verification flow through a localhost tunnel
verify-staging-tunneled:
    #!/usr/bin/env bash
    set -euo pipefail

    just smoke-staging

    ssh -o ControlMaster=no -N -L "{{ staging_tunnel_port }}:127.0.0.1:8080" "{{ staging_ssh }}" &
    TUNNEL_PID=$!
    cleanup() {
        kill "$TUNNEL_PID" >/dev/null 2>&1 || true
        wait "$TUNNEL_PID" >/dev/null 2>&1 || true
    }
    trap cleanup EXIT
    sleep 2

    ./scripts/staging-test.sh --require-admin-http --tw-local --url "{{ staging_tunnel_url }}" --ssh "{{ staging_ssh }}"

# Run the workstation fallback full staging verification flow through a localhost tunnel
verify-staging-full-tunneled:
    #!/usr/bin/env bash
    set -euo pipefail

    just smoke-staging

    ssh -o ControlMaster=no -N -L "{{ staging_tunnel_port }}:127.0.0.1:8080" "{{ staging_ssh }}" &
    TUNNEL_PID=$!
    cleanup() {
        kill "$TUNNEL_PID" >/dev/null 2>&1 || true
        wait "$TUNNEL_PID" >/dev/null 2>&1 || true
    }
    trap cleanup EXIT
    sleep 2

    ./scripts/staging-test.sh --full --require-admin-http --tw-local --url "{{ staging_tunnel_url }}" --ssh "{{ staging_ssh }}"

# Run the current dogfood verification flow: smoke + direct HTTPS Taskwarrior E2E on the ops-managed host
verify-dogfood:
    #!/usr/bin/env bash
    set -euo pipefail

    just smoke-dogfood

    ./scripts/staging-test.sh --require-admin-http --url "{{ dogfood_url }}" --ssh "{{ dogfood_ssh }}"

# Run the current full dogfood verification flow: smoke + full direct HTTPS Taskwarrior E2E on the ops-managed host
verify-dogfood-full:
    #!/usr/bin/env bash
    set -euo pipefail

    just smoke-dogfood

    ./scripts/staging-test.sh --full --require-admin-http --url "{{ dogfood_url }}" --ssh "{{ dogfood_ssh }}"

# Run the workstation fallback dogfood verification flow through a localhost tunnel
verify-dogfood-tunneled:
    #!/usr/bin/env bash
    set -euo pipefail

    just smoke-dogfood

    ssh -o ControlMaster=no -N -L "{{ dogfood_tunnel_port }}:127.0.0.1:8080" "{{ dogfood_ssh }}" &
    TUNNEL_PID=$!
    cleanup() {
        kill "$TUNNEL_PID" >/dev/null 2>&1 || true
        wait "$TUNNEL_PID" >/dev/null 2>&1 || true
    }
    trap cleanup EXIT
    sleep 2

    ./scripts/staging-test.sh --require-admin-http --tw-local --url "{{ dogfood_tunnel_url }}" --ssh "{{ dogfood_ssh }}"

# Run the workstation fallback full dogfood verification flow through a localhost tunnel
verify-dogfood-full-tunneled:
    #!/usr/bin/env bash
    set -euo pipefail

    just smoke-dogfood

    ssh -o ControlMaster=no -N -L "{{ dogfood_tunnel_port }}:127.0.0.1:8080" "{{ dogfood_ssh }}" &
    TUNNEL_PID=$!
    cleanup() {
        kill "$TUNNEL_PID" >/dev/null 2>&1 || true
        wait "$TUNNEL_PID" >/dev/null 2>&1 || true
    }
    trap cleanup EXIT
    sleep 2

    ./scripts/staging-test.sh --full --require-admin-http --tw-local --url "{{ dogfood_tunnel_url }}" --ssh "{{ dogfood_ssh }}"

# Deploy to staging and run the standard verification flow
deploy-verify-staging:
    #!/usr/bin/env bash
    set -euo pipefail
    just deploy-staging
    just verify-staging

# Deploy to dogfood and run the standard verification flow
deploy-verify-dogfood:
    #!/usr/bin/env bash
    set -euo pipefail
    just deploy-dogfood
    just verify-dogfood

# Run Goose load test (profile-aware; default is mixed)

# Profiles: mixed | personal-only | team-contention | multi-device-single-user
load-test users="20" duration="30s" personal="5" seed="10" profile="mixed":
    users="{{ users }}"; \
    duration="{{ duration }}"; \
    personal="{{ personal }}"; \
    seed="{{ seed }}"; \
    profile="{{ profile }}"; \
    case "$users" in users=*) users="${users#users=}" ;; esac; \
    case "$duration" in duration=*) duration="${duration#duration=}" ;; esac; \
    case "$personal" in personal=*|personal-users=*) personal="${personal#*=}" ;; esac; \
    case "$seed" in seed=*|seed-tasks=*) seed="${seed#*=}" ;; esac; \
    case "$profile" in profile=*) profile="${profile#profile=}" ;; esac; \
    ./scripts/load-test.sh "$users" "$duration" "$personal" "$seed" --profile "$profile"

# Run the release qualification matrix against local load-test budgets.
qualify-release *scenarios='':
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -n "{{ scenarios }}" ]]; then
        args=()
        for scenario in {{ scenarios }}; do
            args+=("--scenario" "$scenario")
        done
        python3 ./scripts/release_qualification.py "${args[@]}"
    else
        python3 ./scripts/release_qualification.py
    fi

# Run only the endurance / soak qualification scenario.
qualify-release-soak:
    python3 ./scripts/release_qualification.py --scenario mixed-soak

# Run the shared-team contention qualification scenario.
qualify-release-contention:
    python3 ./scripts/release_qualification.py --scenario team-contention

# Run the optional 1-hour endurance qualification scenario.
qualify-release-endurance:
    python3 ./scripts/release_qualification.py --scenario mixed-endurance

# Run regression tests for the internal release-qualification scripts.
test-release-qualification-scripts:
    python3 -m unittest scripts/test_release_qualification.py

# Run E2E staging tests (P0 core product tests)
staging-test:
    ./scripts/staging-test.sh --url "{{ staging_url }}" --ssh "{{ staging_ssh }}"

# Run full E2E staging tests (P0 + P1 including restart, latency, backup)
staging-test-full:
    ./scripts/staging-test.sh --full --url "{{ staging_url }}" --ssh "{{ staging_ssh }}"

# Legacy alias for verify-staging
staging-test-admin:
    just verify-staging

# Legacy alias for verify-staging-full
staging-test-full-admin:
    just verify-staging-full

# Run real TC sync load test (real task CLI, not opaque blobs)
real-sync-test users="3" tasks="5":
    ./scripts/real-sync-load.sh {{ users }} {{ tasks }}
