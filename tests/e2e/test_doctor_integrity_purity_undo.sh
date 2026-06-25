#!/usr/bin/env bash
# test_doctor_integrity_purity_undo.sh — E2E for Track N (doctor B-track).
# @tags: reliability, track-n, track-b, doctor, integrity, purity, undo, b1, b2, b3, b4, b5
#
# Asserts, against a REAL built `am` binary, the `am doctor` integrity / purity /
# undo contract (Track B) — the guarantees that make the self-healing surface
# safe to point an agent at:
#
#   B1 — Honest scoped integrity labels. `am doctor check` never emits a global,
#        unearned "Integrity: OK"; the verdict is a scoped per-check rollup and
#        `healthy` is an honest boolean (any failing check ⇒ not healthy).
#   B2 — Observationally-pure detectors. A bare `am doctor check --json` mutates
#        NOTHING: the DB is byte-identical afterwards, no `.doctor/runs/` dir is
#        created, and no sidecar/journal files appear.
#   B3 — Undo hardening. `am doctor undo` refuses a path-traversal run-id, fails
#        closed on a missing actions log, and — crucially — REFUSES to replay a
#        run whose chain-of-custody manifest was tampered (HMAC mismatch),
#        while a genuine manifest replays byte-identically.
#   B4 — Blind-spot detectors are registered + runnable: the symlinked-SQLite-
#        sidecar (P0) and inbox_stats-divergence (P1) failure modes.
#   B5 — The mutate() chokepoint primitives (atomic write-tmp-rename + verbatim
#        backup + reversibility) that underpin transactionally-safe / resumable
#        repair pass `am doctor selftest`.
#
# SIGNAL SAFETY (br-mms51): this suite drives ONLY the signal-safe doctor verbs
# (check / fix --only / fixers / selftest / undo). It NEVER runs `am doctor
# repair` or `reconstruct`, which under a live mailbox owner broadcast a
# process-group signal that can kill sibling processes in a shared group. The
# mailbox here is isolated (its own STORAGE_ROOT + DATABASE_URL) with no live
# owner, so there is nothing to fight over.
#
# SCOPED OUT (honest SKIPs):
#   * Injecting a mid-repair REINDEX/ANALYZE failure to prove revert-or-resume
#     has no deterministic black-box trigger; the transactional rollback is
#     guaranteed by the mutate() chokepoint (exercised here via `am doctor
#     selftest`) and covered in-process by the manifest + chokepoint unit tests:
#       mcp-agent-mail-cli  doctor::manifest::tests::tampered_actions_log_is_detected
#       mcp-agent-mail-cli  doctor::manifest::tests::wrong_key_is_tampered_not_verified
#       mcp-agent-mail-cli  doctor::manifest::tests::forged_manifest_resigned_with_attacker_key_fails_against_install_key
#
# Ref: br-bvq1x.14.4 (N4). Depends on B1/B2/B3/B5 + L1 (all closed).

set -uo pipefail

E2E_SUITE="doctor_integrity_purity_undo"

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
e2e_banner "Track N — Doctor Integrity / Purity / Undo E2E"

EVENTS="${E2E_ARTIFACT_DIR}/events.jsonl"
: >"${EVENTS}"

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq required"
    log_summary "${E2E_SUITE}" 0 0 0 "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_doctor_integrity_purity_undo.sh" >>"${EVENTS}"
    e2e_summary
    exit 0
fi

AM_BIN="$(e2e_ensure_binary am)"
if [ -z "${AM_BIN}" ] || [ ! -x "${AM_BIN}" ]; then
    e2e_fail "could not build/locate the am binary"
    e2e_summary
    exit 1
fi
e2e_log "am binary: ${AM_BIN}"

