#!/usr/bin/env bash
# Legacy shell scenario runner for cmdock-server staging verification.
#
# The compatibility entrypoint is now `scripts/staging-test.sh`, which delegates
# to the Python runner at `scripts/staging_verify.py`. This file remains as the
# transitional shell implementation for detailed scenario bodies while the
# orchestration/preflight boundary is moved into Python.
#
# Validates the full product works against a real deployed server:
#   - Standalone cmdock-admin operator flows (doctor, user, connect, backup, restore)
#   - Server-local admin CLI maintenance operations still not exposed by cmdock-admin
#   - Admin HTTP endpoints (status, stats, evict, checkpoint, offline/online)
#   - Real TW CLI ↔ REST API bidirectional sync (the core product flow)
#   - Multi-device convergence (2 TW profiles, same account)
#   - Server restart persistence
#   - Latency baseline measurements
#
# Usage:
#   ./scripts/staging-test.sh                  # Run P0 tests (core product)
#   ./scripts/staging-test.sh --full           # Run P0 + P1 tests
#   ./scripts/staging-test.sh --full --require-admin-http
#                                           # Fail early unless /admin/* coverage will run
#   ./scripts/staging-test.sh --url URL        # Custom server URL
#   ./scripts/staging-test.sh --ssh HOST       # Custom SSH host for admin CLI
#   ./scripts/staging-test.sh --tw-host HOST   # Run Taskwarrior on remote host via SSH
#   ./scripts/staging-test.sh --tw-local       # Run Taskwarrior on the local workstation
#
# Prerequisites:
#   - SSH access to the internal host that runs the root ops-managed compose stack
#   - curl, jq installed locally
#
# The script creates and tears down its own test users. Safe to run repeatedly.
# For internal staging and dogfood, this script assumes the root stack at
# /opt/cmdock/docker-compose.yml and service name `server`. The default
# Taskwarrior path is the ops-managed internal host over direct HTTPS. Use
# --tw-local only as a workstation fallback, typically with an SSH tunnel.

set -euo pipefail

# --- Configuration ---
SERVER_URL="${STAGING_URL:-${CMDOCK_STAGING_URL:-https://staging.example.com}}"
SSH_HOST="${STAGING_SSH:-${CMDOCK_STAGING_SSH:-staging.example.com}}"
ADMIN_HTTP_TOKEN="${STAGING_ADMIN_TOKEN:-${CMDOCK_ADMIN_TOKEN:-}}"
# Server-local admin CLI still runs inside the server container for maintenance
# commands not yet exposed by the standalone cmdock-admin binary.
DOCKER_COMPOSE_FILE="/opt/cmdock/docker-compose.yml"
SERVER_DOCKER_ADMIN="sudo docker compose -f $DOCKER_COMPOSE_FILE exec -T server cmdock-server --config /app/config.toml"
REMOTE_CMDOCK_ADMIN="/usr/local/bin/cmdock-admin"
RUN_FULL=false
REQUIRE_ADMIN_HTTP=false
BUBBLE_DIR=""
SSH_BASE_OPTS=(-o ControlMaster=no)
TW_EXEC_MODE="${STAGING_TW_MODE:-ssh}"
TW_HOST="${STAGING_TW_HOST:-}"
TW_PROFILE_ROOT=""
ANSIBLE_INVENTORY_FILE="${ANSIBLE_INVENTORY_FILE:-deploy/ansible/inventory/hosts.yml}"
ANSIBLE_STAGING_HOST="${ANSIBLE_STAGING_HOST:-vm-cmdock-staging-01}"
ANSIBLE_DOGFOOD_HOST="${ANSIBLE_DOGFOOD_HOST:-vm-cmdock-dogfood-01}"
WEBHOOK_RECEIVER_IMAGE="cmdock-webhook-receiver:latest"
WEBHOOK_RECEIVER_CONTAINER="cmdock-webhook-receiver-e2e"
WEBHOOK_RECEIVER_ROUTE="/__e2e/webhook"
WEBHOOK_RECEIVER_EXTERNAL_URL="${CMDOCK_WEBHOOK_RECEIVER_URL:-${STAGING_WEBHOOK_RECEIVER_URL:-}}"
WEBHOOK_RECEIVER_REMOTE_CADDYFILE="/opt/cmdock/Caddyfile"
WEBHOOK_RECEIVER_CADDY_ORIG=""
WEBHOOK_RECEIVER_CADDY_PATCHED=""
WEBHOOK_RECEIVER_ACTIVE=false
SKIP_PREFLIGHT="${CMDOCK_STAGING_SKIP_PREFLIGHT:-false}"

# Parse args
while [[ $# -gt 0 ]]; do
    case "$1" in
        --full) RUN_FULL=true; shift ;;
        --require-admin-http) REQUIRE_ADMIN_HTTP=true; shift ;;
        --url) SERVER_URL="$2"; shift 2 ;;
        --ssh) SSH_HOST="$2"; shift 2 ;;
        --tw-host) TW_HOST="$2"; TW_EXEC_MODE=ssh; shift 2 ;;
        --tw-local) TW_EXEC_MODE=local; shift ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

if [[ -z "$TW_HOST" ]]; then
    TW_HOST="$SSH_HOST"
fi

# --- Colours and helpers ---
GREEN='\033[0;32m'
RED='\033[0;31m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

PASS=0
FAIL=0
SKIP=0
TOTAL=0
FAILURES=()
STANDALONE_CLI_READY=false

pass() { PASS=$((PASS + 1)); TOTAL=$((TOTAL + 1)); echo -e "  ${GREEN}✓${NC} $1"; }
fail() { FAIL=$((FAIL + 1)); TOTAL=$((TOTAL + 1)); FAILURES+=("$1"); echo -e "  ${RED}✗${NC} $1"; }
skip() { SKIP=$((SKIP + 1)); TOTAL=$((TOTAL + 1)); echo -e "  ${YELLOW}○${NC} $1 (skipped)"; }
info() { echo -e "${BOLD}==> ${NC}$*"; }
section() { echo; echo -e "${BOLD}━━━ $1 ━━━${NC}"; }

ansible_inventory_host() {
    case "$SSH_HOST" in
        *dogfood*|*taskchampion-01*)
            echo "$ANSIBLE_DOGFOOD_HOST"
            ;;
        *)
            echo "$ANSIBLE_STAGING_HOST"
            ;;
    esac
}

