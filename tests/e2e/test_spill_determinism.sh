#!/usr/bin/env bash
# test_spill_determinism.sh - E2E test suite wrapper for spill-path determinism (br-1i11.1.6)
#
# Runs the implementation in scripts/e2e_spill_determinism.sh.
# Authoritative invocation:
#   am e2e run --project . spill_determinism
# Compatibility fallback:
#   AM_E2E_FORCE_LEGACY=1 ./scripts/e2e_test.sh spill_determinism

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Safety: default to keeping temp dirs so shared harness cleanup doesn't run `rm -rf`.
: "${AM_E2E_KEEP_TMP:=1}"

bash "${SCRIPT_DIR}/../../scripts/e2e_spill_determinism.sh"