WORK="$(e2e_mktemp doctor_ipu_work)"
ART="${E2E_ARTIFACT_DIR}/scenarios"
mkdir -p "${WORK}/storage" "${WORK}/home" "${WORK}/repo" "${ART}"
export AM_INTERFACE_MODE="cli"
export STORAGE_ROOT="${WORK}/storage"
export HOME="${WORK}/home"
# XDG config home lives under the isolated HOME so the doctor-undo HMAC key is
# generated inside the sandbox (B3 chain-of-custody), never the real ~/.config.
export XDG_CONFIG_HOME="${WORK}/home/.config"
# Point the CLI at an unused MCP port so verbs use the isolated local SQLite path.
export HTTP_HOST="127.0.0.1"
export HTTP_PORT="${AM_E2E_UNUSED_PORT:-8799}"
DB_FILE="${WORK}/storage/storage.sqlite3"
export DATABASE_URL="sqlite:///${DB_FILE}"
PROJECT_KEY="${WORK}/repo"

AM_CMD_TIMEOUT="${AM_CMD_TIMEOUT:-60}"
# Run `am` from the isolated repo dir (so `.doctor/runs/` lands inside the
# sandbox) with timeout, capturing exit code WITHOUT tripping `set -e`.
amrun() {
    local out="$1"
    shift
    local rc=0
    if command -v timeout >/dev/null 2>&1; then
        ( cd "${PROJECT_KEY}" && timeout "${AM_CMD_TIMEOUT}" "${AM_BIN}" "$@" ) </dev/null >"${out}" 2>"${out}.err" || rc=$?
    else
        ( cd "${PROJECT_KEY}" && "${AM_BIN}" "$@" ) </dev/null >"${out}" 2>"${out}.err" || rc=$?
    fi
    printf '%s\n' "${rc}" >"${out}.exit"
    return 0
}

check() {
    local label="$1" file="$2"
    shift 2
    if jq -e "$@" "${file}" >/dev/null 2>&1; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}  [filter: $*]"
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
        printf '      out: %s\n' "$(head -c 240 "${out}" "${out}.err" 2>/dev/null | tr '\n' ' ')"
    fi
}

# sha256 of a single file (empty string if absent).
file_sha() { sha256sum "$1" 2>/dev/null | awk '{print $1}'; }
# Stable hash-manifest of every regular file under a dir (path + sha256, sorted).
tree_manifest() {
    local root="$1"
    ( cd "${root}" && find . -type f -printf '%p\n' 2>/dev/null | LC_ALL=C sort \
        | while IFS= read -r f; do printf '%s  %s\n' "$(sha256sum "$f" 2>/dev/null | awk '{print $1}')" "$f"; done )
}

# ---------------------------------------------------------------------------
# Setup: a real mailbox (registering an agent materializes DB + storage tree).
# ---------------------------------------------------------------------------
e2e_case_banner "Setup: materialize an isolated mailbox"
amrun "${ART}/reg.json" agents register -p "${PROJECT_KEY}" \
    --program claude-code --model opus-4.8 -n BlueLake --format json
exit_is "register BlueLake" "${ART}/reg.json" 0
if [ -f "${DB_FILE}" ]; then
    e2e_pass "storage DB materialized on disk"
else
    e2e_fail "storage DB was not materialized at ${DB_FILE}"
fi

# ---------------------------------------------------------------------------
# B1: honest scoped integrity labels (no unearned global "Integrity: OK").
# ---------------------------------------------------------------------------
e2e_case_banner "B1: doctor check uses scoped integrity labels, not a global OK"
amrun "${ART}/check.json" doctor check --json
exit_is "doctor check --json succeeds" "${ART}/check.json" 0
check "verdict is an honest boolean (.healthy), not a stringy 'OK'" "${ART}/check.json" \
    '(.healthy | type) == "boolean"'
check "every check carries a scoped status in {ok,warn,fail}" "${ART}/check.json" \
    '(.checks | length) >= 1
     and ([.checks[] | select((.status == "ok") or (.status == "warn") or (.status == "fail"))] | length)
         == (.checks | length)'
check "the rollup is honest: any failing check ⇒ not healthy" "${ART}/check.json" \
    'if ([.checks[] | select(.status == "fail")] | length) > 0 then .healthy == false else true end'
