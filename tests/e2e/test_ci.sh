#!/usr/bin/env bash
# test_ci.sh - E2E wrapper for `am ci` native gate runner test suite (br-271i)
#
# Delegates to scripts/e2e_ci.sh following the standard E2E pattern.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${SCRIPT_DIR}/../../scripts/e2e_ci.sh" "$@"
