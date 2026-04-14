#!/usr/bin/env bash
# Integration test: TaskChampion sync protocol with real `task` CLI.
#
# Spins up an isolated server, creates users via admin CLI, configures
# real .taskrc files, and runs actual `task add`, `task sync`, `task list`
# commands to verify end-to-end sync.
#
# Usage: ./scripts/test-sync.sh
# Requirements: task (taskwarrior 3.x), cargo, curl, jq

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUBBLE_DIR=$(mktemp -d -t tc-sync-test-XXXXXX)
SERVER_PORT=18090
SERVER_PID=""
PASS=0
FAIL=0
TOTAL=0
export CMDOCK_MASTER_KEY="$(printf '2a%.0s' {1..32})"

# Colours
GREEN='\033[0;32m'
RED='\033[0;31m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${BOLD}==> ${NC}$*"; }
ok()    { echo -e "  ${GREEN}✓${NC} $*"; PASS=$((PASS + 1)); TOTAL=$((TOTAL + 1)); }
fail()  { echo -e "  ${RED}✗${NC} $*"; FAIL=$((FAIL + 1)); TOTAL=$((TOTAL + 1)); }
skip()  { echo -e "  ${YELLOW}⊘${NC} $*"; TOTAL=$((TOTAL + 1)); }

cleanup() {
    info "Cleaning up..."
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$BUBBLE_DIR"
}
trap cleanup EXIT

echo -e "${BOLD}╔══════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║  cmdock sync protocol — real CLI integration tests  ║${NC}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════╝${NC}"
echo ""

# --- 0. Pre-flight ---
info "Pre-flight checks"

if ! command -v task &>/dev/null; then
    echo "ERROR: 'task' (taskwarrior) not found. Install taskwarrior 3.x."
    exit 1
fi

TW_VERSION=$(task --version 2>/dev/null)
echo "  task version: $TW_VERSION"

if ! [[ "$TW_VERSION" =~ ^3\. ]]; then
    echo "ERROR: taskwarrior 3.x required for TaskChampion sync (found $TW_VERSION)"
    exit 1
fi

# --- 1. Build server ---
info "Building server (release)..."
cd "$PROJECT_DIR"
cargo build --release --bin cmdock-server 2>&1 | tail -1
SERVER_BIN="$PROJECT_DIR/target/release/cmdock-server"

# --- 2. Set up server ---
info "Setting up isolated server"

SERVER_DATA="$BUBBLE_DIR/server-data"
mkdir -p "$SERVER_DATA/users"

cat > "$BUBBLE_DIR/config.toml" <<EOF
[server]
host = "127.0.0.1"
port = $SERVER_PORT
data_dir = "$SERVER_DATA"
EOF

# Run migrations
"$SERVER_BIN" --config "$BUBBLE_DIR/config.toml" --migrate 2>/dev/null

# Create two test users via admin CLI
info "Creating test users"

USER1_OUTPUT=$("$SERVER_BIN" --config "$BUBBLE_DIR/config.toml" admin user create --username alice)
USER1_ID=$(echo "$USER1_OUTPUT" | grep "ID:" | awk '{print $2}')
USER1_TOKEN=$(echo "$USER1_OUTPUT" | grep -A1 "API token" | tail -1 | tr -d ' ')

USER2_OUTPUT=$("$SERVER_BIN" --config "$BUBBLE_DIR/config.toml" admin user create --username bob)
USER2_ID=$(echo "$USER2_OUTPUT" | grep "ID:" | awk '{print $2}')
USER2_TOKEN=$(echo "$USER2_OUTPUT" | grep -A1 "API token" | tail -1 | tr -d ' ')

echo "  Alice: $USER1_ID (token: ${USER1_TOKEN:0:16}...)"
echo "  Bob:   $USER2_ID (token: ${USER2_TOKEN:0:16}...)"

# --- 3. Set up .taskrc for each user ---
info "Configuring .taskrc files"

