#!/usr/bin/env bash
# test_host_pressure_drift.sh — E2E for Track J (Host-Pressure & Version/Path-Drift).
# @tags: reliability, track-j, drift, host-pressure
#
# Asserts, against a REAL built `am` binary and an isolated STORAGE_ROOT/DB:
#   J1 — `am robot health --include-host` reports the host-pressure section
#        (disk/inode/load/mem/WAL/writer-PID) and says "host pressure likely"
#        ONLY on threshold evidence (forced via AM_HOST_* overrides).
#   J2 — a synthetic known-bad binary version triggers the OFFLINE drift warning
#        (bundled catalog + AM_EXTRA_KNOWN_BAD_AM_JSON override) carrying
#        path/version/repair-command; a healthy build self-classifies "current".
#   J3 — `am doctor check`/`am robot health` always print the effective runtime
#        identity (binary/version/pid/storage_root/database_url/host:port/pids).
#   J4 — archive reconstruction succeeds under a simulated Apple symlinked
#        TMPDIR with NO `pwd -P` wrapper, while still blocking unsafe paths.
#        The firmlink-accept half is macOS-specific (the `/var`->`/private/var`
#        firmlink cannot be fabricated on Linux); on non-Darwin it is an
#        explicit, logged SKIP that points at the covering db unit tests, plus a
#        cross-platform guard-does-not-over-reject assertion.
#
# Ref: br-bvq1x.14.12 (N12). Depends on J1/J2/J3/J4 (all closed).

set -uo pipefail

E2E_SUITE="host_pressure_drift"

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
e2e_banner "Track J — Host-Pressure & Version/Path-Drift E2E"

EVENTS="${E2E_ARTIFACT_DIR}/events.jsonl"
: >"${EVENTS}"

for cmd in jq git; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_skip "${cmd} required"
        log_summary "${E2E_SUITE}" 0 0 0 "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_host_pressure_drift.sh" >>"${EVENTS}"
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

# Isolated runtime: a throwaway STORAGE_ROOT + DB so the suite never touches a
# real mailbox. CLI surface (robot/doctor are CLI-mode commands).
#
# WORK lives OUTSIDE the artifact dir on purpose: the runtime SQLite DB +
# storage tree must not land under E2E_ARTIFACT_DIR, or the harness's recursive
# artifact-bundling would scan/hash the binary DB (a pure-bash spin). Only the
# small scenario JSONs belong in the artifact dir.
WORK="$(e2e_mktemp host_pressure_drift_work)"
ART="${E2E_ARTIFACT_DIR}/scenarios"
mkdir -p "${WORK}/storage" "${WORK}/home" "${ART}"
export AM_INTERFACE_MODE="cli"
export STORAGE_ROOT="${WORK}/storage"
export DATABASE_URL="sqlite:///${WORK}/mb.sqlite3"
export HOME="${WORK}/home"

# Run `am ...`, capturing stdout/stderr/exit under the isolated env. Never aborts
# the suite (am may legitimately exit non-zero when it reports findings) and is
# bounded by a timeout so a single wedged command can never hang the suite — a
# timeout surfaces as an empty JSON file that fails the scenario's jq checks
# loudly rather than blocking CI.
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

# Assert a jq filter evaluates truthy (jq -e). Extra args (e.g. --arg) go before
# the filter. On failure, dumps the filter + a snippet of the captured JSON.
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

# ---------------------------------------------------------------------------
# J1 — host-pressure health section
# ---------------------------------------------------------------------------
e2e_case_banner "J1a host-pressure section present (healthy host => no false alarm)"
amrun "${ART}/j1a.json" robot health --include-host --format json
check "host section carries disk/inode/load/mem/db fields" "${ART}/j1a.json" \
    '([.host|keys[]]) as $k | (["cpu_count","disk_free_bytes","disk_free_pct","inodes_free","load_per_cpu","mem_available_pct","db_file_bytes","db_dir_writable","host_pressure_likely","status","reasons"] | all(. as $n | ($k|index($n)) != null))'
check "host_pressure_likely is false on a healthy host (no threshold breach)" \
    "${ART}/j1a.json" '.host.host_pressure_likely == false'

e2e_case_banner "J1b host_pressure_likely fires ONLY on threshold evidence"
AM_HOST_DISK_FREE_CRIT_PCT=99.9 amrun "${ART}/j1b.json" robot health --include-host --format json
check "forced low-disk threshold => host_pressure_likely true" \
    "${ART}/j1b.json" '.host.host_pressure_likely == true'
check "verdict cites a concrete reason" "${ART}/j1b.json" '(.host.reasons | length) > 0'
check "status escalates to critical" "${ART}/j1b.json" '.host.status == "critical"'

# ---------------------------------------------------------------------------
# J2 — offline known-bad / obsolete am-version warning
# ---------------------------------------------------------------------------
e2e_case_banner "J2a healthy build self-classifies as current"
amrun "${ART}/j2a.json" doctor check --json
check "am_version.state == current for the running build" \
    "${ART}/j2a.json" '.runtime_identity.am_version.state == "current"'