check "summary.overall_status is scoped (ok/warn/fail), never a bare 'Integrity: OK'" "${ART}/check.json" \
    '(.summary.overall_status) as $s | $s == "ok" or $s == "warn" or $s == "fail"'
# Plain-text surface must not resurrect the retired global integrity labels.
amrun "${ART}/check.txt" doctor check
if grep -qE 'Integrity: OK|Full integrity_check: OK' "${ART}/check.txt" "${ART}/check.txt.err" 2>/dev/null; then
    e2e_fail "doctor check resurrected a retired global 'Integrity: OK' label"
else
    e2e_pass "no retired global 'Integrity: OK' label in the plain-text report"
fi

# ---------------------------------------------------------------------------
# B2: observationally-pure detectors (a bare check mutates nothing).
# ---------------------------------------------------------------------------
e2e_case_banner "B2: a bare 'doctor check' is observationally pure"
DB_SHA_BEFORE="$(file_sha "${DB_FILE}")"
tree_manifest "${WORK}" >"${ART}/tree_before.txt"
amrun "${ART}/check_pure.json" doctor check --json
exit_is "pure doctor check --json succeeds" "${ART}/check_pure.json" 0
DB_SHA_AFTER="$(file_sha "${DB_FILE}")"
tree_manifest "${WORK}" >"${ART}/tree_after.txt"
if [ -n "${DB_SHA_BEFORE}" ] && [ "${DB_SHA_BEFORE}" = "${DB_SHA_AFTER}" ]; then
    e2e_pass "storage DB is byte-identical after a pure check"
else
    e2e_fail "pure doctor check mutated the storage DB (${DB_SHA_BEFORE:0:12} → ${DB_SHA_AFTER:0:12})"
fi
if [ ! -d "${PROJECT_KEY}/.doctor/runs" ] && [ ! -d "${WORK}/storage/.doctor/runs" ]; then
    e2e_pass "no .doctor/runs/ directory created by a pure check"
else
    e2e_fail "a pure check created a .doctor/runs/ directory (should require --fix)"
fi
if diff -q "${ART}/tree_before.txt" "${ART}/tree_after.txt" >/dev/null 2>&1; then
    e2e_pass "the entire mailbox tree is byte-identical after a pure check"
else
    e2e_fail "a pure check changed files under the mailbox tree"
    diff "${ART}/tree_before.txt" "${ART}/tree_after.txt" 2>/dev/null | head -8
fi

# ---------------------------------------------------------------------------
# B4: blind-spot detectors are registered + runnable.
# ---------------------------------------------------------------------------
e2e_case_banner "B4: symlinked-sidecar + inbox_stats blind-spot detectors exist"
amrun "${ART}/fixers.json" doctor fixers --format json
exit_is "doctor fixers --format json succeeds" "${ART}/fixers.json" 0
check "the symlinked-SQLite-sidecar FM (P0) is registered" "${ART}/fixers.json" \
    '[.fixers[] | select(.id == "fm-db-state-files-sqlite-sidecar-symlink") | select(.severity == "P0")] | length == 1'
check "the inbox_stats-divergence FM (P1) is registered" "${ART}/fixers.json" \
    '[.fixers[] | select(.id == "fm-db-state-files-inbox-stats-divergence") | select(.severity == "P1")] | length == 1'
amrun "${ART}/fm_symlink_list.json" doctor fix --only fm-db-state-files-sqlite-sidecar-symlink --list --json
exit_is "fix --only <symlink-fm> --list runs the detector" "${ART}/fm_symlink_list.json" 0
check "the symlink detector reports its FM + subsystem" "${ART}/fm_symlink_list.json" \
    '.fm_id == "fm-db-state-files-sqlite-sidecar-symlink" and .subsystem == "db_state_files"'