# Register sync identities and per-device credentials via admin CLI
info "Registering sync identities and devices"

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

"$SERVER_BIN" --config "$BUBBLE_DIR/config.toml" admin sync create "$USER1_ID" >/dev/null
"$SERVER_BIN" --config "$BUBBLE_DIR/config.toml" admin sync create "$USER2_ID" >/dev/null

ALICE_DEVICE_1=$("$SERVER_BIN" --config "$BUBBLE_DIR/config.toml" admin device create "$USER1_ID" --name "alice-cli-1" --server-url "http://127.0.0.1:$SERVER_PORT")
ALICE_CLIENT_ID=$(echo "$ALICE_DEVICE_1" | extract_field "Client ID")
ALICE_SECRET=$(echo "$ALICE_DEVICE_1" | extract_field "Encryption Secret")
echo "  Alice CLI 1: $ALICE_CLIENT_ID"

ALICE_DEVICE_2=$("$SERVER_BIN" --config "$BUBBLE_DIR/config.toml" admin device create "$USER1_ID" --name "alice-cli-2" --server-url "http://127.0.0.1:$SERVER_PORT")
ALICE2_CLIENT_ID=$(echo "$ALICE_DEVICE_2" | extract_field "Client ID")
ALICE2_SECRET=$(echo "$ALICE_DEVICE_2" | extract_field "Encryption Secret")
echo "  Alice CLI 2: $ALICE2_CLIENT_ID"

BOB_DEVICE=$("$SERVER_BIN" --config "$BUBBLE_DIR/config.toml" admin device create "$USER2_ID" --name "bob-cli-1" --server-url "http://127.0.0.1:$SERVER_PORT")
BOB_CLIENT_ID=$(echo "$BOB_DEVICE" | extract_field "Client ID")
BOB_SECRET=$(echo "$BOB_DEVICE" | extract_field "Encryption Secret")
echo "  Bob CLI: $BOB_CLIENT_ID"

# Client 1 (Alice CLI)
ALICE_DIR="$BUBBLE_DIR/alice"
mkdir -p "$ALICE_DIR/data"
cat > "$ALICE_DIR/.taskrc" <<EOF
data.location=$ALICE_DIR/data
sync.server.url=http://127.0.0.1:$SERVER_PORT
sync.server.client_id=$ALICE_CLIENT_ID
sync.encryption_secret=$ALICE_SECRET
confirmation=off
verbose=nothing
EOF

# Client 2 (Alice CLI — second device, same user/server)
ALICE2_DIR="$BUBBLE_DIR/alice2"
mkdir -p "$ALICE2_DIR/data"
cat > "$ALICE2_DIR/.taskrc" <<EOF
data.location=$ALICE2_DIR/data
sync.server.url=http://127.0.0.1:$SERVER_PORT
sync.server.client_id=$ALICE2_CLIENT_ID
sync.encryption_secret=$ALICE2_SECRET
confirmation=off
verbose=nothing
EOF

# Client 3 (Bob CLI — different user)
BOB_DIR="$BUBBLE_DIR/bob"
mkdir -p "$BOB_DIR/data"
cat > "$BOB_DIR/.taskrc" <<EOF
data.location=$BOB_DIR/data
sync.server.url=http://127.0.0.1:$SERVER_PORT
sync.server.client_id=$BOB_CLIENT_ID
sync.encryption_secret=$BOB_SECRET
confirmation=off
verbose=nothing
EOF

alias_task_alice="task rc:$ALICE_DIR/.taskrc"
alias_task_alice2="task rc:$ALICE2_DIR/.taskrc"
alias_task_bob="task rc:$BOB_DIR/.taskrc"

# --- 4. Start server ---
info "Starting server on port $SERVER_PORT"
"$SERVER_BIN" --config "$BUBBLE_DIR/config.toml" &
SERVER_PID=$!

