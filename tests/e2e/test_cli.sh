#!/usr/bin/env bash
# test_cli.sh - E2E wrapper for CLI stability suite (br-2ei.9.5)
#
# Delegates to scripts/e2e_cli.sh following the same pattern as test_http.sh
# and test_archive.sh.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${SCRIPT_DIR}/../../scripts/e2e_cli.sh" "$@"
