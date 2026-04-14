#!/usr/bin/env bash
# Launch Claude Code with homelab bus channel enabled
# Usage: cl.sh [additional claude args...]

exec claude \
  --dangerously-skip-permissions \
  --dangerously-load-development-channels server:homelab-bus \
  "$@"