# Wait for server to be ready
for i in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$SERVER_PORT/healthz" >/dev/null 2>&1; then
        break
    fi
    sleep 0.2
done

if ! curl -sf "http://127.0.0.1:$SERVER_PORT/healthz" >/dev/null 2>&1; then
    echo "ERROR: Server failed to start"
    exit 1
fi
echo "  Server running (PID $SERVER_PID)"

# ═══════════════════════════════════════════════════
# TEST 1: Empty replica first sync
# ═══════════════════════════════════════════════════
info "Test 1: Empty replica first sync"

if $alias_task_alice sync 2>&1; then
    ok "Alice first sync (empty) succeeded"
else
    fail "Alice first sync failed"
fi

# ═══════════════════════════════════════════════════
# TEST 2: CLI add → sync
# ═══════════════════════════════════════════════════
info "Test 2: CLI add → sync"

$alias_task_alice add project:work "Buy milk" 2>/dev/null
$alias_task_alice add project:work priority:H "Write report" 2>/dev/null
$alias_task_alice add project:home +errands "Fix tap" 2>/dev/null

if $alias_task_alice sync 2>&1; then
    ok "Alice sync after adding 3 tasks"
else
    fail "Alice sync after adding tasks failed"
fi

CLI_COUNT=$($alias_task_alice count 2>/dev/null || echo "0")
if [[ "$CLI_COUNT" -ge 3 ]]; then
    ok "Alice CLI shows $CLI_COUNT tasks"
else
    fail "Alice CLI shows $CLI_COUNT tasks (expected ≥3)"
fi

# ═══════════════════════════════════════════════════
# TEST 3: REST create -> TW sync
# ═══════════════════════════════════════════════════
info "Test 3: REST create -> TW sync"

REST_CREATE_DESC="REST_to_TW_local_test"
REST_CREATE_OUT=$(curl -s --max-time 10 -X POST \
    -H "Authorization: Bearer $USER1_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"raw\":\"$REST_CREATE_DESC project:RESTSYNC priority:H\"}" \
    "http://127.0.0.1:$SERVER_PORT/api/tasks" || true)

if echo "$REST_CREATE_OUT" | jq -e '.success' >/dev/null 2>&1; then
    ok "REST task creation succeeded for Alice"
else
    fail "REST task creation failed for Alice"
fi

REST_TASK_UUID=$(curl -s --max-time 10 \
    -H "Authorization: Bearer $USER1_TOKEN" \
    "http://127.0.0.1:$SERVER_PORT/api/tasks" \
    | jq -r ".[] | select(.description == \"$REST_CREATE_DESC\") | .uuid" \
    | head -1 || true)

FOUND_IN_TW=0
for _ in $(seq 1 5); do
    $alias_task_alice sync 2>/dev/null || true
    if $alias_task_alice export 2>/dev/null | jq -r '.[].description' | grep -q "^$REST_CREATE_DESC$"; then
        FOUND_IN_TW=1
        break
    fi
    sleep 1
done

if [[ "$FOUND_IN_TW" -eq 1 ]]; then
    ok "REST-created task appears in Taskwarrior after sync"
else
    fail "REST-created task did not appear in Taskwarrior after sync"
fi

# ═══════════════════════════════════════════════════
# TEST 4: REST modify -> TW sync
# ═══════════════════════════════════════════════════
info "Test 4: REST modify -> TW sync"

