#!/usr/bin/env bash
# Guard the MergedSyncGateway TaskChampion history codec boundary.
#
# This is intentionally mechanical: it makes TaskChampion upgrades and raw
# history-segment parsing boundary drift noisy in local `just check` and CI.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

review_file="tests/fixtures/tc_history/CODEC_REVIEW.toml"

current_version="$(awk '
  /^\[\[package\]\]/ { in_pkg = 0 }
  /^name = "taskchampion"/ { in_pkg = 1 }
  in_pkg && /^version = / {
    gsub(/^version = "/, "")
    gsub(/"$/, "")
    print
    exit
  }
' Cargo.lock)"

reviewed_version="$(awk -F'"' '/^taskchampion_version[[:space:]]*=/ { print $2; exit }' "$review_file")"

if [[ -z "$current_version" ]]; then
  echo "error: could not find taskchampion version in Cargo.lock" >&2
  exit 1
fi

if [[ -z "$reviewed_version" ]]; then
  echo "error: could not find taskchampion_version in $review_file" >&2
  exit 1
fi

if [[ "$current_version" != "$reviewed_version" ]]; then
  cat >&2 <<EOF
error: taskchampion version changed without merged-sync codec review
  Cargo.lock:                 $current_version
  $review_file: $reviewed_version

Regenerate/review tests/fixtures/tc_history/generated/*.json via:
  cargo test merged_sync_gateway::codec::tests::regenerate_taskchampion_history_fixtures --lib -- --ignored --exact
Then run:
  cargo test merged_sync_gateway::codec --lib
  just codec-gate
and update $review_file in the same commit.
EOF
  exit 1
fi

raw_key_matches="$(rg -n '"(operations|Create|Update|Delete)"' src \
  --glob '*.rs' \
  --glob '!src/bin/**' \
  --glob '!src/merged_sync_gateway/codec.rs' || true)"

if [[ -n "$raw_key_matches" ]]; then
  cat >&2 <<EOF
error: raw TaskChampion history JSON keys outside merged_sync_gateway::codec

$raw_key_matches

Raw history-segment parsing/pattern matching belongs in src/merged_sync_gateway/codec.rs.
EOF
  exit 1
fi

gateway_value_matches="$(rg -n 'serde_json::Value' src/merged_sync_gateway \
  --glob '*.rs' \
  --glob '!src/merged_sync_gateway/codec.rs' || true)"

if [[ -n "$gateway_value_matches" ]]; then
  cat >&2 <<EOF
error: raw serde_json::Value inside merged_sync_gateway outside codec

$gateway_value_matches

Gateway routing/planner code should consume typed codec values, not raw JSON.
EOF
  exit 1
fi

history_segment_matches="$(rg -n 'HistorySegment' src \
  --glob '*.rs' \
  --glob '!src/tc_sync/bridge.rs' \
  --glob '!src/merged_sync_gateway/codec.rs' || true)"

if [[ -n "$history_segment_matches" ]]; then
  cat >&2 <<EOF
error: direct TaskChampion HistorySegment use outside reviewed adapter/codec boundary

$history_segment_matches

Review new direct HistorySegment usage before extending the gateway boundary.
EOF
  exit 1
fi

echo "merged-sync codec gate ok (taskchampion $current_version)"
