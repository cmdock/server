#!/bin/bash
# deploy.sh — internal deploy helper for cmdock-server
#
# Internal staging and dogfood are ops-managed environments. This script owns
# only the server artifact flow:
#   - build locally
#   - docker save/scp/load onto the internal host
#   - recreate the `server` service in the root ops-managed stack
#
# It intentionally does NOT manage host runtime composition, ingress, tokens,
# or sibling services such as Caddy or the control plane.
#
# Usage:
#   ./scripts/deploy.sh local              # Build + push server image to staging
#   ./scripts/deploy.sh local dogfood      # Build + push server image to dogfood
#   ./scripts/deploy.sh publish            # Build + push public server image to ghcr.io/cmdock/server
#   ./scripts/deploy.sh staging            # Recreate staging server service in root stack
#   ./scripts/deploy.sh dogfood            # Recreate dogfood server service in root stack
#   ./scripts/deploy.sh status             # Show staging server/container status
#   ./scripts/deploy.sh status dogfood     # Show dogfood server/container status
#   ./scripts/deploy.sh logs               # Tail staging server logs
#   ./scripts/deploy.sh logs dogfood       # Tail dogfood server logs
#   ./scripts/deploy.sh health             # Health check staging server
#   ./scripts/deploy.sh health dogfood     # Health check dogfood server

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

GHCR_IMAGE="ghcr.io/cmdock/server"
LOCAL_IMAGE="cmdock-server:latest"
LOCAL_WEBHOOK_RECEIVER_IMAGE="cmdock-webhook-receiver:latest"
ROOT_COMPOSE_FILE="/opt/cmdock/docker-compose.yml"
SERVER_SERVICE="server"
SERVER_CONTAINER="cmdock-server"

GREEN='\033[0;32m'
RED='\033[0;31m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${BOLD}==> ${NC}$*"; }
ok()    { echo -e "  ${GREEN}✓${NC} $*"; }
fail()  { echo -e "  ${RED}✗${NC} $*"; }

target_ssh_host() {
    case "${1:-staging}" in
        staging) echo "${CMDOCK_STAGING_SSH:-staging.example.com}" ;;
        dogfood) echo "${CMDOCK_DOGFOOD_SSH:-dogfood.example.com}" ;;
        *)
            fail "Unknown target: ${1:-}"
            exit 1
            ;;
    esac
}

SSH_OPTS=(-o ControlMaster=no)
SCP_OPTS=(-o ControlMaster=no)

# Build the server image locally.
#
# The public Dockerfile is the default. Deployments that need extra trust
# material (e.g. an internal CA bundle) can set CMDOCK_DOCKERFILE to a private
# Dockerfile variant checked in outside the public release tree.
#
# Uses --network=host so `cargo` inside the build can resolve crates.io on
# hosts where the default docker bridge network can't reach DNS (for example
# when the host's resolver is Tailscale magic-DNS on 100.100.100.100, which
# is not reachable from the docker0 bridge).
build_server_image() {
    local dockerfile="${1:-${CMDOCK_DOCKERFILE:-Dockerfile}}"
    info "Building Docker image with $dockerfile..."
    cd "$PROJECT_DIR"
    docker build --network=host -f "$dockerfile" -t "$LOCAL_IMAGE" .
    ok "Image built: $LOCAL_IMAGE"
}

build_webhook_receiver_image() {
    info "Building internal webhook receiver image..."
    cd "$PROJECT_DIR"
    docker build --network=host -f deploy/Dockerfile.webhook-receiver -t "$LOCAL_WEBHOOK_RECEIVER_IMAGE" .
    ok "Image built: $LOCAL_WEBHOOK_RECEIVER_IMAGE"
}

# Push server image to a VM via docker save/scp/load (no registry)
push_local() {
    local target="${1:-staging}"
    local ssh_host
    ssh_host="$(target_ssh_host "$target")"

    build_server_image
    build_webhook_receiver_image

    info "Pushing server and webhook receiver images to $target via scp..."
    local tmpfile="/tmp/cmdock-server-bundle-$(date +%s).tar.gz"
    docker save "$LOCAL_IMAGE" "$LOCAL_WEBHOOK_RECEIVER_IMAGE" | gzip > "$tmpfile"
    local size=$(du -h "$tmpfile" | cut -f1)
    ok "Image saved: $tmpfile ($size)"

    scp "${SCP_OPTS[@]}" "$tmpfile" "$ssh_host:/tmp/cmdock-server-bundle.tar.gz"
    ssh "${SSH_OPTS[@]}" "$ssh_host" "sudo docker load < /tmp/cmdock-server-bundle.tar.gz && rm -f /tmp/cmdock-server-bundle.tar.gz"
    rm "$tmpfile"
    ok "Images loaded on $target"

    restart_target "$target"
}