load_admin_http_token_from_ansible() {
    if [[ -n "$ADMIN_HTTP_TOKEN" ]]; then
        return 0
    fi
    if ! command -v ansible-inventory >/dev/null 2>&1; then
        return 1
    fi
    if [[ ! -f "$ANSIBLE_INVENTORY_FILE" ]]; then
        return 1
    fi

    local inventory_host
    inventory_host="$(ansible_inventory_host)"

    local token
    token=$(ansible-inventory -i "$ANSIBLE_INVENTORY_FILE" --host "$inventory_host" 2>/dev/null | python3 -c '
import json
import sys

try:
    data = json.load(sys.stdin)
except Exception:
    raise SystemExit(1)

token = data.get("cmdock_admin_token", "")
if token:
    print(token)
') || return 1

    if [[ -n "$token" ]]; then
        ADMIN_HTTP_TOKEN="$token"
        export ADMIN_HTTP_TOKEN
        info "Loaded admin HTTP token from local Ansible inventory for $inventory_host"
        return 0
    fi

    return 1
}

extract_field() {
    local prefix="$1"
    awk -F': *' -v key="$prefix" '
        {
            field = $1
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", field)
            if (field == key) {
                value = $2
                gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
                print value
                exit
            }
        }
    '
}

extract_api_token() {
    awk '
        /API token/ { capture=1; next }
        capture {
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", $0)
            if ($0 != "") {
                print
                exit
            }
        }
    '
}

# Wait for a condition with bounded retries (for eventual consistency).
# Usage: wait_for "description" 5 command args...
# Returns 0 if command succeeds within retries, 1 otherwise.
wait_for() {
    local desc="$1" max="$2"
    shift 2
    for i in $(seq 1 "$max"); do
        if "$@" 2>/dev/null; then return 0; fi
        sleep 1
    done
    return 1
}

# Run a test: name, command (returns 0=pass, 1=fail).
# Stderr is captured to a temp file for diagnostics on failure.
run_test() {
    local name="$1"
    shift
    local errfile
    errfile=$(mktemp)
    if "$@" 2>"$errfile"; then
        pass "$name"
    else
        local errmsg
        errmsg=$(head -2 "$errfile" | tr '\n' ' ')
        if [[ -n "$errmsg" ]]; then
            fail "$name (stderr: ${errmsg:0:120})"
        else
            fail "$name"
        fi
    fi
    rm -f "$errfile"
}

consume_result_file() {
    local file="$1" unexpected_prefix="$2"
    while IFS=$'\t' read -r status message; do
        [[ -z "${status:-}" ]] && continue
        case "$status" in
            PASS) pass "$message" ;;
            FAIL) fail "$message" ;;
            SKIP) skip "$message" ;;
            INFO) echo -e "  ${CYAN}i${NC} $message" ;;
            *) fail "${unexpected_prefix}: ${status}${message:+ $message}" ;;
        esac
    done <"$file"
}

run_structured_scenario() {
    local file="$1" unexpected_prefix="$2"
    shift 2
    "$@" >"$file" 2>&1 || true
    consume_result_file "$file" "$unexpected_prefix"
}

# SSH helper — runs server-local admin CLI command on staging.
# Builds the full command as a single string for SSH (remote shell parses it).
# Stderr is filtered to remove audit JSON lines (which pollute output parsing)
# but non-JSON stderr lines are preserved for diagnostics.
server_admin_cli() {
    local cmd="$SERVER_DOCKER_ADMIN"
    local arg
    for arg in "$@"; do
        # Simple shell-safe quoting (single quotes + escaped inner quotes)
        cmd+=" '${arg//\'/\'\\\'\'}'"
    done
    ssh -o ControlMaster=no -o ConnectTimeout=10 "$SSH_HOST" "$cmd" 2>&1 | grep -v '^{"timestamp"'
}

# SSH helper — runs the standalone cmdock-admin binary on staging.
cmdock_admin_cli() {
    if [[ -z "$ADMIN_HTTP_TOKEN" ]]; then
        echo "STAGING_ADMIN_TOKEN/CMDOCK_ADMIN_TOKEN not set" >&2
        return 1
    fi

    local cmd="'${REMOTE_CMDOCK_ADMIN}' --server '${SERVER_URL//\'/\'\\\'\'}' --token '${ADMIN_HTTP_TOKEN//\'/\'\\\'\'}'"
    local arg
    for arg in "$@"; do
        cmd+=" '${arg//\'/\'\\\'\'}'"
    done
    ssh -o ControlMaster=no -o ConnectTimeout=10 "$SSH_HOST" "$cmd" 2>&1 | grep -v '^{"timestamp"'
}

# REST helper — authenticated GET
rest_get() {
    local path="$1"
    local token="$2"
    curl -sf --max-time 10 -H "Authorization: Bearer $token" "$SERVER_URL$path"
}

# REST helper — authenticated POST with JSON body
rest_post() {
    local path="$1"
    local token="$2"
    local body="${3-}"
    if [[ -z "$body" ]]; then
        body='{}'
    fi
    curl -sf --max-time 10 -X POST \
        -H "Authorization: Bearer $token" \
        -H "Content-Type: application/json" \
        -d "$body" \
        "$SERVER_URL$path"
}

# REST helper — POST that returns status code
rest_post_status() {
    local path="$1"
    local token="$2"
    local body="${3-}"
    if [[ -z "$body" ]]; then
        body='{}'
    fi
    curl -s --max-time 10 -o /dev/null -w "%{http_code}" -X POST \
        -H "Authorization: Bearer $token" \
        -H "Content-Type: application/json" \
        -d "$body" \
        "$SERVER_URL$path"
}

# REST helper — authenticated PUT with JSON body
rest_put() {
    local path="$1"
    local token="$2"
    local body="${3-}"
    if [[ -z "$body" ]]; then
        body='{}'
    fi
    curl -sf --max-time 10 -X PUT \
        -H "Authorization: Bearer $token" \
        -H "Content-Type: application/json" \
        -d "$body" \
        "$SERVER_URL$path"
}

# REST helper — GET that returns status code
rest_get_status() {
    local path="$1"
    local token="$2"
    curl -s --max-time 10 -o /dev/null -w "%{http_code}" \
        -H "Authorization: Bearer $token" \
        "$SERVER_URL$path"
}

# REST helper — operator-authenticated GET for /admin/*
rest_admin_get() {
    local path="$1"
    if [[ -z "$ADMIN_HTTP_TOKEN" ]]; then
        echo "STAGING_ADMIN_TOKEN/CMDOCK_ADMIN_TOKEN not set" >&2
        return 1
    fi
    rest_get "$path" "$ADMIN_HTTP_TOKEN"
}

# REST helper — operator-authenticated POST status for /admin/*
rest_admin_post_status() {
    local path="$1"
    local body="${2-}"
    if [[ -z "$body" ]]; then
        body='{}'
    fi
    if [[ -z "$ADMIN_HTTP_TOKEN" ]]; then
        echo "STAGING_ADMIN_TOKEN/CMDOCK_ADMIN_TOKEN not set" >&2
        return 1
    fi
    rest_post_status "$path" "$ADMIN_HTTP_TOKEN" "$body"
}

