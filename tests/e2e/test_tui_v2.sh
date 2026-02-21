#!/usr/bin/env bash
# test_tui_v2.sh - E2E test suite wrapper for TUI V2 features (br-2bbt.11.2)
#
# Runs the implementation in scripts/e2e_tui_v2.sh.
# Authoritative invocation:
#   am e2e run --project . tui_v2
# Compatibility fallback:
#   AM_E2E_FORCE_LEGACY=1 ./scripts/e2e_test.sh tui_v2

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Safety: default to keeping temp dirs so shared harness cleanup doesn't run `rm -rf`.
: "${AM_E2E_KEEP_TMP:=1}"

bash "${SCRIPT_DIR}/../../scripts/e2e_tui_v2.sh"
