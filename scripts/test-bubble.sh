#!/usr/bin/env bash
# Test bubble: isolated Taskwarrior + server environment for side-by-side filter testing.
# Everything runs in temp directories — no impact on real TW or server data.
#
# Usage: ./scripts/test-bubble.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUBBLE_DIR=$(mktemp -d -t tc-test-bubble-XXXXXX)
SERVER_PORT=18080
SERVER_PID=""

# Colours
GREEN='\033[0;32m'
RED='\033[0;31m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${BOLD}==> ${NC}$*"; }
ok()    { echo -e "  ${GREEN}✓${NC} $*"; }
fail()  { echo -e "  ${RED}✗${NC} $*"; }
warn()  { echo -e "  ${YELLOW}!${NC} $*"; }

cleanup() {
    info "Cleaning up..."
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
        ok "Server stopped"
    fi
    rm -rf "$BUBBLE_DIR"
    ok "Temp directory removed: $BUBBLE_DIR"
}
trap cleanup EXIT

echo -e "${BOLD}╔══════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║   TaskChampion Server — Test Bubble          ║${NC}"
echo -e "${BOLD}╚══════════════════════════════════════════════╝${NC}"
echo

# --- 1. Set up isolated Taskwarrior ---
info "Setting up isolated Taskwarrior environment"

export TASKDATA="$BUBBLE_DIR/tw-data"
export TASKRC="$BUBBLE_DIR/taskrc"
mkdir -p "$TASKDATA"

cat > "$TASKRC" <<EOF
data.location=$TASKDATA
confirmation=no
verbose=nothing
json.array=on
EOF

ok "TASKDATA=$TASKDATA"
ok "TASKRC=$TASKRC"

# --- 2. Build the server ---
info "Building server (release)..."
cd "$PROJECT_DIR"
cargo build --release --bin cmdock-server 2>&1 | tail -1
ok "Server built"

# --- 3. Set up server config ---
info "Setting up server config"

SERVER_DATA="$BUBBLE_DIR/server-data"
mkdir -p "$SERVER_DATA/users"

cat > "$BUBBLE_DIR/config.toml" <<EOF
[server]
host = "127.0.0.1"
port = $SERVER_PORT
data_dir = "$SERVER_DATA"
EOF

ok "Server config at $BUBBLE_DIR/config.toml"

# --- 4. Run migrations and create test user ---
info "Running migrations and creating test user"

# Start server briefly to run migrations
"$PROJECT_DIR/target/release/cmdock-server" --config "$BUBBLE_DIR/config.toml" --migrate 2>&1 | tail -1

# Create test user and token directly in SQLite
TEST_USER_ID="test-user-001"
TEST_TOKEN="test-token-bubble-$(date +%s)"
# Hash the token (SHA-256)
TOKEN_HASH=$(echo -n "$TEST_TOKEN" | sha256sum | cut -d' ' -f1)

