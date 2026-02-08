#!/usr/bin/env bash
# test_mode_matrix.sh - E2E wrapper for mode matrix suite (br-21gj.5.2)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${SCRIPT_DIR}/../../scripts/e2e_mode_matrix.sh" "$@"
