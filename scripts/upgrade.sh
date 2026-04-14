#!/usr/bin/env bash
# Server upgrade script with pre-flight checks and automatic rollback.
#
# Usage: ./scripts/upgrade.sh [NEW_BINARY_PATH]
#   If no path given, builds from source (cargo build --release)
#
# Steps:
#   1. Pre-flight: verify new binary starts and passes health check
#   2. Back up current binary and config
#   3. Stop current server (graceful drain)
#   4. Swap binary
#   5. Run migrations
#   6. Start new server
#   7. Verify health check
#   8. If health fails: automatic rollback to previous binary
#
# Typical downtime: 2-5 seconds (graceful drain + restart)

set -euo pipefail

# --- Configuration ---
INSTALL_DIR="/usr/local/bin"
BINARY_NAME="cmdock-server"
CONFIG_PATH="/etc/cmdock-server/config.toml"
DATA_DIR="/var/lib/taskchampion/data"
SERVICE_NAME="cmdock-server"
BACKUP_DIR="/var/lib/taskchampion/backups"
HEALTH_URL="http://127.0.0.1:8080/healthz"
HEALTH_TIMEOUT=15  # seconds to wait for health check after restart
PRE_FLIGHT_PORT=18099  # temporary port for pre-flight check

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${BOLD}==> ${NC}$*"; }
ok()    { echo -e "  ${GREEN}✓${NC} $*"; }
fail()  { echo -e "  ${RED}✗${NC} $*"; }
warn()  { echo -e "  ${YELLOW}!${NC} $*"; }

# --- Determine new binary path ---
NEW_BINARY="${1:-}"

if [[ -z "$NEW_BINARY" ]]; then
    info "Building from source..."
    SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
    PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
    cd "$PROJECT_DIR"
    cargo build --release 2>&1 | tail -1
    NEW_BINARY="$PROJECT_DIR/target/release/$BINARY_NAME"
    ok "Built: $NEW_BINARY"
fi

if [[ ! -x "$NEW_BINARY" ]]; then
    fail "Binary not found or not executable: $NEW_BINARY"
    exit 1
fi

CURRENT_BINARY="$INSTALL_DIR/$BINARY_NAME"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)

echo -e "${BOLD}╔══════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║   TaskChampion Server — Upgrade              ║${NC}"
echo -e "${BOLD}╚══════════════════════════════════════════════╝${NC}"
echo

# --- Step 1: Pre-flight check ---
info "Step 1: Pre-flight check (new binary on port $PRE_FLIGHT_PORT)"

# Create temp config pointing at a temp data dir for pre-flight
PRE_FLIGHT_DIR=$(mktemp -d -t tc-preflight-XXXXXX)
mkdir -p "$PRE_FLIGHT_DIR/users"

cat > "$PRE_FLIGHT_DIR/config.toml" <<EOF
[server]
host = "127.0.0.1"
port = $PRE_FLIGHT_PORT
data_dir = "$PRE_FLIGHT_DIR"
EOF

# Run migrations with new binary
if ! "$NEW_BINARY" --config "$PRE_FLIGHT_DIR/config.toml" --migrate 2>/dev/null; then
    fail "Pre-flight: migrations failed with new binary"
    rm -rf "$PRE_FLIGHT_DIR"
    exit 1
fi
ok "Migrations pass"

# Start new binary on temp port
"$NEW_BINARY" --config "$PRE_FLIGHT_DIR/config.toml" &
PRE_FLIGHT_PID=$!
sleep 2

# Health check
if curl -sf --max-time 5 "http://127.0.0.1:$PRE_FLIGHT_PORT/healthz" > /dev/null 2>&1; then
    ok "Health check passes on new binary"
else
    fail "Health check failed on new binary"
    kill "$PRE_FLIGHT_PID" 2>/dev/null || true
    wait "$PRE_FLIGHT_PID" 2>/dev/null || true
    rm -rf "$PRE_FLIGHT_DIR"
    exit 1
fi

# Stop pre-flight server
kill "$PRE_FLIGHT_PID" 2>/dev/null || true
wait "$PRE_FLIGHT_PID" 2>/dev/null || true
rm -rf "$PRE_FLIGHT_DIR"
ok "Pre-flight complete — new binary is healthy"

# --- Step 2: Back up current binary ---
info "Step 2: Back up current installation"
mkdir -p "$BACKUP_DIR"

