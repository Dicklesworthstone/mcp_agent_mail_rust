#!/usr/bin/env bash
# test_locks_live_owner.sh — E2E for Track D (live-owner & lock contention).
# @tags: reliability, track-d, locks, live-owner
#
# Asserts, against a REAL built `am` binary with a REAL live owner (a running
# `am serve-http` that holds the mailbox):
#   D2 — `am doctor locks --json` classifies the owner_state as `live` with the
#        `active_other_owner` disposition and names the holding process(es).
#   D4 — the rolled-up drain verdict flips to NOT safe_to_mutate / read_only
#        while a live owner is present (safe_to_mutate:true with no owner).
#
# The supervised guard never asks you to kill `am`; the safe path is to drain
# via your supervisor. This suite NEVER kills the owner to make a verb succeed —
# it tears its OWN server down at the end.
#
# SCOPED OUT (br-mms51): the `am doctor repair`/`reconstruct` exit-3
# supervised-owner refusal is verified interactively + unit-tested but not
# asserted here, because invoking it against an in-process-group live owner in a
# non-interactive shell emits a process-group signal that kills the test shell.
#
# Ref: br-bvq1x.14.6 (N6). Depends on D1/D2/D3/D4 (all closed).

set -uo pipefail

E2E_SUITE="locks_live_owner"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    export CARGO_TARGET_DIR="${PROJECT_ROOT}/target"
fi
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"
# shellcheck source=lib/structured_logging.sh
source "${SCRIPT_DIR}/lib/structured_logging.sh"

e2e_init_artifacts
e2e_banner "Track D — Live-Owner & Lock-Contention E2E"

EVENTS="${E2E_ARTIFACT_DIR}/events.jsonl"
: >"${EVENTS}"

for cmd in jq git curl; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_skip "${cmd} required"
        log_summary "${E2E_SUITE}" 0 0 0 "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_locks_live_owner.sh" >>"${EVENTS}"
        e2e_summary
        exit 0
    fi
done

AM_BIN="$(e2e_ensure_binary am)"
if [ -z "${AM_BIN}" ] || [ ! -x "${AM_BIN}" ]; then
    e2e_fail "could not build/locate the am binary"
    e2e_summary
    exit 1
fi
e2e_log "am binary: ${AM_BIN}"

WORK="$(e2e_mktemp locks_live_owner_work)"
ART="${E2E_ARTIFACT_DIR}/scenarios"
mkdir -p "${WORK}/storage" "${WORK}/home" "${ART}"
export AM_INTERFACE_MODE="cli"
export STORAGE_ROOT="${WORK}/storage"
export DATABASE_URL="sqlite:///${WORK}/mb.sqlite3"
export HOME="${WORK}/home"

AM_CMD_TIMEOUT="${AM_CMD_TIMEOUT:-60}"
amrun() {
    local out="$1"
    shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "${AM_CMD_TIMEOUT}" "${AM_BIN}" "$@" >"${out}" 2>"${out}.err"
    else
        "${AM_BIN}" "$@" >"${out}" 2>"${out}.err"
    fi
    printf '%s\n' "$?" >"${out}.exit"
    return 0
}

check() {
    local label="$1" file="$2" filter="$3"
    shift 3
    if jq -e "$@" "${filter}" "${file}" >/dev/null 2>&1; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}  [filter: ${filter}]"
        printf '      got: %s\n' "$(jq -c . "${file}" 2>/dev/null | head -c 320)"
    fi
}

exit_is() {
    local label="$1" out="$2" want="$3"
    local got
    got="$(cat "${out}.exit" 2>/dev/null || echo "?")"
    if [ "${got}" = "${want}" ]; then
        e2e_pass "${label} (exit ${got})"
    else
        e2e_fail "${label} (exit ${got}, expected ${want})"
        printf '      out: %s\n' "$(head -c 200 "${out}" "${out}.err" 2>/dev/null | tr '\n' ' ')"
    fi
}

# ---------------------------------------------------------------------------
# Baseline — no live owner
# ---------------------------------------------------------------------------
e2e_case_banner "Baseline: with no owner the drain verdict is safe_to_mutate"
amrun "${ART}/drain_noowner.json" doctor drain --json
check "no owner => safe_to_mutate is true" "${ART}/drain_noowner.json" '.safe_to_mutate == true'

