#!/usr/bin/env bash
# Launch Codex with dangerous permissions enabled
# Usage: cx.sh [additional codex args...]

exec codex \
  --dangerously-bypass-approvals-and-sandbox \
  "$@"