if [[ -f "$CURRENT_BINARY" ]]; then
    cp "$CURRENT_BINARY" "$BACKUP_DIR/$BINARY_NAME.$TIMESTAMP"
    ok "Backed up: $BACKUP_DIR/$BINARY_NAME.$TIMESTAMP"
else
    warn "No existing binary at $CURRENT_BINARY (fresh install)"
fi

# --- Step 3: Back up data (quick SQLite online backup) ---
info "Step 3: Back up data"
BACKUP_DATA_DIR="$BACKUP_DIR/data-$TIMESTAMP"
mkdir -p "$BACKUP_DATA_DIR"

if [[ -f "$DATA_DIR/config.sqlite" ]]; then
    sqlite3 "$DATA_DIR/config.sqlite" ".backup $BACKUP_DATA_DIR/config.sqlite" 2>/dev/null || \
        cp "$DATA_DIR/config.sqlite" "$BACKUP_DATA_DIR/config.sqlite"
    ok "Config DB backed up"
fi

# --- Step 4: Stop current server (graceful shutdown) ---
info "Step 4: Stop current server (graceful drain)"
DRAIN_START=$(date +%s)

if systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
    systemctl stop "$SERVICE_NAME"
    ok "Server stopped ($(( $(date +%s) - DRAIN_START ))s drain time)"
else
    warn "Service not running (fresh install or already stopped)"
fi

# --- Step 5: Swap binary ---
info "Step 5: Install new binary"
cp "$NEW_BINARY" "$CURRENT_BINARY"
chmod +x "$CURRENT_BINARY"
ok "Installed: $CURRENT_BINARY"

# --- Step 6: Run migrations on real data ---
info "Step 6: Run migrations on production data"
if [[ -f "$CONFIG_PATH" ]]; then
    "$CURRENT_BINARY" --config "$CONFIG_PATH" --migrate 2>&1 | tail -1
    ok "Migrations complete"
else
    warn "No config at $CONFIG_PATH — skipping migrations"
fi

# --- Step 7: Start new server ---
info "Step 7: Start server"
RESTART_START=$(date +%s)

if systemctl is-enabled --quiet "$SERVICE_NAME" 2>/dev/null; then
    systemctl start "$SERVICE_NAME"
else
    warn "systemd service not configured — start manually"
fi

# --- Step 8: Verify health ---
info "Step 8: Verify health (waiting up to ${HEALTH_TIMEOUT}s)"

HEALTHY=false
for i in $(seq 1 $((HEALTH_TIMEOUT * 2))); do
    if curl -sf --max-time 2 "$HEALTH_URL" > /dev/null 2>&1; then
        ELAPSED=$(( $(date +%s) - RESTART_START ))
        ok "Server healthy after ${ELAPSED}s"
        HEALTHY=true
        break
    fi
    sleep 0.5
done

if $HEALTHY; then
    TOTAL_DOWNTIME=$(( $(date +%s) - DRAIN_START ))
    echo
    echo -e "${GREEN}${BOLD}Upgrade complete!${NC}"
    echo -e "  Total downtime: ~${TOTAL_DOWNTIME}s"
    echo -e "  Backup: $BACKUP_DIR/$BINARY_NAME.$TIMESTAMP"
    echo -e "  Data backup: $BACKUP_DATA_DIR/"
else
    echo
    fail "Health check failed after ${HEALTH_TIMEOUT}s — ROLLING BACK"

    # --- Automatic rollback ---
    info "Rolling back to previous binary"

    if [[ -f "$BACKUP_DIR/$BINARY_NAME.$TIMESTAMP" ]]; then
        systemctl stop "$SERVICE_NAME" 2>/dev/null || true
        cp "$BACKUP_DIR/$BINARY_NAME.$TIMESTAMP" "$CURRENT_BINARY"
        systemctl start "$SERVICE_NAME" 2>/dev/null || true

        # Verify rollback
        sleep 2
        if curl -sf --max-time 5 "$HEALTH_URL" > /dev/null 2>&1; then
            ok "Rollback successful — previous version is running"
        else
            fail "ROLLBACK ALSO FAILED — manual intervention required"
            exit 2
        fi
    else
        fail "No backup binary found — manual intervention required"
        exit 2
    fi

    exit 1
fi
