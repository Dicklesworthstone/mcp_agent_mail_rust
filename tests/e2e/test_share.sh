#!/usr/bin/env bash
# test_share.sh - E2E test suite wrapper for share/export bundle pipeline.
#
# Runs the implementation in scripts/e2e_share.sh so the suite can also
# be invoked manually while keeping the e2e runner discovery contract.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bash "${SCRIPT_DIR}/../../scripts/e2e_share.sh"