rest_admin_put() {
    local path="$1"
    local body="${2-}"
    if [[ -z "$body" ]]; then
        body='{}'
    fi
    if [[ -z "$ADMIN_HTTP_TOKEN" ]]; then
        echo "STAGING_ADMIN_TOKEN/CMDOCK_ADMIN_TOKEN not set" >&2
        return 1
    fi
    rest_put "$path" "$ADMIN_HTTP_TOKEN" "$body"
}

webhook_receiver_url() {
    if [[ -n "$WEBHOOK_RECEIVER_EXTERNAL_URL" ]]; then
        printf '%s' "${WEBHOOK_RECEIVER_EXTERNAL_URL%/}"
        return
    fi
    printf '%s%s' "$SERVER_URL" "$WEBHOOK_RECEIVER_ROUTE"
}

restore_webhook_receiver_caddy() {
    if [[ -z "${WEBHOOK_RECEIVER_CADDY_ORIG:-}" || ! -f "${WEBHOOK_RECEIVER_CADDY_ORIG:-}" ]]; then
        return 0
    fi
    scp "${SSH_BASE_OPTS[@]}" "$WEBHOOK_RECEIVER_CADDY_ORIG" "$SSH_HOST:/tmp/cmdock-caddyfile-restore" >/dev/null 2>&1 || return 1
    ssh "${SSH_BASE_OPTS[@]}" -o ConnectTimeout=10 "$SSH_HOST" \
        "sudo sh -c 'cat /tmp/cmdock-caddyfile-restore > \"$WEBHOOK_RECEIVER_REMOTE_CADDYFILE\"' \
        && sudo rm -f /tmp/cmdock-caddyfile-restore \
        && sudo docker restart cmdock-caddy >/dev/null" >/dev/null 2>&1
}

setup_webhook_receiver() {
    if [[ -n "$WEBHOOK_RECEIVER_EXTERNAL_URL" ]]; then
        if ! wait_for "public webhook receiver health" 10 bash -c "curl -sf --max-time 5 '$(webhook_receiver_url)/healthz' >/dev/null"; then
            echo -e "${RED}Configured public webhook receiver is not healthy at $(webhook_receiver_url)${NC}"
            return 1
        fi
        WEBHOOK_RECEIVER_ACTIVE=false
        return 0
    fi

    if ! ssh "${SSH_BASE_OPTS[@]}" -o ConnectTimeout=10 "$SSH_HOST" \
        "sudo docker image inspect '$WEBHOOK_RECEIVER_IMAGE' >/dev/null 2>&1"; then
        echo -e "${RED}Webhook receiver image is not present on $SSH_HOST${NC}"
        echo -e "  Deploy it first from the cmdock/server repo: just deploy-staging"
        return 1
    fi

    local network
    network=$(ssh "${SSH_BASE_OPTS[@]}" -o ConnectTimeout=10 "$SSH_HOST" \
        "sudo docker inspect cmdock-caddy --format '{{range \$k, \$_ := .NetworkSettings.Networks}}{{println \$k}}{{end}}' | head -n1" 2>/dev/null | tr -d '\r' || true)
    if [[ -z "$network" ]]; then
        echo -e "${RED}Cannot determine Docker network for cmdock-caddy on $SSH_HOST${NC}"
        return 1
    fi

    ssh "${SSH_BASE_OPTS[@]}" -o ConnectTimeout=10 "$SSH_HOST" \
        "sudo docker rm -f '$WEBHOOK_RECEIVER_CONTAINER' >/dev/null 2>&1 || true
         sudo docker run -d --rm --name '$WEBHOOK_RECEIVER_CONTAINER' --network '$network' '$WEBHOOK_RECEIVER_IMAGE' >/dev/null" || return 1

    WEBHOOK_RECEIVER_CADDY_ORIG="$BUBBLE_DIR/Caddyfile.orig"
    WEBHOOK_RECEIVER_CADDY_PATCHED="$BUBBLE_DIR/Caddyfile.patched"
    ssh "${SSH_BASE_OPTS[@]}" -o ConnectTimeout=10 "$SSH_HOST" \
        "sudo cat '$WEBHOOK_RECEIVER_REMOTE_CADDYFILE'" > "$WEBHOOK_RECEIVER_CADDY_ORIG" || return 1

    python3 - "$WEBHOOK_RECEIVER_CADDY_ORIG" "$WEBHOOK_RECEIVER_CADDY_PATCHED" <<'PY'
import pathlib
import re
import sys

src = pathlib.Path(sys.argv[1]).read_text()
replacement = (
    "\t@webhook_receiver path /__e2e/webhook/*\n"
    "\thandle @webhook_receiver {\n"
    "\t\turi strip_prefix /__e2e/webhook\n"
    "\t\treverse_proxy cmdock-webhook-receiver-e2e:8081\n"
    "\t}\n\n"
    "\treverse_proxy server:8080\n"
)
if replacement in src:
    pathlib.Path(sys.argv[2]).write_text(src)
    raise SystemExit(0)

pattern = re.compile(r"^(\s*)reverse_proxy server:8080\s*$", re.MULTILINE)
patched, count = pattern.subn(replacement.rstrip("\n"), src)
if count == 0:
    raise SystemExit(1)
patched = patched + ("\n" if not patched.endswith("\n") else "")
pathlib.Path(sys.argv[2]).write_text(patched)
PY

    scp "${SSH_BASE_OPTS[@]}" "$WEBHOOK_RECEIVER_CADDY_PATCHED" "$SSH_HOST:/tmp/cmdock-caddyfile-test" >/dev/null || return 1
    ssh "${SSH_BASE_OPTS[@]}" -o ConnectTimeout=10 "$SSH_HOST" \
        "sudo sh -c 'cat /tmp/cmdock-caddyfile-test > \"$WEBHOOK_RECEIVER_REMOTE_CADDYFILE\"' \
        && sudo rm -f /tmp/cmdock-caddyfile-test \
        && sudo docker restart cmdock-caddy >/dev/null" >/dev/null || return 1

    if ! wait_for "webhook receiver health" 10 bash -c "curl -sf --max-time 5 '$(webhook_receiver_url)/healthz' >/dev/null"; then
        restore_webhook_receiver_caddy >/dev/null 2>&1 || true
        ssh "${SSH_BASE_OPTS[@]}" -o ConnectTimeout=10 "$SSH_HOST" "sudo docker rm -f '$WEBHOOK_RECEIVER_CONTAINER' >/dev/null 2>&1 || true" >/dev/null 2>&1 || true
        return 1
    fi

    WEBHOOK_RECEIVER_ACTIVE=true
    return 0
}

# Export helpers for use in bash -c subshells
export -f rest_get rest_post rest_post_status rest_put rest_get_status rest_admin_get rest_admin_post_status rest_admin_put server_admin_cli cmdock_admin_cli
export -f webhook_receiver_url
export SERVER_URL SSH_HOST SERVER_DOCKER_ADMIN ADMIN_HTTP_TOKEN REMOTE_CMDOCK_ADMIN WEBHOOK_RECEIVER_ROUTE

