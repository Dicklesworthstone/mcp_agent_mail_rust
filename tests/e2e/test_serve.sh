#!/usr/bin/env bash
# test_serve.sh - E2E wrapper for native serve/start enhancements (br-17c93)
#
# Runs:
#   scripts/e2e_serve.sh

set -euo pipefail

export WRAPPER_SUITE="serve"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tests/e2e/test_helpers.sh
source "${SCRIPT_DIR}/test_helpers.sh"
wrapper_exec "${SCRIPT_DIR}/../../scripts/e2e_serve.sh"