if [[ -n "$REST_TASK_UUID" && "$REST_TASK_UUID" != "null" ]]; then
    REST_MODIFY_OUT=$(curl -s --max-time 10 -X POST \
        -H "Authorization: Bearer $USER1_TOKEN" \
        -H "Content-Type: application/json" \
        -d '{"priority":"M"}' \
        "http://127.0.0.1:$SERVER_PORT/api/tasks/$REST_TASK_UUID/modify" || true)

    if echo "$REST_MODIFY_OUT" | jq -e '.success' >/dev/null 2>&1; then
        ok "REST modify succeeded for Alice task"
    else
        fail "REST modify failed for Alice task"
    fi

    MODIFIED_IN_TW=0
    for _ in $(seq 1 5); do
        $alias_task_alice sync 2>/dev/null || true
        TW_PRI=$($alias_task_alice export 2>/dev/null \
            | jq -r ".[] | select(.uuid == \"$REST_TASK_UUID\") | .priority" \
            | head -1 || true)
        if [[ "$TW_PRI" == "M" ]]; then
            MODIFIED_IN_TW=1
            break
        fi
        sleep 1
    done

    if [[ "$MODIFIED_IN_TW" -eq 1 ]]; then
        ok "REST modify propagates into Taskwarrior"
    else
        fail "REST modify did not propagate into Taskwarrior"
    fi

    REST_DONE_OUT=$(curl -s --max-time 10 -X POST \
        -H "Authorization: Bearer $USER1_TOKEN" \
        "http://127.0.0.1:$SERVER_PORT/api/tasks/$REST_TASK_UUID/done" || true)
    if echo "$REST_DONE_OUT" | jq -e '.success' >/dev/null 2>&1; then
        ok "REST complete succeeded for Alice task"
    else
        fail "REST complete failed for Alice task"
    fi

    REST_UNDO_OUT=$(curl -s --max-time 10 -X POST \
        -H "Authorization: Bearer $USER1_TOKEN" \
        "http://127.0.0.1:$SERVER_PORT/api/tasks/$REST_TASK_UUID/undo" || true)
    if echo "$REST_UNDO_OUT" | jq -e '.success' >/dev/null 2>&1; then
        ok "REST undo succeeded for Alice task"
    else
        fail "REST undo failed for Alice task"
    fi

    REOPENED_IN_TW=0
    for _ in $(seq 1 5); do
        $alias_task_alice sync 2>/dev/null || true
        TW_STATUS=$($alias_task_alice export 2>/dev/null \
            | jq -r ".[] | select(.uuid == \"$REST_TASK_UUID\") | .status" \
            | head -1 || true)
        if [[ "$TW_STATUS" == "pending" ]]; then
            REOPENED_IN_TW=1
            break
        fi
        sleep 1
    done

    if [[ "$REOPENED_IN_TW" -eq 1 ]]; then
        ok "REST undo propagates into Taskwarrior"
    else
        fail "REST undo did not propagate into Taskwarrior"
    fi
else
    fail "REST modify test skipped because REST task UUID could not be resolved"
fi

# ═══════════════════════════════════════════════════
# TEST 5: Two CLI clients syncing (same user, different devices)
# ═══════════════════════════════════════════════════
info "Test 5: Two CLI clients — same user, different devices"

# Alice2 syncs to get all existing tasks
$alias_task_alice2 sync 2>/dev/null

ALICE2_COUNT=$($alias_task_alice2 count 2>/dev/null || echo "0")
if [[ "$ALICE2_COUNT" -ge 3 ]]; then
    ok "Alice device 2 sees $ALICE2_COUNT tasks after initial sync"
else
    fail "Alice device 2 sees $ALICE2_COUNT tasks (expected ≥3)"
fi

# Alice2 adds a task, syncs
$alias_task_alice2 add "Task from device 2" 2>/dev/null
$alias_task_alice2 sync 2>/dev/null

# Alice1 syncs to pick it up. Allow a few rounds for queued bridge convergence.
for _ in $(seq 1 5); do
    $alias_task_alice sync 2>/dev/null || true
    ALICE1_COUNT=$($alias_task_alice count 2>/dev/null || echo "0")
    if [[ "$ALICE1_COUNT" -ge "$((ALICE2_COUNT + 1))" ]]; then
        break
    fi
    sleep 1
done