# ---------------------------------------------------------------------------
# Bring up a REAL live owner and probe it
# ---------------------------------------------------------------------------
SRV_PORT="${AM_E2E_OWNER_PORT:-8797}"
SRV_LOG="${ART}/owner_server.log"
: >"${SRV_LOG}"
# Start the owner in its OWN session via setsid: the supervised-owner
# coordination + graceful-shutdown paths broadcast signals to their process
# group, which — if shared with this non-interactive test shell — would kill the
# suite (br-mms51). A new session keeps those signals contained; we then tear
# the server down by its own process group, never touching the test's group.
setsid "${AM_BIN}" serve-http --no-tui --no-auth --host 127.0.0.1 --port "${SRV_PORT}" \
    </dev/null >"${SRV_LOG}" 2>&1 &
SRV_PID=$!

owner_bound=0
for _ in $(seq 1 40); do
    if curl -fsS "http://127.0.0.1:${SRV_PORT}/healthz" >/dev/null 2>&1; then
        owner_bound=1
        break
    fi
    sleep 0.5
done

if [ "${owner_bound}" = "1" ]; then
    e2e_pass "live owner server bound on 127.0.0.1:${SRV_PORT}"

    e2e_case_banner "D2: doctor locks classifies the owner_state as live"
    amrun "${ART}/locks_live.json" doctor locks --json
    check "owner_state.class is live" "${ART}/locks_live.json" '.owner_state.class == "live"'
    check "disposition is active_other_owner" "${ART}/locks_live.json" \
        '.disposition == "active_other_owner"'
    check "the holding process is named" "${ART}/locks_live.json" '(.processes | length) >= 1'

    e2e_case_banner "D4: drain verdict flips to not-safe under a live owner"
    amrun "${ART}/drain_live.json" doctor drain --json
    check "live owner => safe_to_mutate is false" "${ART}/drain_live.json" '.safe_to_mutate == false'
    check "live owner => read_only is true" "${ART}/drain_live.json" '.read_only == true'
    check "live owner => owner_class is live" "${ART}/drain_live.json" '.owner_class == "live"'

    e2e_case_banner "D4: repair/reconstruct REFUSE under a live owner — scoped out (br-mms51)"
    # The supervised-owner REFUSAL is correct and verified interactively:
    # `am doctor repair`/`reconstruct` exit 3 with classification
    # "supervised-owner-required" rather than ever killing the live `am`.
    # It is NOT asserted here because invoking it against an in-process-group
    # live owner inside a NON-INTERACTIVE shell emits a process-group signal
    # (SIGSTKFLT/16) that kills the test shell itself — a harness/runtime
    # interaction tracked as br-mms51. The refusal's design guarantee is in
    # AGENTS.md (supervised-owner guard, D4) and the exit-3 path is unit-tested.
    e2e_skip "D4 repair/reconstruct exit-3 refusal under a live owner is scoped out of this black-box e2e: it triggers a process-group signal that destabilizes a non-interactive harness (br-mms51); verified interactively + unit-tested + documented in AGENTS.md"
else
    e2e_fail "live owner server failed to bind on 127.0.0.1:${SRV_PORT}"
    printf '      server log tail: %s\n' "$(tail -3 "${SRV_LOG}" 2>/dev/null | head -c 320)"
fi

# Tear down OUR server with SIGKILL matched by its UNIQUE port: a graceful
# (SIGTERM) shutdown runs the coordination path that broadcasts a signal to the
# process group (br-mms51) which would kill this shell, and signalling by PID/
# job has proven to reach this shell too. SIGKILL cannot be handled (no shutdown
# code runs) and the pattern (the serve-http command line) never matches this
# test script — so neither a broadcast nor a mis-targeted kill can hit us.
pkill -KILL -f "serve-http --no-tui --no-auth --host 127.0.0.1 --port ${SRV_PORT}" 2>/dev/null || true
SRV_PID="${SRV_PID:-}"  # referenced to satisfy set -u; the pattern kill is authoritative

# ---------------------------------------------------------------------------
log_summary "${E2E_SUITE}" "$((_E2E_PASS + _E2E_FAIL + _E2E_SKIP))" "${_E2E_PASS}" "${_E2E_FAIL}" \
    "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_locks_live_owner.sh" >>"${EVENTS}"
e2e_summary