# Build and push the public self-host server image to GHCR.
publish() {
    build_server_image "Dockerfile"

    local sha=$(git rev-parse --short HEAD)
    info "Pushing to GHCR..."

    docker tag "$LOCAL_IMAGE" "$GHCR_IMAGE:latest"
    docker tag "$LOCAL_IMAGE" "$GHCR_IMAGE:$sha"
    docker push "$GHCR_IMAGE:latest"
    docker push "$GHCR_IMAGE:$sha"
    ok "Pushed: $GHCR_IMAGE:latest and $GHCR_IMAGE:$sha"
}

# Recreate the server service inside the root ops-managed compose stack
restart_target() {
    local target="${1:-staging}"
    local ssh_host
    ssh_host="$(target_ssh_host "$target")"

    info "Recreating $target server service in root stack..."
    ssh "${SSH_OPTS[@]}" "$ssh_host" "sudo docker compose -f $ROOT_COMPOSE_FILE up -d --no-deps --force-recreate $SERVER_SERVICE"
    sleep 3
    health_check "$target"
}

# Health check
health_check() {
    local target="${1:-staging}"
    local ssh_host
    ssh_host="$(target_ssh_host "$target")"

    info "Health check..."
    local response
    response=$(ssh "${SSH_OPTS[@]}" "$ssh_host" "curl -sf http://localhost:8080/healthz" 2>/dev/null || true)
    if [[ -n "$response" ]]; then
        ok "$target healthy"
        echo "$response" | python3 -m json.tool 2>/dev/null || echo "$response"
    else
        fail "$target health check failed"
        return 1
    fi
}

# Show container status
show_status() {
    local target="${1:-staging}"
    local ssh_host
    ssh_host="$(target_ssh_host "$target")"

    info "$target status:"
    ssh "${SSH_OPTS[@]}" "$ssh_host" "sudo docker compose -f $ROOT_COMPOSE_FILE ps $SERVER_SERVICE"
    echo ""
    info "Server image:"
    ssh "${SSH_OPTS[@]}" "$ssh_host" "sudo docker inspect $SERVER_CONTAINER --format '{{.Config.Image}} {{.Image}}' 2>/dev/null || echo 'server container not running'"
}

# Tail logs
show_logs() {
    local target="${1:-staging}"
    local ssh_host
    ssh_host="$(target_ssh_host "$target")"

    info "$target server logs (Ctrl+C to exit):"
    ssh "${SSH_OPTS[@]}" "$ssh_host" "sudo docker compose -f $ROOT_COMPOSE_FILE logs -f --tail=50 $SERVER_SERVICE"
}

# Main
case "${1:-help}" in
    local)
        push_local "${2:-staging}"
        ;;
    publish)
        publish
        ;;
    staging)
        if [[ $# -gt 1 ]]; then
            fail "staging does not accept image-source switches in the converged internal model"
            exit 1
        fi
        restart_target "staging"
        ;;
    dogfood)
        if [[ $# -gt 1 ]]; then
            fail "dogfood does not accept image-source switches in the converged internal model"
            exit 1
        fi
        restart_target "dogfood"
        ;;
    status)
        show_status "${2:-staging}"
        ;;
    logs)
        show_logs "${2:-staging}"
        ;;
    health)
        health_check "${2:-staging}"
        ;;
    *)
        echo "Usage: $0 <command> [options]"
        echo ""
        echo "Build commands:"
        echo "  local [target]     Build + push server image to target via scp/load (default: staging)"
        echo "  publish            Build + push the public self-host server image to GHCR"
        echo ""
        echo "Internal runtime commands:"
        echo "  staging            Recreate staging server service in /opt/cmdock/docker-compose.yml"
        echo "  dogfood            Recreate dogfood server service in /opt/cmdock/docker-compose.yml"
        echo "  status [target]    Show server service status + current image"
        echo "  logs [target]      Tail server logs"
        echo "  health [target]    Health check server"
        ;;
esac
