#!/usr/bin/env bash
# Real TaskChampion sync load test.
#
# Uses the actual `task` CLI to create, sync, and verify tasks against
# the server. This tests the REAL user experience (not opaque blobs).
#
# Unlike the Goose load test (which measures raw protocol throughput),
# this test validates that real TaskChampion operations propagate correctly
# under concurrent load.
#
# Usage:
#   ./scripts/real-sync-load.sh [USERS] [TASKS_PER_USER] [URL]
#   ./scripts/real-sync-load.sh 5 10                    # 5 users, 10 tasks each
#   ./scripts/real-sync-load.sh 5 10 http://localhost:8080
#
# From a workstation against internal staging: the local `task` CLI cannot
# validate the internal CA (TaskChampion uses rustls tls-native-roots, which
# ignores SSL_CERT_FILE), so an HTTPS internal URL fails the sync preflight.
# Use `just real-sync-test-tunneled` (SSH-tunnels the server's plain-HTTP port
# and runs over it), or run this on the staging host. (#145)
#
# Prerequisites:
#   - task (Taskwarrior 3.x) installed locally
#   - SSH access to the internal host that runs the root ops-managed compose stack
#   - curl, jq installed locally

set -euo pipefail

NUM_USERS="${1:-3}"
TASKS_PER_USER="${2:-5}"
SERVER_URL="${3:-${STAGING_URL:-${CMDOCK_STAGING_URL:-https://staging.example.com}}}"
SSH_HOST="${STAGING_SSH:-${CMDOCK_STAGING_SSH:-staging.example.com}}"
DOCKER_COMPOSE_FILE="/opt/cmdock/docker-compose.yml"
# Array form so admin_cli can %q-quote each argument for the remote shell.
DOCKER_ADMIN_ARGV=(sudo docker compose -f "$DOCKER_COMPOSE_FILE" exec -T server cmdock-server --config /app/config.toml)

GREEN='\033[0;32m'
RED='\033[0;31m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

PASS=0
FAIL=0
BUBBLE_DIR=$(mktemp -d -t real-sync-XXXXXX)

pass() { PASS=$((PASS + 1)); echo -e "  ${GREEN}✓${NC} $1"; }
fail() { FAIL=$((FAIL + 1)); echo -e "  ${RED}✗${NC} $1"; }
info() { echo -e "${BOLD}==> ${NC}$*"; }

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

admin_cli() {
    # %q-quote each argument so values with spaces/metacharacters survive the
    # remote shell intact rather than being re-split or re-interpreted. (#145)
    local quoted
    printf -v quoted '%q ' "${DOCKER_ADMIN_ARGV[@]}" "$@"
    ssh -o ControlMaster=no -o ConnectTimeout=10 "$SSH_HOST" "$quoted 2>/dev/null"
}

cleanup() {
    info "Cleaning up..."
    for i in $(seq 1 "$NUM_USERS"); do
        if [[ -n "${USER_IDS[$i]:-}" ]]; then
            admin_cli admin user delete "${USER_IDS[$i]}" --yes 2>/dev/null || true
        fi
    done
    rm -rf "$BUBBLE_DIR"
    echo
    echo -e "${BOLD}Real Sync Load Test Results${NC}"
    echo -e "  Users:     ${CYAN}${NUM_USERS}${NC}"
    echo -e "  Tasks/user:${CYAN}${TASKS_PER_USER}${NC}"
    echo -e "  Passed:    ${GREEN}${PASS}${NC}"
    echo -e "  Failed:    ${RED}${FAIL}${NC}"
}
trap cleanup EXIT

echo -e "${BOLD}╔══════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║   Real TaskChampion Sync Load Test                   ║${NC}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════╝${NC}"
echo -e "  Server:     ${CYAN}${SERVER_URL}${NC}"
echo -e "  Users:      ${CYAN}${NUM_USERS}${NC}"
echo -e "  Tasks/user: ${CYAN}${TASKS_PER_USER}${NC}"
echo

# Check dependencies
for cmd in task curl jq ssh bc; do
    command -v "$cmd" &>/dev/null || { echo -e "${RED}Missing: $cmd${NC}"; exit 1; }