check "am_version names the installed version" \
    "${ART}/j2a.json" '(.runtime_identity.am_version.installed | type) == "string"'

RUNNING_VERSION="$(jq -r '.runtime_identity.version' "${ART}/j2a.json" 2>/dev/null)"
e2e_log "running am version: ${RUNNING_VERSION}"

e2e_case_banner "J2b synthetic known-bad version => OFFLINE drift warning"
cat >"${ART}/known_bad_am.json" <<JSON
{ "entries": [ { "code": "SYNTH_E2E_KNOWN_BAD", "match": { "kind": "exact", "version": "${RUNNING_VERSION}" }, "severity": "fail", "summary": "synthetic e2e known-bad", "remediation_ref": "docs/RECOVERY_RUNBOOK.md#synthetic" } ] }
JSON
AM_EXTRA_KNOWN_BAD_AM_JSON="${ART}/known_bad_am.json" amrun "${ART}/j2b.json" doctor check --json
check "known-bad version => state known_bad" \
    "${ART}/j2b.json" '.runtime_identity.am_version.state == "known_bad"'
check "warning carries the EXACT install repair command" \
    "${ART}/j2b.json" '(.runtime_identity.am_version.repair_command // "") | contains("install.sh")'
check "warning carries the binary path" \
    "${ART}/j2b.json" '(.runtime_identity.binary_path | type) == "string"'
check "warning carries the installed version" \
    "${ART}/j2b.json" '.runtime_identity.am_version.installed == $v' --arg v "${RUNNING_VERSION}"
check "classification is bundled/offline (verdict from the local catalog)" \
    "${ART}/j2b.json" '.runtime_identity.am_version.verdict.code == "SYNTH_E2E_KNOWN_BAD"'

# ---------------------------------------------------------------------------
# J3 — path/version-confusion: always name the effective runtime identity
# ---------------------------------------------------------------------------
e2e_case_banner "J3 runtime_identity always names the effective binary/mailbox/server"
# The always-present contract (mirrors doctor_check_json_always_includes_runtime_identity);
# db_file is emitted opportunistically (present in robot health, omitted by the
# read-only doctor check), so database_url is the universal "which mailbox" anchor.
for field in binary_path version pid storage_root database_url http_host http_port server_pids am_version; do
    check "runtime_identity carries ${field}" \
        "${ART}/j2a.json" '.runtime_identity | has($f)' --arg f "${field}"
done
check "database_url names the isolated mailbox DB" \
    "${ART}/j2a.json" '(.runtime_identity.database_url // "") | contains("mb.sqlite3")'
check "server_pids is an array (empty when no server bound)" \
    "${ART}/j2a.json" '(.runtime_identity.server_pids | type) == "array"'

# ---------------------------------------------------------------------------
# J4 — macOS firmlink TMPDIR reconstruction (no shell wrapper)
# ---------------------------------------------------------------------------
e2e_case_banner "J4 sqlite path guard: firmlink temp roots accepted, escapes refused"
# Cross-platform: the standard isolated temp DB path must NOT trip a false
# symlink-escape refusal (the J4 change must not over-tighten common paths).
if grep -qi "traverses symlinked path" "${ART}/j2a.json.err" 2>/dev/null; then
    e2e_fail "path guard falsely refused the standard temp DB path as a symlink-escape"
else
    e2e_pass "path guard accepts the standard temp DB path (no false symlink-escape refusal)"
fi

if [ "$(uname -s)" = "Darwin" ]; then
    # On macOS the per-user TMPDIR lives under /var/folders/..., and /var is the
    # `/private/var` firmlink. Reconstruction must succeed with the system TMPDIR
    # and NO `TMPDIR="$(pwd -P)"` wrapper.
    MAC_TMP="${TMPDIR:-/var/folders}"
    e2e_log "Darwin TMPDIR (firmlink path): ${MAC_TMP}"
    TMPDIR="${MAC_TMP}" amrun "${ART}/j4_mac.json" doctor reconstruct --json
    rc="$(cat "${ART}/j4_mac.json.exit" 2>/dev/null || echo 1)"
    if [ "${rc}" = "0" ] || ! grep -qi "traverses symlinked path" "${ART}/j4_mac.json.err" 2>/dev/null; then
        e2e_pass "reconstruction under the Apple firmlink TMPDIR did not hit a symlink-escape refusal"
    else
        e2e_fail "reconstruction under the Apple firmlink TMPDIR was wrongly refused (rc=${rc})"
    fi
else
    e2e_skip "J4 firmlink-accept is macOS-specific (the /var->/private/var firmlink cannot be fabricated on $(uname -s)); covered by db unit tests is_macos_temp_firmlink + canonical_snapshot_tempdir_resolves_symlinked_tmpdir_for_sqlite_targets"
fi

# ---------------------------------------------------------------------------
log_summary "${E2E_SUITE}" "$((_E2E_PASS + _E2E_FAIL + _E2E_SKIP))" "${_E2E_PASS}" "${_E2E_FAIL}" \
    "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_host_pressure_drift.sh" >>"${EVENTS}"
e2e_summary
