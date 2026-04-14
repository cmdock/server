#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 6 ]]; then
    echo "Usage: $0 <ssh-host> <server-url> <user-id> <api-token> <client-id> <encryption-secret>" >&2
    exit 1
fi

SSH_HOST="$1"
SERVER_URL="$2"
USER_ID="$3"
API_TOKEN="$4"
CLIENT_ID="$5"
SECRET="$6"

ssh -o ControlMaster=no -o ControlPath=none "$SSH_HOST" \
    "USER_ID='$USER_ID' SERVER_URL='$SERVER_URL' API_TOKEN='$API_TOKEN' CLIENT_ID='$CLIENT_ID' SECRET='$SECRET' bash -s" <<'EOF'
set -euo pipefail

TW_DIR=$(mktemp -d -t dogfood-vm-tw-smoke-XXXXXX)
ROOTS="$TW_DIR/roots.pem"
trap 'rm -rf "$TW_DIR"' EXIT

dump_logs() {
  echo "=== /tmp/dogfood-vm-tw-sync1.log ===" >&2
  cat /tmp/dogfood-vm-tw-sync1.log >&2 || true
  echo "=== /tmp/dogfood-vm-tw-add.log ===" >&2
  cat /tmp/dogfood-vm-tw-add.log >&2 || true
  echo "=== /tmp/dogfood-vm-tw-sync2.log ===" >&2
  cat /tmp/dogfood-vm-tw-sync2.log >&2 || true
}

trap dump_logs ERR

mkdir -p "$TW_DIR/data"
# Optional: fetch an internal CA bundle. Skip when CMDOCK_CA_ROOTS_URL is unset
# (e.g. your deployment uses a public CA).
if [[ -n "${CMDOCK_CA_ROOTS_URL:-}" ]]; then
  curl -fsSL "$CMDOCK_CA_ROOTS_URL" -o "$ROOTS"
  export SSL_CERT_FILE="$ROOTS"
  export CURL_CA_BUNDLE="$ROOTS"
fi

cat >"$TW_DIR/taskrc" <<TASKRC
confirmation=no
verbose=nothing
news.version=0
sync.server.url=$SERVER_URL
sync.server.client_id=$CLIENT_ID
sync.encryption_secret=$SECRET
data.location=$TW_DIR/data
TASKRC

TASKRC="$TW_DIR/taskrc" TASKDATA="$TW_DIR/data" task sync >/tmp/dogfood-vm-tw-sync1.log 2>&1
TASKRC="$TW_DIR/taskrc" TASKDATA="$TW_DIR/data" task add "+dogfood_smoke VM isolated TW smoke task" project:DOGFOOD >/tmp/dogfood-vm-tw-add.log 2>&1
TASKRC="$TW_DIR/taskrc" TASKDATA="$TW_DIR/data" task sync >/tmp/dogfood-vm-tw-sync2.log 2>&1

curl -fsSL -H "Authorization: Bearer $API_TOKEN" "$SERVER_URL/api/tasks" \
  | jq -e '.[] | select(.description == "VM isolated TW smoke task")' >/dev/null

printf 'vm_isolated_taskwarrior_smoke=pass\nuser_id=%s\nclient_id=%s\n' "$USER_ID" "$CLIENT_ID"
EOF
