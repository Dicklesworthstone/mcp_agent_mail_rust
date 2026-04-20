#!/usr/bin/env bash
# br-8ujfs.3.1 — audit count script
# Counts in-process `Command::new("git")` shell-outs across the workspace,
# broken down by crate and by classification (production vs test).
#
# Used as a baseline snapshot for Track D's regression guard (D2).
# The baseline values live in docs/GIT_SHELLOUT_AUDIT_COUNT.txt;
# drift must be explained by a corresponding audit doc update.

set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v rg >/dev/null 2>&1; then
    echo "rg (ripgrep) not found on PATH" >&2
    exit 2
fi

echo "# Git shell-out count — $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo

total=$(rg -c 'Command::new\("git"\)' crates/ 2>/dev/null \
    | awk -F: '{s+=$2} END {print s+0}')
echo "Total occurrences: $total"
echo

echo "## Per-crate breakdown"
rg -c 'Command::new\("git"\)' crates/ 2>/dev/null | sort

echo
echo "## Production vs test (approximate; mod tests boundaries parsed separately for full accuracy)"
prod=$(rg -c 'Command::new\("git"\)' \
    --glob '!**/tests/**' \
    --glob '!**/benches/**' \
    --glob '!**/*_test.rs' \
    crates/ 2>/dev/null \
    | awk -F: '{s+=$2} END {print s+0}')

test_count=$(rg -c 'Command::new\("git"\)' \
    crates/*/tests/ 2>/dev/null \
    | awk -F: '{s+=$2} END {print s+0}')

echo "Production src/ + top-level: $prod"
echo "Dedicated tests/: $test_count"
echo "(in-source #[cfg(test)] blocks counted in production — see GIT_SHELLOUT_AUDIT.md for exact classification)"
