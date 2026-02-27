#!/usr/bin/env bash
# test_tui_full_traversal.sh — E2E wrapper for deterministic full-screen
# traversal repro harness (br-legjy.1.1).
#
# Canonical entrypoint:
#   am e2e run --project . tui_full_traversal
# Direct:
#   bash tests/e2e/test_tui_full_traversal.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Safety: default to keeping temp dirs so shared harness cleanup doesn't run `rm -rf`.
: "${AM_E2E_KEEP_TMP:=1}"

bash "${SCRIPT_DIR}/../../scripts/e2e_tui_full_traversal.sh"