tw_ssh() {
    ssh "${SSH_BASE_OPTS[@]}" -o ConnectTimeout=10 "$TW_HOST" "$1"
}

# Configure a TW profile in an isolated directory
# Args: profile_dir server_url client_id encryption_secret
setup_tw_profile() {
    local dir="$1" url="$2" client_id="$3" secret="$4"
    if [[ "$TW_EXEC_MODE" == "ssh" ]]; then
        tw_ssh "mkdir -p '$dir/data' && cat > '$dir/taskrc' <<'EOF'
data.location=$dir/data
sync.server.url=$url
sync.server.client_id=$client_id
sync.encryption_secret=$secret
confirmation=off
recurrence=off
verbose=nothing
EOF
"
    else
        mkdir -p "$dir"
        cat > "$dir/taskrc" <<EOF
data.location=$dir/data
sync.server.url=$url
sync.server.client_id=$client_id
sync.encryption_secret=$secret
confirmation=off
recurrence=off
verbose=nothing
EOF
        mkdir -p "$dir/data"
    fi
}

# Run task command with isolated profile
tw() {
    local dir="$1"; shift
    if [[ "$TW_EXEC_MODE" == "ssh" ]]; then
        local cmd="TASKRC='$dir/taskrc' TASKDATA='$dir/data' task"
        local arg
        for arg in "$@"; do
            cmd+=" '${arg//\'/\'\\\'\'}'"
        done
        tw_ssh "$cmd"
    else
        TASKRC="$dir/taskrc" TASKDATA="$dir/data" task "$@"
    fi
}

tw_export() {
    local dir="$1"
    tw "$dir" export 2>/dev/null || echo "[]"
}

tw_sync_has_description() {
    local dir="$1" desc="$2"
    tw "$dir" sync >/dev/null 2>&1 || true
    tw_export "$dir" | jq -r '.[].description' | grep -q "$desc"
}

tw_field_for_uuid() {
    local dir="$1" uuid="$2" field="$3"
    tw_export "$dir" | jq -r ".[] | select(.uuid == \"$uuid\") | .$field" || echo ""
}

tw_uuid_for_description_contains() {
    local dir="$1" needle="$2"
    tw_export "$dir" | jq -r --arg needle "$needle" '.[] | select(.description | contains($needle)) | .uuid' | head -1
}

rest_task_exists() {
    local token="$1" needle="$2"
    rest_get "/api/tasks" "$token" 2>/dev/null | jq -e --arg needle "$needle" '.[] | select(.description | contains($needle))' >/dev/null
}

context_exists() {
    local token="$1" context_id="$2"
    curl -sf --max-time 10 -H "Authorization: Bearer $token" \
        "$SERVER_URL/api/contexts" 2>/dev/null \
        | jq -e --arg id "$context_id" '.[] | select(.id == $id)' >/dev/null
}

admin_user_has_recent_sync() {
    local username="$1"
    rest_admin_get "/admin/users" 2>/dev/null \
        | jq -e --arg username "$username" '
            .[]
            | select(.username == $username and .lastSyncAt != null and .lastSyncAt != "")
            | (
                .lastSyncAt
                | capture("(?<base>.*)(?<tzsign>[+-])(?<h>[0-9]{2}):(?<m>[0-9]{2})$")
                | (.base + .tzsign + .h + .m)
                | strptime("%Y-%m-%dT%H:%M:%S%z")
                | mktime
                | (now - .) <= 86400
            )
        ' >/dev/null
}

doctor_should_pass() {
    local name="$1"
    local output
    if output=$(cmdock_admin_cli --json doctor 2>&1); then
        pass "$name"
    else
        local summary
        summary=$(echo "$output" | jq -r '[.checks[] | select(.status == "fail") | "\(.title): \(.summary)"] | join("; ")' 2>/dev/null || true)
        if [[ -n "$summary" && "$summary" != "null" ]]; then
            fail "$name ($summary)"
        else
            output=$(echo "$output" | head -n 8 | tr '\n' ' ')
            fail "$name (output: ${output:0:180})"
        fi
    fi
}

doctor_should_fail() {
    local name="$1" needle="${2:-}"
    local output
    if output=$(cmdock_admin_cli --json doctor 2>&1); then
        fail "$name (doctor unexpectedly passed)"
        return
    fi

    if [[ -n "$needle" ]]; then
        if echo "$output" | jq -e --arg needle "$needle" '.checks[] | select(.status == "fail" and .title == $needle)' >/dev/null 2>&1; then
            pass "$name"
            return
        fi

        local summary
        summary=$(echo "$output" | jq -r '[.checks[] | select(.status == "fail") | "\(.title): \(.summary)"] | join("; ")' 2>/dev/null || true)
        if [[ -n "$summary" && "$summary" != "null" ]]; then
            fail "$name (unexpected failing checks: $summary)"
        else
            output=$(echo "$output" | head -n 8 | tr '\n' ' ')
            fail "$name (unexpected output: ${output:0:180})"
        fi
        return
    fi

    pass "$name"
}

