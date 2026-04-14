#!/usr/bin/env bash
# Load test runner: spins up an isolated server with profile-shaped users/devices,
# runs Goose load tests, then reports Prometheus metrics.
#
# Profiles:
#   mixed                     - baseline blend of personal users plus one shared team
#   personal-only             - isolated users only, no shared contention
#   team-contention           - all VUs hammer one shared team user/device
#   multi-device-single-user  - one user with many registered devices
#
# Usage: ./scripts/load-test.sh [USERS] [DURATION] [PERSONAL_USERS] [SEED_TASKS] [--profile PROFILE]
#   USERS          — total concurrent virtual users (default: 20)
#   DURATION       — test duration (default: 30s)
#   PERSONAL_USERS — mixed-profile personal user count (default: 5)
#   SEED_TASKS     — tasks to seed per token/user (default: 10, try 100/500 for large replicas)
#   PROFILE        — mixed | personal-only | team-contention | multi-device-single-user

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUBBLE_DIR=$(mktemp -d -t tc-load-test-XXXXXX)
SERVER_PORT=18090
USERS=20
DURATION=30s
PERSONAL_COUNT=5
SEED_TASKS=10
PROFILE="mixed"
SUMMARY_JSON=""
SERVER_PID=""

GREEN='\033[0;32m'
RED='\033[0;31m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${BOLD}==> ${NC}$*"; }
ok()    { echo -e "  ${GREEN}✓${NC} $*"; }
warn()  { echo -e "  ${YELLOW}!${NC} $*"; }

usage() {
    cat <<EOF
Usage: ./scripts/load-test.sh [USERS] [DURATION] [PERSONAL_USERS] [SEED_TASKS] [--profile PROFILE]

Profiles:
  mixed
  personal-only
  team-contention
  multi-device-single-user
EOF
}

POSITIONAL=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile)
            PROFILE="${2:-}"
            shift 2
            ;;
        --personal-users)
            PERSONAL_COUNT="${2:-}"
            shift 2
            ;;
        --seed-tasks)
            SEED_TASKS="${2:-}"
            shift 2
            ;;
        --summary-json)
            SUMMARY_JSON="${2:-}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            POSITIONAL+=("$1")
            shift
            ;;
    esac
done

