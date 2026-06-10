#!/usr/bin/env bash
# HC-2 import-boundary enforcement (ADR-0002 §HC-2).
#
# Greps for both `use crate::X::` AND fully-qualified `crate::X::` patterns
# (incl. fully-qualified function calls), since the 2026-05-04 review found
# that `tasks/handlers.rs` calling `crate::views::defaults::reconcile_default_views`
# bypassed the original `use`-only sweep. See server#128.
#
# Exits non-zero on any violation. Output names file:line + offending line.

set -euo pipefail

cd "$(dirname "$0")/.."

VIOLATIONS=0

# Each rule: source-glob → forbidden-modules
# Per ADR-0002 §HC-2 lines 207-211.
check() {
    local src_glob="$1"
    local label="$2"
    shift 2
    local forbidden=("$@")

    for forb in "${forbidden[@]}"; do
        # Match either:
        #   use crate::<forb>::      (use-import form)
        #   crate::<forb>::          (fully-qualified path)
        # Excluding:
        #   //  (line comments)
        #   ///, //!  (doc comments)
        local pattern="\bcrate::${forb}::"
        # shellcheck disable=SC2086
        local hits
        hits=$(rg -n --type rust "$pattern" $src_glob 2>/dev/null \
            | grep -vE '^\s*[^:]+:\s*[0-9]+:\s*//' \
            || true)
        if [[ -n "$hits" ]]; then
            echo "HC-2 violation: $label must NOT depend on \`$forb\`"
            echo "$hits" | sed 's/^/    /'
            echo
            VIOLATIONS=$((VIOLATIONS + 1))
        fi
    done
}

# Per ADR-0002 §HC-2:
#   src/tasks/   must NOT import src/sync_bridge, src/tc_sync/, src/devices/
#                (tasks → views IS allowed via the published views::resolve_view
#                entry point — see §HC-2 § "tasks MAY import views")
#   src/views/   must NOT import src/tasks/, src/tc_sync/
#   src/tc_sync/ must NOT import src/tasks/, src/views/
#   src/devices/ must NOT import src/tc_sync/, src/sync_bridge
check "src/tasks"      "src/tasks/"   sync_bridge tc_sync devices
check "src/views"      "src/views/"   tasks tc_sync
check "src/tc_sync"    "src/tc_sync/" tasks views
check "src/devices"    "src/devices/" tc_sync sync_bridge

# Submodule-reach check: the published views::resolve_view entry point is
# the only sanctioned tasks → views path. tasks may not reach into
# views::defaults or other internal submodules.
SUBMODULE_HITS=$(rg -n --type rust 'crate::views::(defaults|handlers)::' src/tasks 2>/dev/null \
    | grep -vE '^\s*[^:]+:\s*[0-9]+:\s*//' \
    || true)
if [[ -n "$SUBMODULE_HITS" ]]; then
    echo "HC-2 violation: src/tasks/ must reach views via the published views::resolve_view entry point only — not internal submodules"
    echo "$SUBMODULE_HITS" | sed 's/^/    /'
    echo
    VIOLATIONS=$((VIOLATIONS + 1))
fi

if [[ $VIOLATIONS -gt 0 ]]; then
    echo "HC-2 boundary check FAILED ($VIOLATIONS rule(s) violated)"
    echo "See ADR-0002 §HC-2 (docs/adr/ADR-0002-design-simplicity-principles.md)"
    exit 1
fi

echo "HC-2 boundaries OK (no cross-module violations)"
