#!/usr/bin/env bash
# test_dual_mode.sh - E2E wrapper for dual-mode suite (br-21gj.5.6)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${SCRIPT_DIR}/../../scripts/e2e_dual_mode.sh" "$@"