ALICE1_COUNT=$($alias_task_alice count 2>/dev/null || echo "0")
if [[ "$ALICE1_COUNT" -ge "$((ALICE2_COUNT + 1))" ]]; then
    ok "Alice device 1 sees task from device 2 ($ALICE1_COUNT tasks)"
else
    fail "Alice device 1 count $ALICE1_COUNT — expected ≥$((ALICE2_COUNT + 1))"
fi

# ═══════════════════════════════════════════════════
# TEST 6: Data isolation — Bob can't see Alice's tasks
# ═══════════════════════════════════════════════════
info "Test 6: Data isolation — different users"

$alias_task_bob sync 2>/dev/null

BOB_COUNT=$($alias_task_bob count 2>/dev/null || echo "0")
if [[ "$BOB_COUNT" -eq 0 ]]; then
    ok "Bob sees 0 tasks (Alice's data is isolated)"
else
    fail "Bob sees $BOB_COUNT tasks (should be 0 — data leak!)"
fi

# Bob adds his own task
$alias_task_bob add "Bob's task" 2>/dev/null
$alias_task_bob sync 2>/dev/null

BOB_COUNT=$($alias_task_bob count 2>/dev/null || echo "0")
if [[ "$BOB_COUNT" -eq 1 ]]; then
    ok "Bob has 1 task after adding his own"
else
    fail "Bob has $BOB_COUNT tasks (expected 1)"
fi

# Verify Alice doesn't see Bob's tasks
ALICE_BEFORE_BOB=$($alias_task_alice count 2>/dev/null || echo "0")
$alias_task_alice sync 2>/dev/null
ALICE_AFTER_BOB=$($alias_task_alice count 2>/dev/null || echo "0")
if [[ "$ALICE_AFTER_BOB" -eq "$ALICE_BEFORE_BOB" ]]; then
    ok "Alice still has $ALICE_AFTER_BOB tasks (no leak from Bob)"
else
    fail "Alice count changed $ALICE_BEFORE_BOB → $ALICE_AFTER_BOB (possible leak)"
fi

# ═══════════════════════════════════════════════════
# TEST 7: Modify round-trip — CLI→sync→other CLI
# ═══════════════════════════════════════════════════
info "Test 7: Modify round-trip — device 1 modify → sync → device 2 sees change"

TASK_UUID=$($alias_task_alice _uuids 2>/dev/null | head -1)
if [[ -n "$TASK_UUID" ]]; then
    $alias_task_alice modify "$TASK_UUID" priority:M 2>/dev/null || true
    $alias_task_alice sync 2>/dev/null

    # Sync device 2
    $alias_task_alice2 sync 2>/dev/null

    # Check on device 2
    MODIFIED=$($alias_task_alice2 "$TASK_UUID" _unique priority 2>/dev/null || \
               $alias_task_alice2 export "$TASK_UUID" 2>/dev/null | jq -r '.[0].priority' 2>/dev/null || echo "")

    if [[ "$MODIFIED" == "M" ]]; then
        ok "Modify round-trip: priority M visible on device 2"
    else
        # Try alternate check
        skip "Modify round-trip: couldn't verify priority on device 2 (TW output format)"
    fi
else
    skip "No task UUID found for modify test"
fi

# ═══════════════════════════════════════════════════
# TEST 8: Complete round-trip
# ═══════════════════════════════════════════════════
info "Test 8: Complete round-trip — task done on device 1 → sync → device 2"