if [[ ${#POSITIONAL[@]} -ge 1 ]]; then
    USERS="${POSITIONAL[0]}"
fi
if [[ ${#POSITIONAL[@]} -ge 2 ]]; then
    DURATION="${POSITIONAL[1]}"
fi
if [[ ${#POSITIONAL[@]} -ge 3 ]]; then
    PERSONAL_COUNT="${POSITIONAL[2]}"
fi
if [[ ${#POSITIONAL[@]} -ge 4 ]]; then
    SEED_TASKS="${POSITIONAL[3]}"
fi

case "$PROFILE" in
    mixed|personal-only|team-contention|multi-device-single-user) ;;
    *)
        echo -e "${RED}Invalid profile: $PROFILE${NC}"
        usage
        exit 1
        ;;
esac

if [[ "$PROFILE" == "team-contention" && "$USERS" -gt 20 ]]; then
    warn "team-contention above 20 VUs is intentionally pessimistic; mixed is usually the more realistic team profile"
fi

if [[ -z "$SUMMARY_JSON" ]]; then
    SUMMARY_JSON="$PROJECT_DIR/load-test-summary.json"
fi

cleanup() {
    info "Cleaning up..."
    if [[ -n "${SAMPLER_PID:-}" ]] && kill -0 "$SAMPLER_PID" 2>/dev/null; then
        kill "$SAMPLER_PID" 2>/dev/null || true
        wait "$SAMPLER_PID" 2>/dev/null || true
    fi
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        echo
        info "Final Prometheus metrics (key counters):"
        curl -s --max-time 5 "http://127.0.0.1:$SERVER_PORT/metrics" 2>/dev/null | \
            grep -E "^(http_requests_total|http_request_duration|replica_open|replica_operation|config_db_quer|filter_|sqlite_busy|http_requests_in_flight|llm_|replica_count|sync_)" | \
            sort || true
        echo

        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
        ok "Server stopped"
    fi
    rm -rf "$BUBBLE_DIR"
    ok "Temp directory removed"
}
trap cleanup EXIT

# Dependency check
for cmd in python3 curl cargo bc; do
    if ! command -v "$cmd" &>/dev/null; then
        echo -e "${RED}Missing dependency: $cmd${NC}"
        exit 1
    fi
done

# Check Python cryptography module (needed for AES-256-GCM encryption of sync secrets)
if ! python3 -c "from cryptography.hazmat.primitives.ciphers.aead import AESGCM" 2>/dev/null; then
    echo -e "${RED}Missing Python dependency: cryptography${NC}"
    echo -e "  Install with: pip install cryptography"
    exit 1
fi

echo -e "${BOLD}╔══════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║   TaskChampion Server — Multi-User Load Test         ║${NC}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════╝${NC}"
echo -e "  VUs: ${CYAN}$USERS${NC}  Duration: ${CYAN}$DURATION${NC}  Profile: ${CYAN}$PROFILE${NC}  Seed: ${CYAN}$SEED_TASKS${NC} tasks/token"
case "$PROFILE" in
    mixed)
        echo -e "  Pattern: ${CYAN}$PERSONAL_COUNT${NC} personal users + ${CYAN}1${NC} shared team user"
        ;;
    personal-only)
        echo -e "  Pattern: ${CYAN}$USERS${NC} isolated personal users"
        ;;
    team-contention)
        echo -e "  Pattern: ${CYAN}$USERS${NC} VUs sharing ${CYAN}1${NC} hot team user/device"
        ;;
    multi-device-single-user)
        echo -e "  Pattern: ${CYAN}$USERS${NC} devices attached to ${CYAN}1${NC} user"
        ;;
esac
echo

# --- 1. Build ---
info "Building server and load test (release)..."
cd "$PROJECT_DIR"
cargo build --release --bin cmdock-server --bin load-test 2>&1 | tail -3
ok "Binaries built"

# --- 2. Set up isolated server ---
info "Setting up isolated server"
SERVER_DATA="$BUBBLE_DIR/server-data"
mkdir -p "$SERVER_DATA/users"

# Generate a random 32-byte master key (hex-encoded) for the sync bridge.
# This enables the full REST ↔ TC sync bridge path during load testing,
# adding realistic per-request sync overhead (~10-50ms).
MASTER_KEY=$(python3 -c "import os; print(os.urandom(32).hex())")
export CMDOCK_MASTER_KEY="$MASTER_KEY"

cat > "$BUBBLE_DIR/config.toml" <<EOF
[server]
host = "127.0.0.1"
port = $SERVER_PORT
data_dir = "$SERVER_DATA"
EOF

"$PROJECT_DIR/target/release/cmdock-server" --config "$BUBBLE_DIR/config.toml" --migrate 2>&1 | tail -1
ok "Master key generated (sync bridge ENABLED)"

# --- 3. Create users ---
info "Creating users and tokens"

TOKENS_FILE="$BUBBLE_DIR/tokens.txt"
TEAM_TOKEN=""

python3 "$SCRIPT_DIR/load_test_seed.py" \
    --db-path "$SERVER_DATA/config.sqlite" \
    --server-data "$SERVER_DATA" \
    --tokens-file "$TOKENS_FILE" \
    --master-key "$MASTER_KEY" \
    --profile "$PROFILE" \
    --users "$USERS" \
    --personal-count "$PERSONAL_COUNT"

ok "$(cat "$TOKENS_FILE" | wc -l) user tokens created"

# --- 4. Start server and seed data ---
info "Starting server and seeding data"

"$PROJECT_DIR/target/release/cmdock-server" --config "$BUBBLE_DIR/config.toml" &
SERVER_PID=$!

# Poll for server readiness (up to 10 seconds)
for i in $(seq 1 20); do
    if curl -sf --max-time 2 "http://127.0.0.1:$SERVER_PORT/healthz" > /dev/null 2>&1; then
        READY_MS=$(echo "$i * 500" | bc)
        ok "Server running (PID $SERVER_PID) — ready after ${READY_MS}ms"
        break
    fi
    if [[ $i -eq 20 ]]; then
        echo -e "${RED}Server failed to start after 10s${NC}"
        exit 1
    fi
    sleep 0.5
done

# Seed tasks per user so reads have data (continue on individual failures).
# For large seed counts, vary task attributes to create realistic replicas.
# Token format: "type:bearer_token:client_id:sync_secret"
# Users seed in parallel (background subshells), sequential within each user.
USER_COUNT=0
SEED_PIDS=()
PRIORITIES=("H" "M" "L" "M" "H")
PROJECTS=("work" "home" "errands" "health" "study")
SEED_FAIL_DIR=$(mktemp -d -t tc-seed-fail-XXXXXX)
SEED_START=$(date +%s)

while IFS= read -r line; do
    type="${line%%:*}"
    rest="${line#*:}"
    token="${rest%%:*}"
    USER_COUNT=$((USER_COUNT + 1))
    # Each user seeds in a background subshell; failures tracked via temp file
    (
        fail_count=0
        for i in $(seq 1 "$SEED_TASKS"); do
            pri=${PRIORITIES[$((i % 5))]}
            proj=${PROJECTS[$((i % 5))]}
            # Add due dates to ~30% of tasks for filter/urgency testing
            due=""
            if (( i % 3 == 0 )); then
                due=" due:$(date -d "+$((i % 14)) days" +%Y-%m-%d 2>/dev/null || date -v+$((i % 14))d +%Y-%m-%d 2>/dev/null || true)"
            fi
            if ! curl -sf --max-time 10 -X POST "http://127.0.0.1:$SERVER_PORT/api/tasks" \
                -H "Authorization: Bearer $token" \
                -H "Content-Type: application/json" \
                -d "{\"raw\": \"+load_seed project:LOAD.$proj priority:$pri${due} Seed task $i for $type\"}" > /dev/null 2>&1; then
                fail_count=$((fail_count + 1))
            fi
        done
        echo "$fail_count" > "$SEED_FAIL_DIR/user-${USER_COUNT}.fail"
    ) &
    SEED_PIDS+=($!)
done < "$TOKENS_FILE"

# Wait only for seed subshells (not the server process)
for pid in "${SEED_PIDS[@]}"; do
    wait "$pid" 2>/dev/null || true
done

SEED_ELAPSED=$(( $(date +%s) - SEED_START ))
TOTAL_SEEDS=$(( USER_COUNT * SEED_TASKS ))

# Sum failures from all per-user temp files
SEED_FAIL=0
for f in "$SEED_FAIL_DIR"/*.fail; do
    [[ -f "$f" ]] || continue
    count=$(cat "$f")
    SEED_FAIL=$((SEED_FAIL + count))
done
rm -rf "$SEED_FAIL_DIR"

if [[ $SEED_FAIL -gt 0 ]]; then
    echo -e "  ${YELLOW}!${NC} $SEED_FAIL of $TOTAL_SEEDS seed requests failed (continuing, ${SEED_ELAPSED}s)"
else
    ok "Seeded $TOTAL_SEEDS tasks ($USER_COUNT users × $SEED_TASKS) in ${SEED_ELAPSED}s"
fi

# --- 5. Capture baseline and start memory monitor ---
MEMORY_LOG="$BUBBLE_DIR/memory-samples.csv"
echo "timestamp,rss_mb,vss_mb,fds,sync_in_flight,http_in_flight,disk_mb" > "$MEMORY_LOG"

# Capture baseline RSS before load
BASELINE_RSS=$(ps -o rss= -p "$SERVER_PID" 2>/dev/null | tr -d ' ')
BASELINE_RSS_MB=$(echo "scale=1; ${BASELINE_RSS:-0} / 1024" | bc)
BASELINE_DISK_KB=$(du -sk "$SERVER_DATA" 2>/dev/null | cut -f1)
ok "Baseline: RSS=${BASELINE_RSS_MB}MB, disk=$(echo "scale=1; ${BASELINE_DISK_KB:-0} / 1024" | bc)MB"

# Background memory sampler — polls every 2s during the load test
sample_memory() {
    while kill -0 "$SERVER_PID" 2>/dev/null; do
        local ts rss_kb rss_mb vss_kb vss_mb fds sync_inf http_inf disk_kb disk_mb
        ts=$(date +%s)
        rss_kb=$(ps -o rss= -p "$SERVER_PID" 2>/dev/null | tr -d ' ')
        vss_kb=$(ps -o vsz= -p "$SERVER_PID" 2>/dev/null | tr -d ' ')
        rss_mb=$(echo "scale=1; ${rss_kb:-0} / 1024" | bc)
        vss_mb=$(echo "scale=1; ${vss_kb:-0} / 1024" | bc)
        fds=$(ls /proc/"$SERVER_PID"/fd 2>/dev/null | wc -l)
        # Pull gauges from Prometheus endpoint (best-effort)
        local metrics_out
        metrics_out=$(curl -sf --max-time 2 "http://127.0.0.1:$SERVER_PORT/metrics" 2>/dev/null || true)
        sync_inf=$(echo "$metrics_out" | grep -m1 '^sync_storage_in_flight ' | awk '{print $2}' || echo "0")
        http_inf=$(echo "$metrics_out" | grep -m1 '^http_requests_in_flight ' | awk '{print $2}' || echo "0")
        disk_kb=$(du -sk "$SERVER_DATA" 2>/dev/null | cut -f1)
        disk_mb=$(echo "scale=1; ${disk_kb:-0} / 1024" | bc)
        echo "$ts,$rss_mb,$vss_mb,$fds,${sync_inf:-0},${http_inf:-0},$disk_mb" >> "$MEMORY_LOG"
        sleep 2
    done
}

sample_memory &
SAMPLER_PID=$!

# --- 6. Run load test ---
echo
info "Running Goose load test: $USERS VUs for $DURATION"
case "$PROFILE" in
    mixed)
        ACTUAL_PERSONAL=$(( USERS < PERSONAL_COUNT ? USERS : PERSONAL_COUNT ))
        TEAM_VUS=$(( USERS > PERSONAL_COUNT ? USERS - PERSONAL_COUNT : 0 ))
        echo -e "  Personal VUs: ${CYAN}$ACTUAL_PERSONAL${NC} (1 VU per replica — isolated)"
        echo -e "  Team VUs:     ${CYAN}$TEAM_VUS${NC} (all share 1 replica — tests contention)"
        ;;
    personal-only)
        echo -e "  Personal VUs: ${CYAN}$USERS${NC} (isolated replicas only)"
        ;;
    team-contention)
        echo -e "  Team VUs:     ${CYAN}$USERS${NC} (all share 1 replica and 1 device)"
        ;;
    multi-device-single-user)
        echo -e "  Device VUs:   ${CYAN}$USERS${NC} (1 user, many registered devices)"
        ;;
esac
echo

export TC_LOAD_TOKENS_FILE="$TOKENS_FILE"
export TC_LOAD_PROFILE="$PROFILE"
RUN_LOG="$BUBBLE_DIR/load-test-output.log"

"$PROJECT_DIR/target/release/load-test" \
    --host "http://127.0.0.1:$SERVER_PORT" \
    --users "$USERS" \
    --startup-time 3s \
    --run-time "$DURATION" \
    --report-file "$BUBBLE_DIR/report.html" \
    --no-reset-metrics \
    2>&1 | tee "$RUN_LOG"

# Stop sampler
kill "$SAMPLER_PID" 2>/dev/null || true
wait "$SAMPLER_PID" 2>/dev/null || true

echo
info "Load test complete"

# --- 7. Memory & resource report ---
echo
echo -e "${BOLD}╔══════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║   Resource Usage Report (for prod VM sizing)         ║${NC}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════╝${NC}"

FINAL_RSS=$(ps -o rss= -p "$SERVER_PID" 2>/dev/null | tr -d ' ')
FINAL_RSS_MB=$(echo "scale=1; ${FINAL_RSS:-0} / 1024" | bc)
FINAL_DISK_KB=$(du -sk "$SERVER_DATA" 2>/dev/null | cut -f1)
FINAL_DISK_MB=$(echo "scale=1; ${FINAL_DISK_KB:-0} / 1024" | bc)

# Compute peak from samples
PEAK_RSS_MB=$(tail -n +2 "$MEMORY_LOG" | cut -d, -f2 | sort -rn | head -1)
PEAK_VSS_MB=$(tail -n +2 "$MEMORY_LOG" | cut -d, -f3 | sort -rn | head -1)
PEAK_FDS=$(tail -n +2 "$MEMORY_LOG" | cut -d, -f4 | sort -rn | head -1)
PEAK_SYNC_INF=$(tail -n +2 "$MEMORY_LOG" | cut -d, -f5 | sort -rn | head -1)
SAMPLE_COUNT=$(tail -n +2 "$MEMORY_LOG" | wc -l)

echo -e "  ${BOLD}Memory${NC}"
echo -e "    Baseline RSS:    ${CYAN}${BASELINE_RSS_MB}${NC} MB"
echo -e "    Peak RSS:        ${CYAN}${PEAK_RSS_MB:-?}${NC} MB  (from $SAMPLE_COUNT samples @ 2s)"
echo -e "    Final RSS:       ${CYAN}${FINAL_RSS_MB}${NC} MB"
echo -e "    Peak VSS:        ${CYAN}${PEAK_VSS_MB:-?}${NC} MB"
RSS_GROWTH=$(echo "scale=1; ${PEAK_RSS_MB:-0} - ${BASELINE_RSS_MB}" | bc)
echo -e "    RSS growth:      ${CYAN}${RSS_GROWTH}${NC} MB  (load-induced)"
if [[ "$USERS" -gt 0 ]]; then
    PER_VU=$(echo "scale=2; ${RSS_GROWTH} / $USERS" | bc)
    echo -e "    Per-VU estimate: ${CYAN}${PER_VU}${NC} MB/VU"
fi

echo -e "  ${BOLD}File descriptors${NC}"
echo -e "    Peak FDs:        ${CYAN}${PEAK_FDS:-?}${NC}"

echo -e "  ${BOLD}Concurrency${NC}"
echo -e "    Peak sync in-flight: ${CYAN}${PEAK_SYNC_INF:-0}${NC}"

echo -e "  ${BOLD}Disk${NC} (data dir under /tmp — may be tmpfs/RAM-backed)"
echo -e "    Baseline:        ${CYAN}$(echo "scale=1; ${BASELINE_DISK_KB:-0} / 1024" | bc)${NC} MB"
echo -e "    Final:           ${CYAN}${FINAL_DISK_MB}${NC} MB"
DISK_GROWTH=$(echo "scale=1; ${FINAL_DISK_MB} - ${BASELINE_DISK_KB:-0} / 1024" | bc)
echo -e "    Disk growth:     ${CYAN}${DISK_GROWTH}${NC} MB"

echo
echo -e "  ${BOLD}Raw samples:${NC} $MEMORY_LOG"

# Copy memory log for analysis
cp "$MEMORY_LOG" "$PROJECT_DIR/load-test-memory.csv"
ok "Memory samples saved to load-test-memory.csv"

if [[ -f "$BUBBLE_DIR/report.html" ]]; then
    cp "$BUBBLE_DIR/report.html" "$PROJECT_DIR/load-test-report.html"
    ok "Report saved to load-test-report.html"
fi

FINAL_METRICS="$BUBBLE_DIR/final-metrics.prom"
curl -sf --max-time 5 "http://127.0.0.1:$SERVER_PORT/metrics" > "$FINAL_METRICS"

python3 "$SCRIPT_DIR/load_test_summary.py" \
    --run-log "$RUN_LOG" \
    --metrics "$FINAL_METRICS" \
    --summary-json "$SUMMARY_JSON" \
    --profile "$PROFILE" \
    --users "$USERS" \
    --duration "$DURATION" \
    --personal-count "$PERSONAL_COUNT" \
    --seed-tasks "$SEED_TASKS" \
    --startup-ready-ms "${READY_MS:-0}" \
    --seed-elapsed-s "$SEED_ELAPSED" \
    --baseline-rss-mb "$BASELINE_RSS_MB" \
    --peak-rss-mb "${PEAK_RSS_MB:-0}" \
    --final-rss-mb "$FINAL_RSS_MB" \
    --peak-vss-mb "${PEAK_VSS_MB:-0}" \
    --peak-fds "${PEAK_FDS:-0}" \
    --peak-sync-in-flight "${PEAK_SYNC_INF:-0}" \
    --baseline-disk-mb "$(echo "scale=1; ${BASELINE_DISK_KB:-0} / 1024" | bc)" \
    --final-disk-mb "$FINAL_DISK_MB" \
    --sample-count "$SAMPLE_COUNT"
ok "Summary saved to $SUMMARY_JSON"