done

# Verify server is up
curl -sf --max-time 5 "$SERVER_URL/healthz" > /dev/null || { echo -e "${RED}Server unreachable${NC}"; exit 1; }

# =========================================================================
# Phase 1: Provision users
# =========================================================================
info "Provisioning $NUM_USERS users..."

declare -A USER_IDS TOKENS CLIENT_IDS SECRETS TW_DIRS

for i in $(seq 1 "$NUM_USERS"); do
    OUTPUT=$(admin_cli admin user create --username "realsync-user-$i")
    USER_IDS[$i]=$(echo "$OUTPUT" | awk '/^[[:space:]]*ID:/ { print $2; exit }')
    TOKENS[$i]=$(echo "$OUTPUT" | awk '
        /API token/ { capture=1; next }
        capture {
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", $0)
            if ($0 != "") {
                print
                exit
            }
        }
    ')

    admin_cli admin sync create "${USER_IDS[$i]}" > /dev/null
    DEVICE_OUT=$(admin_cli admin device create "${USER_IDS[$i]}" --name "load-user-$i" --server-url "$SERVER_URL")
    CLIENT_IDS[$i]=$(echo "$DEVICE_OUT" | extract_field "Client ID")
    SECRETS[$i]=$(echo "$DEVICE_OUT" | extract_field "Encryption Secret")

    if [[ -z "${CLIENT_IDS[$i]}" || -z "${SECRETS[$i]}" ]]; then
        echo -e "${RED}Failed to provision device credentials for user ${USER_IDS[$i]}${NC}"
        exit 1
    fi

    # Create TW profile
    TW_DIRS[$i]="$BUBBLE_DIR/user-$i"
    mkdir -p "${TW_DIRS[$i]}/data"
    cat > "${TW_DIRS[$i]}/taskrc" <<EOF
data.location=${TW_DIRS[$i]}/data
sync.server.url=$SERVER_URL
sync.server.client_id=${CLIENT_IDS[$i]}
sync.encryption_secret=${SECRETS[$i]}
confirmation=off
recurrence=off
verbose=nothing
EOF

    # Initial sync to establish chain
    TASKRC="${TW_DIRS[$i]}/taskrc" TASKDATA="${TW_DIRS[$i]}/data" task sync 2>/dev/null || true
done
pass "Provisioned $NUM_USERS users with per-device TW credentials"

# Sync preflight (#145): do a real `task sync` and abort LOUDLY on failure,
# rather than swallowing the error and proceeding to a misleading
# "0 tasks in REST" result. The systemic failure this guards against is the
# local `task` CLI not trusting the server's TLS cert (TaskChampion uses rustls
# tls-native-roots, which ignores SSL_CERT_FILE) — run this script ON the server
# host where the internal CA is trusted, or point SERVER_URL at a plain-HTTP
# SSH tunnel to the server.
if ! preflight_err=$(TASKRC="${TW_DIRS[1]}/taskrc" TASKDATA="${TW_DIRS[1]}/data" task sync 2>&1 >/dev/null); then
    echo -e "${RED}✗ task sync preflight failed — aborting before misleading 0-task verification:${NC}"
    echo "$preflight_err" | sed 's/^/    /'
    exit 1
fi

# =========================================================================
# Phase 2: Create tasks via TW CLI (parallel)
# =========================================================================
info "Creating $TASKS_PER_USER tasks per user via TW CLI..."

START_CREATE=$(date +%s%N)
for i in $(seq 1 "$NUM_USERS"); do
    (
        for j in $(seq 1 "$TASKS_PER_USER"); do
            # Capture stderr + sentinel on failure so a wholesale `task add`
            # failure surfaces as its own diagnostic rather than only as a
            # downstream "0 tasks in REST" mismatch. (#145)
            if ! TASKRC="${TW_DIRS[$i]}/taskrc" TASKDATA="${TW_DIRS[$i]}/data" \
                task add "+realsync User$i task $j project:LOAD priority:M" 2>>"${TW_DIRS[$i]}/add.err"; then
                touch "${TW_DIRS[$i]}/add.failed"
            fi
        done
    ) &
done
wait
END_CREATE=$(date +%s%N)
CREATE_MS=$(( (END_CREATE - START_CREATE) / 1000000 ))

ADD_FAILURES=0
for i in $(seq 1 "$NUM_USERS"); do
    if [[ -f "${TW_DIRS[$i]}/add.failed" ]]; then
        ADD_FAILURES=$((ADD_FAILURES + 1))
        echo -e "${RED}  User $i had task-add failures:${NC}"
        tail -3 "${TW_DIRS[$i]}/add.err" 2>/dev/null | sed 's/^/      /' || true
    fi
done
if [[ $ADD_FAILURES -gt 0 ]]; then
    fail "$ADD_FAILURES/$NUM_USERS users had task-add failures (see above)"
else
    pass "Created $(( NUM_USERS * TASKS_PER_USER )) tasks in ${CREATE_MS}ms"
fi

# =========================================================================
# Phase 3: Sync all users (parallel)
# =========================================================================
info "Syncing all users to server (parallel)..."

START_SYNC=$(date +%s%N)
SYNC_FAILURES=0
for i in $(seq 1 "$NUM_USERS"); do
    (
        # Capture stderr and record a sentinel on failure so the parent can
        # surface it after `wait` — parallel subshell exit codes are otherwise
        # lost, which is how sync failures used to masquerade as "0 tasks". (#145)
        if ! TASKRC="${TW_DIRS[$i]}/taskrc" TASKDATA="${TW_DIRS[$i]}/data" \
            task sync >/dev/null 2>"${TW_DIRS[$i]}/sync.err"; then
            touch "${TW_DIRS[$i]}/sync.failed"
        fi
    ) &
done
wait
END_SYNC=$(date +%s%N)
SYNC_MS=$(( (END_SYNC - START_SYNC) / 1000000 ))

for i in $(seq 1 "$NUM_USERS"); do
    if [[ -f "${TW_DIRS[$i]}/sync.failed" ]]; then
        SYNC_FAILURES=$((SYNC_FAILURES + 1))
        echo -e "${RED}  User $i sync failed:${NC}"
        sed 's/^/      /' "${TW_DIRS[$i]}/sync.err" 2>/dev/null || true
    fi
done
if [[ $SYNC_FAILURES -gt 0 ]]; then
    fail "$SYNC_FAILURES/$NUM_USERS users failed to sync — REST verification below would be meaningless"
else
    pass "All $NUM_USERS users synced in ${SYNC_MS}ms"
fi

# =========================================================================
# Phase 4: Verify tasks appear in REST API
# =========================================================================
info "Verifying tasks in REST API..."

sleep 2  # Allow gateway source-apply path to settle

for i in $(seq 1 "$NUM_USERS"); do
    # Verify the gateway-applied source state through REST
    REST_TASKS=$(curl -s --max-time 10 -H "Authorization: Bearer ${TOKENS[$i]}" "$SERVER_URL/api/tasks" || true)
    REST_COUNT=$(echo "$REST_TASKS" | jq 'length' 2>/dev/null || echo "0")

    if [[ "$REST_COUNT" -ge "$TASKS_PER_USER" ]]; then
        pass "User $i: $REST_COUNT tasks visible in REST (expected $TASKS_PER_USER)"
    else
        fail "User $i: $REST_COUNT tasks in REST (expected $TASKS_PER_USER)"
    fi
done

# =========================================================================
# Phase 5: Create tasks via REST, verify in TW
# =========================================================================
info "Creating tasks via REST API..."

# -sfS: fail (non-2xx -> non-zero) and show errors; capture the body so a 401/
# 500/network failure aborts loudly here instead of surfacing later as a
# confusing "REST task NOT found in TW". (#145)
for i in $(seq 1 "$NUM_USERS"); do
    if ! create_body=$(curl -sfS --max-time 10 -X POST \
        -H "Authorization: Bearer ${TOKENS[$i]}" \
        -H "Content-Type: application/json" \
        -d "{\"raw\": \"+realsync REST-created for user$i\"}" \
        "$SERVER_URL/api/tasks" 2>&1); then
        fail "User $i: REST task creation failed: $create_body"
        FATAL_REST_CREATE=1
    fi
done
if [[ "${FATAL_REST_CREATE:-0}" -eq 1 ]]; then
    echo -e "${RED}REST task creation failed for one or more users — aborting Phase 5${NC}"
    exit 1
fi

# Mark REST writes observed before TW clients pull the gateway projection
for i in $(seq 1 "$NUM_USERS"); do
    curl -s --max-time 10 -H "Authorization: Bearer ${TOKENS[$i]}" "$SERVER_URL/api/tasks" > /dev/null || true
done

sleep 2

info "Syncing TW to pull REST-created tasks..."
for i in $(seq 1 "$NUM_USERS"); do
    (
        # Same sentinel pattern as Phase 3 — surface pull-sync failures rather
        # than inferring them from a missing task downstream. (#145)
        if ! TASKRC="${TW_DIRS[$i]}/taskrc" TASKDATA="${TW_DIRS[$i]}/data" \
            task sync >/dev/null 2>"${TW_DIRS[$i]}/sync2.err"; then
            touch "${TW_DIRS[$i]}/sync2.failed"
        fi
    ) &
done
wait
SYNC2_FAILURES=0
for i in $(seq 1 "$NUM_USERS"); do
    if [[ -f "${TW_DIRS[$i]}/sync2.failed" ]]; then
        SYNC2_FAILURES=$((SYNC2_FAILURES + 1))
        echo -e "${RED}  User $i pull-sync failed:${NC}"
        sed 's/^/      /' "${TW_DIRS[$i]}/sync2.err" 2>/dev/null || true
    fi
done
# Record as a failure so the run exits non-zero — surfacing alone isn't enough,
# the downstream TW verification could otherwise still pass and exit 0. Use an
# `if` (not `&&`) so the false branch doesn't trip `set -e`. (#145)
if [[ $SYNC2_FAILURES -gt 0 ]]; then
    fail "$SYNC2_FAILURES/$NUM_USERS users failed the REST->TW pull sync"
fi

for i in $(seq 1 "$NUM_USERS"); do
    TW_EXPORT=$(TASKRC="${TW_DIRS[$i]}/taskrc" TASKDATA="${TW_DIRS[$i]}/data" task export 2>/dev/null || echo "[]")
    if echo "$TW_EXPORT" | jq -r '.[].description' 2>/dev/null | grep -q "REST-created for user$i"; then
        pass "User $i: REST task appears in TW after sync"
    else
        fail "User $i: REST task NOT found in TW"
    fi
done

# =========================================================================
# Phase 6: Cross-user isolation
# =========================================================================
info "Verifying cross-user isolation..."

# User 1's REST API should NOT show User 2's tasks
if [[ $NUM_USERS -ge 2 ]]; then
    U1_TASKS=$(curl -s --max-time 10 -H "Authorization: Bearer ${TOKENS[1]}" "$SERVER_URL/api/tasks" | jq -r '.[].description' || true)
    if echo "$U1_TASKS" | grep -q "User2"; then
        fail "Cross-user leak: User 1 sees User 2's tasks"
    else
        pass "Cross-user isolation: User 1 cannot see User 2's tasks"
    fi
fi

# =========================================================================
# Summary
# =========================================================================
TOTAL_TASKS=$(( NUM_USERS * (TASKS_PER_USER + 1) ))  # +1 for REST-created
echo
echo -e "${BOLD}Performance:${NC}"
echo -e "  Task creation: ${CYAN}${CREATE_MS}ms${NC} for $(( NUM_USERS * TASKS_PER_USER )) tasks"
echo -e "  Parallel sync: ${CYAN}${SYNC_MS}ms${NC} for $NUM_USERS users"
echo -e "  Throughput:    ${CYAN}$(echo "scale=0; $TOTAL_TASKS * 1000 / ($CREATE_MS + $SYNC_MS + 1)" | bc) tasks/s${NC} (create + sync)"

# Exit with failure if any tests failed
if [[ $FAIL -gt 0 ]]; then exit 1; fi