python3 -c "
import sqlite3
conn = sqlite3.connect('$SERVER_DATA/config.sqlite')
conn.execute(\"INSERT INTO users (id, username, password_hash) VALUES ('$TEST_USER_ID', 'testuser', 'not-a-real-hash')\")
conn.execute(\"INSERT INTO api_tokens (token_hash, user_id, label) VALUES ('$TOKEN_HASH', '$TEST_USER_ID', 'test-bubble')\")
conn.commit()
conn.close()
"

ok "Test user created: testuser"
ok "Token: $TEST_TOKEN"

# --- 5. Start the server ---
info "Starting server on port $SERVER_PORT"

"$PROJECT_DIR/target/release/cmdock-server" --config "$BUBBLE_DIR/config.toml" &
SERVER_PID=$!
sleep 1

# Verify it's running
if curl -sf "http://127.0.0.1:$SERVER_PORT/healthz" > /dev/null 2>&1; then
    ok "Server running (PID $SERVER_PID)"
else
    fail "Server failed to start"
    exit 1
fi

# --- 6. Seed identical tasks into both TW and server ---
info "Seeding test tasks"

TASKS=(
    "project:PERSONAL.Home +shopping +coles priority:H Buy milk"
    "project:PERSONAL.Home +shopping +woolworths Buy bread"
    "project:PERSONAL.Home +shopping +bunnings Buy screws"
    "project:PERSONAL.Health +gym priority:M Morning workout"
    "project:PERSONAL +reading Read chapter 5"
    "project:10FIFTEEN priority:H Review PR for auth module"
    "project:10FIFTEEN.Backend +deploy Deploy staging fixes"
    "project:SSRP +meeting Sprint planning"
    "project:SSRP priority:L Organise shared drive"
    "+urgent priority:H Call plumber"
    "project:PERSONAL +errands priority:M Return library books"
)

SERVER_URL="http://127.0.0.1:$SERVER_PORT"

for raw in "${TASKS[@]}"; do
    # Add to local TW
    task add $raw 2>/dev/null

    # Add to server
    curl -sf -X POST "$SERVER_URL/api/tasks" \
        -H "Authorization: Bearer $TEST_TOKEN" \
        -H "Content-Type: application/json" \
        -d "{\"raw\": \"$raw\"}" > /dev/null
done

TW_COUNT=$(task count 2>/dev/null)
SERVER_COUNT=$(curl -sf "$SERVER_URL/api/tasks" \
    -H "Authorization: Bearer $TEST_TOKEN" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))")

ok "Taskwarrior: $TW_COUNT tasks"
ok "Server:      $SERVER_COUNT tasks"
echo

# --- 7. Run filter comparisons ---
info "Running filter comparisons"
echo

FILTERS=(
    "status:pending"
    "status:pending +shopping"
    "status:pending project:PERSONAL"
    "status:pending project:PERSONAL.Home"
    "status:pending project:10FIFTEEN"
    "status:pending priority:H"
    "status:pending priority:M"
    "status:pending +urgent"
    "status:pending -shopping"
    "status:pending project:SSRP"
)

PASS=0
FAIL=0

for filter in "${FILTERS[@]}"; do
    echo -e "  ${CYAN}Filter:${NC} $filter"

    # Get TW results
    TW_UUIDS=$(task $filter export 2>/dev/null | python3 -c "
import sys, json
tasks = json.load(sys.stdin)
for t in sorted(tasks, key=lambda x: x.get('uuid','')):
    print(t['uuid'])
" 2>/dev/null || echo "")
    TW_N=$(echo "$TW_UUIDS" | wc -l | tr -d ' ')

    # Get server results — we need to create a temp view with this filter
    # Since the server filters via views, we'll use the direct task list and
    # apply the filter description. For now, get all pending from server.
    SERVER_UUIDS=$(curl -sf "$SERVER_URL/api/tasks" \
        -H "Authorization: Bearer $TEST_TOKEN" | python3 -c "
import sys, json
tasks = json.load(sys.stdin)
for t in sorted(tasks, key=lambda x: x.get('uuid','')):
    print(t['uuid'])
" 2>/dev/null || echo "")
    SERVER_N=$(echo "$SERVER_UUIDS" | wc -l | tr -d ' ')

    # For a proper comparison, we need view-based filtering.
    # Since we don't have views set up, let's compare the UUIDs directly
    # for the base case (status:pending = all pending tasks from both).

    # For the base "status:pending" filter, compare directly
    if [[ "$filter" == "status:pending" ]]; then
        # Both should return all pending tasks — compare UUID counts
        if [[ "$TW_N" -eq "$SERVER_N" ]]; then
            echo -e "    ${GREEN}PASS${NC} TW=$TW_N Server=$SERVER_N (counts match)"
            PASS=$((PASS + 1))
        else
            echo -e "    ${YELLOW}DIFF${NC} TW=$TW_N Server=$SERVER_N"
            FAIL=$((FAIL + 1))
        fi
    else
        # For filtered queries, we need a view. Create one, query, delete.
        VIEW_ID="test-$(echo "$filter" | md5sum | cut -c1-8)"

        # Create view
        VIEW_RESP=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$SERVER_URL/api/views/$VIEW_ID" \
            -H "Authorization: Bearer $TEST_TOKEN" \
            -H "Content-Type: application/json" \
            -d "{\"id\":\"$VIEW_ID\",\"label\":\"Test\",\"icon\":\"star\",\"filter\":\"$filter\"}")

        if [[ "$VIEW_RESP" != "200" ]]; then
            echo -e "    ${RED}FAIL${NC} view creation returned HTTP $VIEW_RESP"
            FAIL=$((FAIL + 1))
            continue
        fi

        # Query via view
        FILTERED_UUIDS=$(curl -s "$SERVER_URL/api/tasks?view=$VIEW_ID" \
            -H "Authorization: Bearer $TEST_TOKEN" | python3 -c "
import sys, json
tasks = json.load(sys.stdin)
for t in sorted(tasks, key=lambda x: x.get('uuid','')):
    print(t['uuid'])
" 2>/dev/null || echo "")
        FILTERED_N=$(echo "$FILTERED_UUIDS" | wc -l | tr -d ' ')

        # Delete view
        curl -sf -X DELETE "$SERVER_URL/api/views/$VIEW_ID" \
            -H "Authorization: Bearer $TEST_TOKEN" > /dev/null 2>&1 || true

        if [[ "$TW_N" -eq "$FILTERED_N" ]]; then
            echo -e "    ${GREEN}PASS${NC} TW=$TW_N Server=$FILTERED_N (counts match)"
            PASS=$((PASS + 1))
        else
            echo -e "    ${YELLOW}DIFF${NC} TW=$TW_N Server=$FILTERED_N"

            # Show the diff
            DIFF_TW=$(comm -23 <(echo "$TW_UUIDS" | sort) <(echo "$FILTERED_UUIDS" | sort) 2>/dev/null)
            DIFF_SRV=$(comm -13 <(echo "$TW_UUIDS" | sort) <(echo "$FILTERED_UUIDS" | sort) 2>/dev/null)

            if [[ -n "$DIFF_TW" ]]; then
                echo -e "    Only in TW:"
                echo "$DIFF_TW" | while read -r uuid; do
                    DESC=$(task $uuid export 2>/dev/null | python3 -c "import sys,json; t=json.load(sys.stdin); print(t[0].get('description','?') if t else '?')" 2>/dev/null || echo "?")
                    echo -e "      - ${uuid:0:8} $DESC"
                done
            fi
            if [[ -n "$DIFF_SRV" ]]; then
                echo -e "    Only in Server:"
                echo "$DIFF_SRV" | while read -r uuid; do
                    echo -e "      + ${uuid:0:8}"
                done
            fi

            FAIL=$((FAIL + 1))
        fi
    fi
done

echo
echo -e "${BOLD}╔══════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║   Results                                    ║${NC}"
echo -e "${BOLD}╚══════════════════════════════════════════════╝${NC}"
echo -e "  Passed: ${GREEN}$PASS${NC}  Failed: ${RED}$FAIL${NC}  Total: $((PASS + FAIL))"
echo

if [[ $FAIL -gt 0 ]]; then
    echo -e "${YELLOW}Some filters produced different results. Check diffs above.${NC}"
    exit 1
else
    echo -e "${GREEN}All filters match between Taskwarrior and the server!${NC}"
fi
