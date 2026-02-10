#!/usr/bin/env bash
# test_tui_a11y.sh - E2E test suite wrapper for TUI accessibility (keyboard + contrast).
#
# Runs the implementation in scripts/e2e_tui_a11y.sh so the suite can be invoked via:
#   ./scripts/e2e_test.sh tui_a11y

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Safety: default to keeping temp dirs so shared harness cleanup doesn't run `rm -rf`.
: "${AM_E2E_KEEP_TMP:=1}"

bash "${SCRIPT_DIR}/../../scripts/e2e_tui_a11y.sh"

