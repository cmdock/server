#!/usr/bin/env bash
set -euo pipefail

REQUIRE_NO_SKIPS=false
REQUIRE_WEBHOOK_PASS=false
INPUT=""

usage() {
    cat <<'EOF'
Usage: scripts/staging_summarize.sh [--require-no-skips] [--require-webhook-pass] [output-file]

Parse cmdock staging-test output and print a compact pass/fail summary.
Exits non-zero when the suite reports failures, when required skips are present,
or when required webhook checks did not pass.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --require-no-skips)
            REQUIRE_NO_SKIPS=true
            shift
            ;;
        --require-webhook-pass)
            REQUIRE_WEBHOOK_PASS=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            if [[ -n "$INPUT" ]]; then
                echo "staging_summarize: unexpected extra argument: $1" >&2
                usage >&2
                exit 2
            fi
            INPUT="$1"
            shift
            ;;
    esac
done

TMP=""
CLEAN=""
cleanup() {
    [[ -n "$TMP" && -f "$TMP" ]] && rm -f "$TMP"
    [[ -n "$CLEAN" && -f "$CLEAN" ]] && rm -f "$CLEAN"
}
trap cleanup EXIT

if [[ -z "$INPUT" ]]; then
    TMP=$(mktemp)
    cat >"$TMP"
    INPUT="$TMP"
fi

if [[ ! -f "$INPUT" ]]; then
    echo "staging_summarize: output file not found: $INPUT" >&2
    exit 2
fi

CLEAN=$(mktemp)
# Strip ANSI escapes so parsing is stable across colored local/CI output.
perl -pe 's/\e\[[0-9;]*[A-Za-z]//g' "$INPUT" >"$CLEAN"

extract_count() {
    local label="$1"
    awk -v label="$label" '$1 == label":" { print $2; found=1 } END { if (!found) print "" }' "$CLEAN" | tail -n1
}

PASSED=$(extract_count "Passed")
FAILED=$(extract_count "Failed")
SKIPPED=$(extract_count "Skipped")
TOTAL=$(extract_count "Total")

if [[ -z "$PASSED" || -z "$FAILED" || -z "$SKIPPED" || -z "$TOTAL" ]]; then
    echo "staging_summarize: could not find final staging summary" >&2
    exit 2
fi

echo "staging summary: passed=$PASSED failed=$FAILED skipped=$SKIPPED total=$TOTAL"

STATUS=0
if [[ "$FAILED" != "0" ]]; then
    echo "staging_summarize: suite reported failures" >&2
    awk '/Failures:/ {show=1; next} show && /^    / {print}' "$CLEAN" >&2 || true
    STATUS=1
fi

if [[ "$REQUIRE_NO_SKIPS" == true && "$SKIPPED" != "0" ]]; then
    echo "staging_summarize: skips present but --require-no-skips was requested" >&2
    grep -F "(skipped)" "$CLEAN" >&2 || true
    STATUS=1
fi

if [[ "$REQUIRE_WEBHOOK_PASS" == true ]]; then
    missing=0
    for expected in \
        "POST /api/webhooks" \
        "Webhook delivery succeeds via HTTPS ingress" \
        "Webhook modifiedFields filter suppresses non-matching change" \
        "Webhook modifiedFields filter emits matching change" \
        "POST /api/webhooks/{id}/test"
    do
        if ! grep -Eq "✓[[:space:]]+$expected$" "$CLEAN"; then
            echo "staging_summarize: missing webhook PASS: $expected" >&2
            missing=1
        fi
    done
    if grep -F "Webhook" "$CLEAN" | grep -F "(skipped)" >&2; then
        echo "staging_summarize: webhook check was skipped" >&2
        missing=1
    fi
    [[ "$missing" == 0 ]] || STATUS=1
fi

exit "$STATUS"