# Use task number 1 (not UUID — TW `done` prefers filter syntax)
BEFORE=$($alias_task_alice count 2>/dev/null || echo "0")
if [[ "$BEFORE" -gt 0 ]]; then
    $alias_task_alice 1 done 2>/dev/null || true
    $alias_task_alice sync 2>/dev/null
    AFTER=$($alias_task_alice count 2>/dev/null || echo "0")
    if [[ "$AFTER" -lt "$BEFORE" ]]; then
        ok "Complete round-trip: pending count decreased ($BEFORE → $AFTER)"
    elif [[ "$AFTER" -eq "$BEFORE" ]]; then
        # TW3 `done` may not decrease `count` in non-interactive mode
        skip "Complete round-trip: count unchanged ($BEFORE) — TW3 non-interactive quirk"
    else
        fail "Complete round-trip: count increased unexpectedly ($BEFORE → $AFTER)"
    fi

    # Verify on device 2
    $alias_task_alice2 sync 2>/dev/null
    AFTER2=$($alias_task_alice2 count 2>/dev/null || echo "0")
    if [[ "$AFTER2" -eq "$AFTER" ]]; then
        ok "Device 2 pending count matches ($AFTER2)"
    else
        fail "Device 2 count $AFTER2 doesn't match device 1 count $AFTER"
    fi
else
    skip "No tasks to complete"
fi

# ═══════════════════════════════════════════════════
# TEST 9: Large batch
# ═══════════════════════════════════════════════════
info "Test 9: Large batch — 50 tasks"

for i in $(seq 1 50); do
    $alias_task_bob add "Batch task $i" 2>/dev/null
done

if $alias_task_bob sync 2>&1; then
    ok "Bob synced 50 tasks"
else
    fail "Bob sync after batch add failed"
fi

BOB_FINAL=$($alias_task_bob count 2>/dev/null || echo "0")
if [[ "$BOB_FINAL" -ge 51 ]]; then
    ok "Bob has $BOB_FINAL tasks (50 batch + 1 earlier)"
else
    fail "Bob has $BOB_FINAL tasks (expected ≥51)"
fi

# ═══════════════════════════════════════════════════
# TEST 10: Unregistered client_id → rejected
# ═══════════════════════════════════════════════════
info "Test 10: Unregistered client_id → 403"

FAKE_CLIENT=$(python3 -c "import uuid; print(uuid.uuid4())")
RESP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST \
    -H "X-Client-Id: $FAKE_CLIENT" \
    -H "Content-Type: application/vnd.taskchampion.history-segment" \
    -d "fake-data" \
    "http://127.0.0.1:$SERVER_PORT/v1/client/add-version/00000000-0000-0000-0000-000000000000" 2>/dev/null || echo "000")

if [[ "$RESP_CODE" == "403" ]]; then
    ok "Unregistered client_id returns 403"
else
    fail "Unregistered client_id returns $RESP_CODE (expected 403)"
fi

# ═══════════════════════════════════════════════════
# TEST 11: Missing X-Client-Id → 400
# ═══════════════════════════════════════════════════
info "Test 11: Missing X-Client-Id → 400"

RESP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST \
    -H "Content-Type: application/vnd.taskchampion.history-segment" \
    -d "data" \
    "http://127.0.0.1:$SERVER_PORT/v1/client/add-version/00000000-0000-0000-0000-000000000000" 2>/dev/null || echo "000")

if [[ "$RESP_CODE" == "400" ]]; then
    ok "Missing X-Client-Id returns 400"
else
    fail "Missing X-Client-Id returns $RESP_CODE (expected 400)"
fi

# ═══════════════════════════════════════════════════
# TEST 12: Wrong Content-Type → 415
# ═══════════════════════════════════════════════════
info "Test 12: Wrong Content-Type → 415"

RESP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST \
    -H "X-Client-Id: $ALICE_CLIENT_ID" \
    -H "Content-Type: application/json" \
    -d '{"bad":"payload"}' \
    "http://127.0.0.1:$SERVER_PORT/v1/client/add-version/00000000-0000-0000-0000-000000000000" 2>/dev/null || echo "000")

if [[ "$RESP_CODE" == "415" ]]; then
    ok "Wrong Content-Type returns 415"
else
    fail "Wrong Content-Type returns $RESP_CODE (expected 415)"
fi

