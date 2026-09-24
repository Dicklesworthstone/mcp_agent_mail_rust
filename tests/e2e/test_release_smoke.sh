#!/usr/bin/env bash
# test_release_smoke.sh - Bounded black-box release smoke (br-kp1in.14).
#
# Drives the exact candidate `am` binary through MCP-over-HTTP in an isolated
# mailbox: coordination flow, cross-project misdelivery negative, 16-client
# storm (zero RESOURCE_BUSY), SIGKILL + archive convergence without client
# reads + independent full integrity_check, and a >= 5 minute mixed soak that
# bounds SQLite descriptors and requires drain progress in every 10 s window,
# a fetch_inbox p99 budget, no EMFILE, zombies, HTTP restarts or archive
# re-roots. An optional control binary (usually the previous release) runs the
# same phases in the same invocation for an A/B receipt; only the candidate
# gates.
#
# Run via:
#   am e2e run --project . release_smoke
#   AM_RELEASE_SMOKE_BIN=/path/to/am \
#   AM_RELEASE_SMOKE_CONTROL_BIN=$HOME/.local/bin/am ./tests/e2e/test_release_smoke.sh
#
# Knobs: AM_RELEASE_SMOKE_SOAK_SECS (default and release minimum 300),
#        AM_RELEASE_SMOKE_CONVERGE_SECS (default and release maximum 300).
# A shorter soak or a looser convergence bound can run for development but
# yields NO_VERDICT, never PASS.

set -euo pipefail

E2E_SUITE="release_smoke"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Bounded black-box release smoke (br-kp1in.14)"

if ! command -v python3 >/dev/null 2>&1 || [ ! -d /proc/self/fd ]; then
    e2e_skip "python3 and Linux /proc are required"
    e2e_summary
    exit 0
fi

CANDIDATE="${AM_RELEASE_SMOKE_BIN:-}"
if [ -z "${CANDIDATE}" ]; then
    CANDIDATE="$(e2e_ensure_binary "am" | tail -n 1)"
fi
CONTROL_ARGS=()
if [ -n "${AM_RELEASE_SMOKE_CONTROL_BIN:-}" ]; then
    CONTROL_ARGS=(--control-bin "${AM_RELEASE_SMOKE_CONTROL_BIN}")
fi
e2e_log "candidate: ${CANDIDATE}"
e2e_log "control:   ${AM_RELEASE_SMOKE_CONTROL_BIN:-<none>}"

set +e
python3 "${SCRIPT_DIR}/lib/release_smoke.py" --bin "${CANDIDATE}" "${CONTROL_ARGS[@]}" \
    --out "${E2E_ARTIFACT_DIR}/release_smoke" 2>&1 | tee "${E2E_ARTIFACT_DIR}/release_smoke.log"
rc=${PIPESTATUS[0]}
set -e

RECEIPT="${E2E_ARTIFACT_DIR}/release_smoke/release_smoke_receipt.json"
if [ ! -f "${RECEIPT}" ]; then
    e2e_fail "release smoke produced no receipt (exit ${rc})"
elif [ "${rc}" -eq 0 ]; then
    e2e_pass "candidate passed every release-smoke phase (receipt: ${RECEIPT})"
else
    e2e_fail "candidate failed release smoke (exit ${rc}; receipt: ${RECEIPT})"
fi

e2e_summary
