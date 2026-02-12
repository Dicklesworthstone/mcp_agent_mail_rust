#!/usr/bin/env bash
# test_share_verify_live.sh - E2E wrapper for verify-live matrix suite.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${SCRIPT_DIR}/../../scripts/e2e_share_verify_live.sh" "$@"