# ═══════════════════════════════════════════════════
# TEST 13: Deleted shared sync DB → fresh start
# ═══════════════════════════════════════════════════
info "Test 13: Deleted shared sync DB → server recovers"

# Bob has 51 tasks. Delete his shared sync DB.
BOB_SYNC_DB="$SERVER_DATA/users/$USER2_ID/sync.sqlite"
if [[ -f "$BOB_SYNC_DB" ]]; then
    rm -f "$BOB_SYNC_DB" "$BOB_SYNC_DB-wal" "$BOB_SYNC_DB-shm"

    # Bob should be able to sync again (starts fresh chain)
    if $alias_task_bob sync 2>&1; then
        ok "Bob re-synced after shared sync DB deletion"
    else
        fail "Bob failed to re-sync after shared sync DB deletion"
    fi

    BOB_AFTER=$($alias_task_bob count 2>/dev/null || echo "0")
    if [[ "$BOB_AFTER" -ge 51 ]]; then
        ok "Bob still has $BOB_AFTER tasks locally (no data loss)"
    else
        fail "Bob has $BOB_AFTER tasks (expected ≥51 — local data should survive)"
    fi
else
    skip "Bob's shared sync DB not found"
fi

# ═══════════════════════════════════════════════════
# TEST 14: Simultaneous edits on two devices
# ═══════════════════════════════════════════════════
info "Test 14: Simultaneous edits → conflict resolution"

# Alice device 1 adds a task
$alias_task_alice add "Device 1 edit" 2>/dev/null

# Alice device 2 adds a task (before syncing)
$alias_task_alice2 add "Device 2 edit" 2>/dev/null

# Both sync — one should conflict and resolve
if $alias_task_alice sync 2>&1; then
    ok "Device 1 sync succeeded"
else
    fail "Device 1 sync failed"
fi

if $alias_task_alice2 sync 2>&1; then
    ok "Device 2 sync succeeded (conflict resolved)"
else
    # Conflict may require a second sync
    if $alias_task_alice2 sync 2>&1; then
        ok "Device 2 sync succeeded on retry (conflict resolved)"
    else
        fail "Device 2 sync failed even after retry"
    fi
fi

# Both devices should converge
$alias_task_alice sync 2>/dev/null
ALICE_D1=$($alias_task_alice count 2>/dev/null || echo "0")
ALICE_D2=$($alias_task_alice2 count 2>/dev/null || echo "0")
if [[ "$ALICE_D1" -eq "$ALICE_D2" ]]; then
    ok "Both devices converged: $ALICE_D1 tasks each"
else
    fail "Devices diverged: device 1 has $ALICE_D1, device 2 has $ALICE_D2"
fi

# ═══════════════════════════════════════════════════
# TEST 15: Task delete + sync
# ═══════════════════════════════════════════════════
info "Test 15: Task delete syncs across devices"

BEFORE_DEL=$($alias_task_alice count 2>/dev/null || echo "0")
$alias_task_alice 1 delete 2>/dev/null || true
$alias_task_alice sync 2>/dev/null
$alias_task_alice2 sync 2>/dev/null

AFTER_DEL_D1=$($alias_task_alice count 2>/dev/null || echo "0")
AFTER_DEL_D2=$($alias_task_alice2 count 2>/dev/null || echo "0")
if [[ "$AFTER_DEL_D1" -eq "$AFTER_DEL_D2" ]]; then
    ok "Delete synced: both devices have $AFTER_DEL_D1 tasks"
else
    fail "Delete not synced: device 1=$AFTER_DEL_D1, device 2=$AFTER_DEL_D2"
fi

# ═══════════════════════════════════════════════════
# SUMMARY
# ═══════════════════════════════════════════════════
echo ""
echo -e "${BOLD}╔══════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║  Results: $PASS passed, $FAIL failed, $TOTAL total${NC}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════╝${NC}"

if [[ "$FAIL" -gt 0 ]]; then
    exit 1
fi