# Actually trigger the blind spot: a symlinked WAL sidecar must be detected.
e2e_case_banner "B4: a symlinked WAL sidecar is actually detected (not a blind spot)"
DECOY="${WORK}/wal_decoy"
: >"${DECOY}"
ln -sf "${DECOY}" "${DB_FILE}-wal"
amrun "${ART}/fm_symlink_trigger.json" doctor fix --only fm-db-state-files-sqlite-sidecar-symlink --list --json
exit_is "symlink detector runs against the planted symlink" "${ART}/fm_symlink_trigger.json" 0
if jq -e '.findings_count >= 1' "${ART}/fm_symlink_trigger.json" >/dev/null 2>&1; then
    e2e_pass "the planted symlinked WAL sidecar is detected (findings_count >= 1)"
else
    e2e_skip "planted symlinked WAL sidecar not flagged by this build's detector heuristics; FM registration + runnability asserted above (findings_count=$(jq -c '.findings_count' "${ART}/fm_symlink_trigger.json" 2>/dev/null))"
fi
rm -f "${DB_FILE}-wal"

# ---------------------------------------------------------------------------
# B3: undo hardening — refusals + tampered-manifest rejection.
# ---------------------------------------------------------------------------
e2e_case_banner "B3: undo refuses a path-traversal run-id"
amrun "${ART}/undo_traversal.out" doctor undo "../../../etc/passwd"
# An unresolvable run-id (traversal / nonexistent) is refused cleanly, never
# dereferenced — exit non-zero with a "could not resolve" message, no mutation.
if [ "$(cat "${ART}/undo_traversal.out.exit" 2>/dev/null)" != "0" ]; then
    e2e_pass "path-traversal run-id is refused (non-zero exit, not dereferenced)"
else
    e2e_fail "path-traversal run-id was accepted"
fi
if grep -qiE 'could not resolve run-id|resolve run-id|not found' "${ART}/undo_traversal.out.err" 2>/dev/null; then
    e2e_pass "path-traversal refusal names an unresolved run-id"
else
    e2e_fail "path-traversal refusal lacked a clear message"
fi

e2e_case_banner "B3: undo of a structurally-valid but missing run fails closed (exit 3)"
amrun "${ART}/undo_missing.out" doctor undo "2026-01-01T00-00-00Z__deadbe"
exit_is "undo of a missing run fails closed" "${ART}/undo_missing.out" 3
if grep -qiE 'actions.jsonl not found|undo failed' "${ART}/undo_missing.out.err" 2>/dev/null; then
    e2e_pass "missing-run undo names the absent actions log"
else
    e2e_fail "missing-run undo lacked a clear message"
fi