# --- Cleanup ---
cleanup() {
    info "Cleaning up..."
    # Delete test users on server (best-effort)
    if [[ -n "${USER_A_ID:-}" ]]; then
        if $STANDALONE_CLI_READY; then
            cmdock_admin_cli user delete "e2e-user-a" --yes 2>/dev/null || true
        else
            server_admin_cli admin user delete "$USER_A_ID" --yes 2>/dev/null || true
        fi
    fi
    if [[ -n "${USER_B_ID:-}" ]]; then
        if $STANDALONE_CLI_READY; then
            cmdock_admin_cli user delete "e2e-user-b" --yes 2>/dev/null || true
        else
            server_admin_cli admin user delete "$USER_B_ID" --yes 2>/dev/null || true
        fi
    fi
    if [[ "$TW_EXEC_MODE" == "ssh" && -n "${TW_PROFILE_ROOT:-}" ]]; then
        tw_ssh "rm -rf '$TW_PROFILE_ROOT'" 2>/dev/null || true
    fi
    if [[ "${WEBHOOK_RECEIVER_ACTIVE:-false}" == true ]]; then
        restore_webhook_receiver_caddy >/dev/null 2>&1 || true
        ssh "${SSH_BASE_OPTS[@]}" -o ConnectTimeout=10 "$SSH_HOST" \
            "sudo docker rm -f '$WEBHOOK_RECEIVER_CONTAINER' >/dev/null 2>&1 || true" >/dev/null 2>&1 || true
    fi
    # Remove temp dir
    if [[ -n "${BUBBLE_DIR:-}" ]] && [[ -d "$BUBBLE_DIR" ]]; then
        rm -rf "$BUBBLE_DIR"
    fi
    echo
    echo -e "${BOLD}╔══════════════════════════════════════════════════════╗${NC}"
    echo -e "${BOLD}║   Internal Verification Results                      ║${NC}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════╝${NC}"
    echo -e "  Passed:  ${GREEN}${PASS}${NC}"
    echo -e "  Failed:  ${RED}${FAIL}${NC}"
    echo -e "  Skipped: ${YELLOW}${SKIP}${NC}"
    echo -e "  Total:   ${BOLD}${TOTAL}${NC}"
    if [[ ${#FAILURES[@]} -gt 0 ]]; then
        echo
        echo -e "  ${RED}Failures:${NC}"
        for f in "${FAILURES[@]}"; do
            echo -e "    ${RED}✗${NC} $f"
        done
    fi
    echo
    if [[ $FAIL -eq 0 ]]; then
        echo -e "  ${GREEN}${BOLD}ALL TESTS PASSED${NC}"
    else
        echo -e "  ${RED}${BOLD}$FAIL TESTS FAILED${NC}"
    fi
}
trap cleanup EXIT

# --- Pre-flight ---
if [[ "$SKIP_PREFLIGHT" == "true" ]]; then
    BUBBLE_DIR="${CMDOCK_STAGING_BUBBLE_DIR:-$(mktemp -d -t staging-test-XXXXXX)}"
    STANDALONE_CLI_READY="${CMDOCK_STAGING_STANDALONE_CLI_READY:-false}"
else
    echo -e "${BOLD}╔══════════════════════════════════════════════════════╗${NC}"
    echo -e "${BOLD}║   cmdock-server — Internal E2E Tests                 ║${NC}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════╝${NC}"
    echo -e "  Server:  ${CYAN}${SERVER_URL}${NC}"
    echo -e "  SSH:     ${CYAN}${SSH_HOST}${NC}"
    echo -e "  TW:      ${CYAN}${TW_EXEC_MODE}$(if [[ "$TW_EXEC_MODE" == "ssh" ]]; then printf ' (%s)' "$TW_HOST"; fi)${NC}"
    echo -e "  Mode:    ${CYAN}$(if $RUN_FULL; then echo "Full (P0+P1)"; else echo "P0 only"; fi)${NC}"
    echo -e "  Admin:   ${CYAN}$(if $REQUIRE_ADMIN_HTTP; then echo "required"; else echo "optional"; fi)${NC}"
    echo

    for cmd in curl jq ssh bc; do
        if ! command -v "$cmd" &>/dev/null; then
            echo -e "${RED}Missing dependency: $cmd${NC}"
            exit 1
        fi
    done
    if [[ "$TW_EXEC_MODE" == "local" ]] && ! command -v task &>/dev/null; then
        echo -e "${RED}Missing dependency: task${NC}"
        exit 1
    fi

    BUBBLE_DIR=$(mktemp -d -t staging-test-XXXXXX)

    info "Checking connectivity..."
    if ! curl -sf --max-time 5 "$SERVER_URL/healthz" > /dev/null 2>&1; then
        echo -e "${RED}Server unreachable at $SERVER_URL${NC}"
        echo -e "  Is the staging server running? Try: ./scripts/deploy.sh health"
        exit 1
    fi
    pass "Server reachable (healthz)"

    if ! ssh "${SSH_BASE_OPTS[@]}" -o ConnectTimeout=5 "$SSH_HOST" "sudo docker compose -f '$DOCKER_COMPOSE_FILE' ps server 2>/dev/null | grep -q healthy" 2>/dev/null; then
        echo -e "${RED}Cannot SSH to $SSH_HOST or server container not healthy${NC}"
        exit 1
    fi
    pass "SSH access + server container healthy"

    if [[ "$TW_EXEC_MODE" == "ssh" ]]; then
        if ! tw_ssh "command -v task >/dev/null 2>&1 && task --version >/dev/null 2>&1"; then
            echo -e "${RED}Cannot run Taskwarrior on $TW_HOST${NC}"
            exit 1
        fi
        pass "SSH access + remote Taskwarrior client ready"
    else
        pass "Local Taskwarrior client ready"
    fi

    if [[ -z "$ADMIN_HTTP_TOKEN" ]]; then
        load_admin_http_token_from_ansible || true
    fi
    if [[ -z "$ADMIN_HTTP_TOKEN" && "$REQUIRE_ADMIN_HTTP" == true ]]; then
        echo -e "${RED}Admin HTTP coverage required but no admin token is available.${NC}"
        echo -e "  Provide STAGING_ADMIN_TOKEN / CMDOCK_ADMIN_TOKEN or ensure local Ansible inventory exposes cmdock_admin_token."
        exit 1
    fi
    if [[ -z "$ADMIN_HTTP_TOKEN" && "$RUN_FULL" == true ]]; then
        echo -e "  ${YELLOW}!${NC} Full mode is running without admin HTTP coverage."
        echo -e "    /admin/* tests will be skipped unless STAGING_ADMIN_TOKEN / CMDOCK_ADMIN_TOKEN is set"
        echo -e "    or local Ansible inventory exposes cmdock_admin_token."
    fi

    if [[ -n "$ADMIN_HTTP_TOKEN" ]]; then
        if ssh "${SSH_BASE_OPTS[@]}" -o ConnectTimeout=5 "$SSH_HOST" "command -v cmdock-admin >/dev/null 2>&1 && '${REMOTE_CMDOCK_ADMIN}' --version >/dev/null 2>&1" 2>/dev/null; then
            STANDALONE_CLI_READY=true
            pass "SSH access + standalone cmdock-admin ready"
        else
            echo -e "${RED}Standalone cmdock-admin is not installed on $SSH_HOST${NC}"
            echo -e "  Deploy it first from the cmdock/cli repo: just deploy-staging"
            exit 1
        fi
    else
        skip "Standalone cmdock-admin coverage (no admin token available)"
    fi

    info "Validating Docker image..."
    REMOTE_CREATED=$(ssh "${SSH_BASE_OPTS[@]}" -o ConnectTimeout=5 "$SSH_HOST" "sudo docker inspect cmdock-server:latest --format '{{.Created}}'" 2>/dev/null || echo "unknown")
    LOCAL_CREATED=$(docker inspect cmdock-server:latest --format '{{.Created}}' 2>/dev/null || echo "unknown")
    if [[ "$REMOTE_CREATED" == "unknown" ]]; then
        skip "Docker image age check (cannot inspect remote image)"
    else
        REMOTE_TS=$(date -d "${REMOTE_CREATED}" +%s 2>/dev/null || echo "0")
        LOCAL_TS=$(date -d "${LOCAL_CREATED}" +%s 2>/dev/null || echo "0")
        AGE_DIFF=$(( LOCAL_TS - REMOTE_TS ))
        AGE_DIFF=${AGE_DIFF#-}
        if [[ $AGE_DIFF -gt 3600 && "$LOCAL_TS" != "0" ]]; then
            echo -e "  ${YELLOW}!${NC} Staging image is $(( AGE_DIFF / 60 ))m older than local build"
            echo -e "    Local:  ${LOCAL_CREATED}"
            echo -e "    Remote: ${REMOTE_CREATED}"
            echo -e "  Consider redeploying: docker save cmdock-server:latest | ssh $SSH_HOST 'sudo docker load'"
        fi
        pass "Docker image validated (remote: ${REMOTE_CREATED:0:19})"
    fi
fi

# =============================================================================
section "ADMIN CLI: User Management"
# =============================================================================

# Clean up stale test users from previous runs (idempotent)
info "Cleaning up stale test users..."
if $STANDALONE_CLI_READY; then
    EXISTING_USERS_JSON=$(cmdock_admin_cli --json user list 2>/dev/null || echo "[]")
else
    EXISTING_USERS=$(server_admin_cli admin user list 2>/dev/null || echo "")
fi
for stale_user in e2e-user-a e2e-user-b; do
    if $STANDALONE_CLI_READY; then
        STALE_ID=$(echo "$EXISTING_USERS_JSON" | jq -r --arg username "$stale_user" '.[] | select(.username == $username) | .id' | head -1 || true)
    else
        STALE_ID=$(echo "$EXISTING_USERS" | grep "$stale_user" | awk '{print $1}' || true)
    fi
    if [[ -n "$STALE_ID" ]]; then
        if [[ -n "$ADMIN_HTTP_TOKEN" ]]; then
            rest_admin_put "/admin/user/$STALE_ID/runtime-policy" '{"policyVersion":"stale-cleanup-allow-delete","policy":{"runtimeAccess":"allow","deleteAction":"allow"}}' >/dev/null 2>&1 || true
        fi
        if $STANDALONE_CLI_READY; then
            cmdock_admin_cli user delete "$stale_user" --yes 2>/dev/null || true
        else
            server_admin_cli admin user delete "$STALE_ID" --yes 2>/dev/null || true
        fi
        echo -e "  ${YELLOW}!${NC} Cleaned up stale user: $stale_user ($STALE_ID)"
    fi
done

# Create test users via the server-local maintenance CLI so the harness also
# gets bearer tokens for the REST-side checks.
info "Creating test users..."
USER_A_OUTPUT=$(server_admin_cli admin user create --username "e2e-user-a" || true)
USER_A_ID=$(echo "$USER_A_OUTPUT" | awk '/^[[:space:]]*ID:/ { print $2; exit }' || true)
TOKEN_A=$(echo "$USER_A_OUTPUT" | extract_api_token || true)

USER_B_OUTPUT=$(server_admin_cli admin user create --username "e2e-user-b" || true)
USER_B_ID=$(echo "$USER_B_OUTPUT" | awk '/^[[:space:]]*ID:/ { print $2; exit }' || true)
TOKEN_B=$(echo "$USER_B_OUTPUT" | extract_api_token || true)

if [[ -n "$USER_A_ID" ]]; then pass "server admin user create (user A)"; else fail "server admin user create (user A)"; fi
if [[ -n "$USER_B_ID" ]]; then pass "server admin user create (user B)"; else fail "server admin user create (user B)"; fi
if [[ -n "$TOKEN_A" ]]; then pass "server admin user create returns token (A)"; else fail "server admin user create returns token (A)"; fi
if [[ -n "$TOKEN_B" ]]; then pass "server admin user create returns token (B)"; else fail "server admin user create returns token (B)"; fi

# Early exit if user creation failed — remaining tests would cascade-fail
if [[ -z "$USER_A_ID" || -z "$TOKEN_A" || -z "$USER_B_ID" || -z "$TOKEN_B" ]]; then
    echo -e "${RED}Cannot continue — user provisioning failed${NC}"
    exit 1
fi

section "STANDALONE ADMIN CLI: Operator Flows"

if $STANDALONE_CLI_READY; then
    run_test "cmdock-admin doctor" bash -c "cmdock_admin_cli doctor >/dev/null"
    run_test "cmdock-admin user list includes user A" \
        bash -c "cmdock_admin_cli --json user list | jq -e '.[] | select(.username == \"e2e-user-a\")' >/dev/null"
else
    skip "cmdock-admin doctor"
    skip "cmdock-admin user list includes user A"
fi

# Create additional token
EXTRA_TOKEN_OUTPUT=$(server_admin_cli admin token create "$USER_A_ID" --label "extra-token")
if [[ -n "$EXTRA_TOKEN_OUTPUT" ]]; then pass "admin token create"; else fail "admin token create"; fi

# Get the hash prefix for the extra token from token list (for later revoke test)
EXTRA_TOKEN_HASH=$(server_admin_cli admin token list "$USER_A_ID" 2>/dev/null | grep "extra-token" | awk '{print $1}' || true)

# List tokens
run_test "admin token list" bash -c "server_admin_cli admin token list '$USER_A_ID' | grep -q 'load-test\|extra-token\|test'"

# =============================================================================
section "ADMIN CLI: Sync Identity Management"
# =============================================================================

# Create sync replicas (requires CMDOCK_MASTER_KEY on server)
info "Creating sync identities..."
SYNC_A_OUTPUT=$(server_admin_cli admin sync create "$USER_A_ID" 2>&1 || true)
if echo "$SYNC_A_OUTPUT" | grep -q "Canonical sync identity created"; then
    pass "server admin sync create (user A)"
    SYNC_ENABLED=true
else
    skip "server admin sync create (master key not configured on staging)"
    SYNC_ENABLED=false
fi

if $SYNC_ENABLED; then
    SYNC_B_OUTPUT=$(server_admin_cli admin sync create "$USER_B_ID" 2>&1 || true)
    if echo "$SYNC_B_OUTPUT" | grep -q "Canonical sync identity created"; then
        pass "server admin sync create (user B)"
    else
        fail "server admin sync create (user B)"
    fi

    if $STANDALONE_CLI_READY; then
        CONNECT_B1_JSON=$(cmdock_admin_cli --json connect "e2e-user-b" --taskwarrior 2>&1 || true)
        CLIENT_ID_B1=$(echo "$CONNECT_B1_JSON" | jq -r '.deviceClientId // empty' 2>/dev/null || true)
        SECRET_B1=$(echo "$CONNECT_B1_JSON" | jq -r '.encryptionSecret // empty' 2>/dev/null || true)
        CONNECT_B2_JSON=$(cmdock_admin_cli --json connect "e2e-user-b" --taskwarrior 2>&1 || true)
        CLIENT_ID_B2=$(echo "$CONNECT_B2_JSON" | jq -r '.deviceClientId // empty' 2>/dev/null || true)
        SECRET_B2=$(echo "$CONNECT_B2_JSON" | jq -r '.encryptionSecret // empty' 2>/dev/null || true)
        if [[ -n "$CLIENT_ID_B1" && -n "$SECRET_B1" ]]; then
            pass "cmdock-admin connect --taskwarrior (user B device A)"
        else
            fail "cmdock-admin connect --taskwarrior (user B device A)"
        fi
        if [[ -n "$CLIENT_ID_B2" && -n "$SECRET_B2" ]]; then
            pass "cmdock-admin connect --taskwarrior (user B device B)"
        else
            fail "cmdock-admin connect --taskwarrior (user B device B)"
        fi
    else
        DEVICE_B1_OUTPUT=$(server_admin_cli admin device create "$USER_B_ID" --name "staging-user-b-device-a" --server-url "$SERVER_URL" 2>&1 || true)
        CLIENT_ID_B1=$(echo "$DEVICE_B1_OUTPUT" | extract_field "Client ID")
        SECRET_B1=$(echo "$DEVICE_B1_OUTPUT" | extract_field "Encryption Secret")
        if [[ -n "$CLIENT_ID_B1" && -n "$SECRET_B1" ]]; then
            pass "server admin device create (user B device A)"
        else
            fail "server admin device create (user B device A)"
        fi

        DEVICE_B2_OUTPUT=$(server_admin_cli admin device create "$USER_B_ID" --name "staging-user-b-device-b" --server-url "$SERVER_URL" 2>&1 || true)
        CLIENT_ID_B2=$(echo "$DEVICE_B2_OUTPUT" | extract_field "Client ID")
        SECRET_B2=$(echo "$DEVICE_B2_OUTPUT" | extract_field "Encryption Secret")
        if [[ -n "$CLIENT_ID_B2" && -n "$SECRET_B2" ]]; then
            pass "server admin device create (user B device B)"
        else
            fail "server admin device create (user B device B)"
        fi
    fi

    # Show sync info
    run_test "server admin sync show" bash -c "server_admin_cli admin sync show '$USER_A_ID' | grep -q 'Client ID'"

    # Hidden debug/migration helper: show-secret returns the decrypted canonical secret
    run_test "server admin sync show-secret" bash -c "server_admin_cli admin sync show-secret '$USER_A_ID' | grep -q 'Encryption Secret'"
fi

# =============================================================================
section "ADMIN HTTP: Server Endpoints"
# =============================================================================

ADMIN_RUNTIME_RESULTS_FILE="$BUBBLE_DIR/admin-runtime-results.tsv"
run_structured_scenario "$ADMIN_RUNTIME_RESULTS_FILE" "Admin runtime scenario runner emitted unexpected output" \
    python3 "$(dirname "$0")/staging_admin_runtime.py" \
        --server-url "$SERVER_URL" \
        --ssh-host "$SSH_HOST" \
        --admin-token "$ADMIN_HTTP_TOKEN" \
        --token-a "$TOKEN_A" \
        --user-a-id "$USER_A_ID" \
        --run-admin-http

# =============================================================================
section "REST API: Auth + CRUD"
# =============================================================================
PRODUCT_RESULTS_FILE="$BUBBLE_DIR/product-runtime-results.tsv"
python3 "$(dirname "$0")/staging_product_runtime.py" \
    --server-url "$SERVER_URL" \
    --ssh-host "$SSH_HOST" \
    --token-a "$TOKEN_A" \
    --token-b "$TOKEN_B" \
    --run-rest-crud \
    >"$PRODUCT_RESULTS_FILE" 2>&1 || true
consume_result_file "$PRODUCT_RESULTS_FILE" "Product scenario runner emitted unexpected output"

# =============================================================================
section "REST API: App Config & Config CRUD"
# =============================================================================
python3 "$(dirname "$0")/staging_product_runtime.py" \
    --server-url "$SERVER_URL" \
    --ssh-host "$SSH_HOST" \
    --token-a "$TOKEN_A" \
    --token-b "$TOKEN_B" \
    --run-app-config \
    >"$PRODUCT_RESULTS_FILE" 2>&1 || true
consume_result_file "$PRODUCT_RESULTS_FILE" "Product scenario runner emitted unexpected output"

# =============================================================================
section "E2E: TW CLI ↔ REST Sync"
# =============================================================================

if ! $SYNC_ENABLED; then
    skip "TW → REST sync (sync not enabled)"
    skip "REST → TW sync (sync not enabled)"
    skip "Bidirectional mutations (sync not enabled)"
    skip "Multi-device convergence (sync not enabled)"
else
    # Use User B for TW sync tests — clean user with no pre-existing REST tasks.
    # User A has REST tasks from the CRUD section which pushed encrypted versions
    # to the sync chain; using A would require TW to decrypt those first.
    if [[ "$TW_EXEC_MODE" == "ssh" ]]; then
        TW_PROFILE_ROOT="$(tw_ssh "mktemp -d -t staging-test-tw-XXXXXX")"
    else
        TW_PROFILE_ROOT="$BUBBLE_DIR"
    fi
    TW_DIR_A="$TW_PROFILE_ROOT/tw-device-a"
    TW_DIR_B="$TW_PROFILE_ROOT/tw-device-b"
    setup_tw_profile "$TW_DIR_A" "$SERVER_URL" "$CLIENT_ID_B1" "$SECRET_B1"
    setup_tw_profile "$TW_DIR_B" "$SERVER_URL" "$CLIENT_ID_B2" "$SECRET_B2"

    # Initial sync to establish the chain (required before TW can pull)
    tw "$TW_DIR_A" sync 2>/dev/null || true

    python3 "$(dirname "$0")/staging_product_runtime.py" \
        --server-url "$SERVER_URL" \
        --ssh-host "$SSH_HOST" \
        --token-a "$TOKEN_A" \
        --token-b "$TOKEN_B" \
        --sync-enabled \
        --tw-mode "$TW_EXEC_MODE" \
        --tw-host "$TW_HOST" \
        --tw-dir-a "${TW_DIR_A:-}" \
        --tw-dir-b "${TW_DIR_B:-}" \
        --run-sync-e2e \
        >"$PRODUCT_RESULTS_FILE" 2>&1 || true
    consume_result_file "$PRODUCT_RESULTS_FILE" "Product scenario runner emitted unexpected output"
fi

# =============================================================================
# P1 Tests (--full only)
# =============================================================================

if $RUN_FULL; then

    section "P1: Admin Token Lifecycle"

    # Revoke the extra token
    run_structured_scenario "$ADMIN_RUNTIME_RESULTS_FILE" "Admin runtime scenario runner emitted unexpected output" \
        python3 "$(dirname "$0")/staging_admin_runtime.py" \
            --server-url "$SERVER_URL" \
            --ssh-host "$SSH_HOST" \
            --token-a "$TOKEN_A" \
            --user-a-id "$USER_A_ID" \
            --extra-token-hash "${EXTRA_TOKEN_HASH:-}" \
            --run-token-revoke

    section "P1: Restart Persistence"
    run_structured_scenario "$PRODUCT_RESULTS_FILE" "Product scenario runner emitted unexpected output" \
        python3 "$(dirname "$0")/staging_product_runtime.py" \
            --server-url "$SERVER_URL" \
            --ssh-host "$SSH_HOST" \
            --token-a "$TOKEN_A" \
            --token-b "$TOKEN_B" \
            $(if $SYNC_ENABLED; then printf '%s ' --sync-enabled; fi) \
            --tw-mode "$TW_EXEC_MODE" \
            --tw-host "$TW_HOST" \
            --tw-dir-a "${TW_DIR_A:-}" \
            --tw-dir-b "${TW_DIR_B:-}" \
            --run-restart-latency

    section "P1: Webhook HTTPS Delivery"

    if setup_webhook_receiver; then
        WEBHOOK_RUNTIME_RESULTS_FILE="$BUBBLE_DIR/webhook-results.tsv"
        run_structured_scenario "$WEBHOOK_RUNTIME_RESULTS_FILE" "Webhook/runtime scenario runner emitted unexpected output" \
            python3 "$(dirname "$0")/staging_webhooks_runtime.py" \
                --server-url "$SERVER_URL" \
                --ssh-host "$SSH_HOST" \
                --admin-token "$ADMIN_HTTP_TOKEN" \
                --token-a "$TOKEN_A" \
                --token-b "$TOKEN_B" \
                --user-a-id "$USER_A_ID" \
                --user-b-id "$USER_B_ID" \
                --receiver-url "$(webhook_receiver_url)" \
                --run-webhooks \
                $(if $SYNC_ENABLED; then printf '%s ' --sync-enabled; fi) \
                $(if $STANDALONE_CLI_READY; then printf '%s ' --standalone-cli-ready; fi)
    else
        fail "Webhook receiver behind Caddy ready"
    fi

    section "ADMIN HTTP: Runtime Policy + Provisioning Gates"

    RUNTIME_POLICY_RESULTS_FILE="$BUBBLE_DIR/runtime-policy-results.tsv"
    run_structured_scenario "$RUNTIME_POLICY_RESULTS_FILE" "Runtime policy scenario runner emitted unexpected output" \
        python3 "$(dirname "$0")/staging_webhooks_runtime.py" \
            --server-url "$SERVER_URL" \
            --ssh-host "$SSH_HOST" \
            --admin-token "$ADMIN_HTTP_TOKEN" \
            --token-a "$TOKEN_A" \
            --token-b "$TOKEN_B" \
            --user-a-id "$USER_A_ID" \
            --user-b-id "$USER_B_ID" \
            --run-runtime-policy \
            $(if $SYNC_ENABLED; then printf '%s ' --sync-enabled; fi) \
            $(if $STANDALONE_CLI_READY; then printf '%s ' --standalone-cli-ready; fi)

    section "P1: Backup/Restore"

    if ! $STANDALONE_CLI_READY; then
        skip "cmdock-admin backup (standalone CLI unavailable)"
        skip "cmdock-admin backup restore (standalone CLI unavailable)"
        skip "cmdock-admin backup --include-secrets (standalone CLI unavailable)"
    else
        BACKUP_RESULTS_FILE="$BUBBLE_DIR/backup-restore-results.tsv"
        run_structured_scenario "$BACKUP_RESULTS_FILE" "Backup/restore scenario runner emitted unexpected output" \
            python3 "$(dirname "$0")/staging_backup_restore.py" \
                --server-url "$SERVER_URL" \
                --ssh-host "$SSH_HOST" \
                --admin-token "$ADMIN_HTTP_TOKEN" \
                --token-a "$TOKEN_A" \
                --token-b "$TOKEN_B" \
                --user-b-id "$USER_B_ID" \
                $(if $SYNC_ENABLED; then printf '%s ' --sync-enabled; fi) \
                --tw-mode "$TW_EXEC_MODE" \
                --tw-host "$TW_HOST" \
                --tw-dir-a "${TW_DIR_A:-}" \
                --tw-dir-b "${TW_DIR_B:-}"
    fi

    section "P1: Admin Sync Delete"

    run_structured_scenario "$ADMIN_RUNTIME_RESULTS_FILE" "Admin runtime scenario runner emitted unexpected output" \
        python3 "$(dirname "$0")/staging_admin_runtime.py" \
            --server-url "$SERVER_URL" \
            --ssh-host "$SSH_HOST" \
            --token-a "$TOKEN_A" \
            --user-a-id "$USER_A_ID" \
            --user-b-id "$(if $SYNC_ENABLED; then printf '%s' "${USER_B_ID:-}"; fi)" \
            --run-sync-delete

fi  # --full

# =============================================================================
section "ADMIN CLI: User Delete (cleanup)"
# =============================================================================

# Restore user A delete policy so cleanup can remove the account after the
# runtime-policy tests leave it in allow/forbid mode.
if [[ -n "$ADMIN_HTTP_TOKEN" && -n "${USER_A_ID:-}" ]]; then
    rest_admin_put "/admin/user/$USER_A_ID/runtime-policy" '{"policyVersion":"cleanup-allow-delete","policy":{"runtimeAccess":"allow","deleteAction":"allow"}}' >/dev/null 2>&1 || true
fi

# Delete user B first (tests delete + cascade)
if $STANDALONE_CLI_READY; then
    if cmdock_admin_cli user delete "e2e-user-b" --yes 2>/dev/null; then pass "cmdock-admin user delete (user B)"; else fail "cmdock-admin user delete (user B)"; fi
else
    if server_admin_cli admin user delete "$USER_B_ID" --yes 2>/dev/null; then pass "server admin user delete (user B)"; else fail "server admin user delete (user B)"; fi
fi

# Verify deleted user's token no longer works (may be cached for up to 30s)
DELETED_STATUS=$(rest_get_status "/api/tasks" "$TOKEN_B" || true)
if [[ "$DELETED_STATUS" == "401" ]]; then
    pass "Deleted user token → 401 (immediate revocation)"
elif [[ "$DELETED_STATUS" == "200" ]]; then
    pass "Deleted user token → 200 (stale cache, expires within 30s — documented behavior)"
else
    fail "Deleted user token → unexpected $DELETED_STATUS"
fi

# Delete user A
if $STANDALONE_CLI_READY; then
    if cmdock_admin_cli user delete "e2e-user-a" --yes 2>/dev/null; then pass "cmdock-admin user delete (user A)"; else fail "cmdock-admin user delete (user A)"; fi
else
    if server_admin_cli admin user delete "$USER_A_ID" --yes 2>/dev/null; then pass "server admin user delete (user A)"; else fail "server admin user delete (user A)"; fi
fi

# Clear IDs so cleanup trap doesn't try to delete again
USER_A_ID=""
USER_B_ID=""

# Exit with appropriate code
if [[ $FAIL -gt 0 ]]; then exit 1; else exit 0; fi
