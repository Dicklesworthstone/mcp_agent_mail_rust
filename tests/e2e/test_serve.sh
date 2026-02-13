#!/usr/bin/env bash
# test_serve.sh - E2E wrapper for native serve/start enhancements (br-17c93)
#
# Runs:
#   scripts/e2e_serve.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Safety: default to keeping temp dirs so shared harness cleanup doesn't run rm -rf.
: "${AM_E2E_KEEP_TMP:=1}"

bash "${SCRIPT_DIR}/../../scripts/e2e_serve.sh"