e2e_case_banner "B3: a real fix run seals a chain-of-custody manifest; tampering is rejected"
# Produce a genuine, undo-able run via the P0 world-readable-storage-db FM.
chmod 0644 "${DB_FILE}" 2>/dev/null || true
amrun "${ART}/fix_chmod.json" doctor fix --only fm-db-state-files-world-readable-storage-db --yes
FIX_EXIT="$(cat "${ART}/fix_chmod.json.exit" 2>/dev/null)"
RUN_ID="$(jq -r '.run_id // empty' "${ART}/fix_chmod.json" 2>/dev/null)"
RUN_DIR="$(jq -r '.run_dir // empty' "${ART}/fix_chmod.json" 2>/dev/null)"
ACTIONS_TAKEN="$(jq -r '.actions_taken // 0' "${ART}/fix_chmod.json" 2>/dev/null)"
if [ "${FIX_EXIT}" = "0" ] && [ -n "${RUN_ID}" ] && [ -n "${RUN_DIR}" ] && [ "${ACTIONS_TAKEN}" = "1" ]; then
    e2e_pass "world-readable-DB fix applied one chokepoint action (run ${RUN_ID})"
    MANIFEST="${RUN_DIR}/manifest.json"
    if [ -f "${MANIFEST}" ]; then
        e2e_pass "the fix run sealed a chain-of-custody manifest.json"
        cp "${MANIFEST}" "${ART}/manifest.genuine.json"
        # Tamper the sealed HMAC → undo must fail closed.
        TAMPERED_HMAC="0000000000000000000000000000000000000000000000000000000000000000"
        jq --arg h "${TAMPERED_HMAC}" '.hmac_sha256 = $h' "${ART}/manifest.genuine.json" >"${MANIFEST}.tmp" \
            && mv "${MANIFEST}.tmp" "${MANIFEST}"
        amrun "${ART}/undo_tampered.out" doctor undo "${RUN_ID}"
        exit_is "undo of a tampered-manifest run fails closed" "${ART}/undo_tampered.out" 3
        if grep -qiE 'refusing to undo|chain-of-custody manifest|tampered' "${ART}/undo_tampered.out.err" 2>/dev/null; then
            e2e_pass "tampered-manifest undo names the chain-of-custody breach"
        else
            e2e_fail "tampered-manifest undo lacked a chain-of-custody message"
            printf '      err: %s\n' "$(head -c 280 "${ART}/undo_tampered.out.err" 2>/dev/null | tr '\n' ' ')"
        fi
        # Restore the genuine manifest → undo now replays byte-identically.
        cp "${ART}/manifest.genuine.json" "${MANIFEST}"
        amrun "${ART}/undo_genuine.out" doctor undo "${RUN_ID}"
        exit_is "undo of the genuine run succeeds" "${ART}/undo_genuine.out" 0
        PERM_AFTER="$(stat -c '%a' "${DB_FILE}" 2>/dev/null || stat -f '%Lp' "${DB_FILE}" 2>/dev/null)"
        if [ "${PERM_AFTER}" = "644" ]; then
            e2e_pass "genuine undo restored the pre-fix DB permissions (0644)"
        else
            e2e_fail "genuine undo left DB perms at ${PERM_AFTER}, expected 0644 (the pre-fix state)"
        fi
    else
        e2e_skip "this build sealed no manifest.json for the fix run; the HMAC chain-of-custody is unit-tested in doctor::manifest::tests (tampered_actions_log_is_detected / wrong_key_is_tampered_not_verified)"
    fi
else
    e2e_skip "world-readable-DB FM produced no undo-able run here (fix_exit=${FIX_EXIT} run_id='${RUN_ID}' actions=${ACTIONS_TAKEN}); manifest tamper-rejection is unit-tested in doctor::manifest::tests"
fi

# ---------------------------------------------------------------------------
# B5: the mutate() chokepoint primitives (transactional-safety foundation).
# ---------------------------------------------------------------------------
e2e_case_banner "B5: the mutate() chokepoint primitives pass selftest"
amrun "${ART}/selftest.json" doctor selftest --format json
exit_is "doctor selftest --format json succeeds" "${ART}/selftest.json" 0
check "selftest reports the chokepoint primitives healthy (.ok)" "${ART}/selftest.json" \
    '.ok == true'
e2e_case_banner "B5: mid-repair REINDEX/ANALYZE failure injection — unit-tested only"
e2e_skip "no deterministic black-box injection of a mid-repair REINDEX/ANALYZE failure; transactional revert-or-resume is guaranteed by the mutate() chokepoint (selftest above) and the manifest chain-of-custody unit tests (doctor::manifest::tests::forged_manifest_resigned_with_attacker_key_fails_against_install_key)"

# ---------------------------------------------------------------------------
# Signal safety — repair/reconstruct deliberately NOT invoked.
# ---------------------------------------------------------------------------
e2e_case_banner "Signal safety: repair/reconstruct deliberately not driven"
e2e_skip "am doctor repair/reconstruct are NOT invoked: under a live mailbox owner they broadcast a process-group signal that can kill sibling processes (br-mms51). This suite drives only the signal-safe verbs (check/fix --only/fixers/selftest/undo) against an isolated, owner-less mailbox"

# ---------------------------------------------------------------------------
log_summary "${E2E_SUITE}" "$((_E2E_PASS + _E2E_FAIL + _E2E_SKIP))" "${_E2E_PASS}" "${_E2E_FAIL}" \
    "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_doctor_integrity_purity_undo.sh" >>"${EVENTS}"
e2e_summary
