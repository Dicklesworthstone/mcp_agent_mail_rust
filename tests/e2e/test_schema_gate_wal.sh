#!/usr/bin/env bash
# test_schema_gate_wal.sh — E2E for Track G (schema gate, migrations, WAL/sidecar).
# @tags: reliability, track-g, schema-gate, wal
#
# Asserts, against a REAL built `am` binary and isolated on-disk fixtures:
#   G1 — the startup schema gate refuses a FUTURE user_version with an
#        "upgrade binary" message naming the offending version (observed in the
#        `am serve-http` startup log; the listener then bind-degrades).
#   G2 — `am migrate` applies the FULL migration set, incl. the messages
#        recipients_json column and the message_recipients table.
#   G3 — after migration the DB is in journal_mode=WAL at the compiled
#        SCHEMA_VERSION (user_version=1).
#   G4 — the WAL/SHM sidecar-drift detector flags a header-only / asymmetric
#        sidecar and stays silent on a clean DB (no false positive).
#   G4(live-writer checkpoint refusal) + G5(reconstruct preserves IDs) have no
#        cheap black-box surface and are explicit, logged SKIPs (unit-covered).
#
# Ref: br-bvq1x.14.9 (N9). Depends on G1/G3/G4/G5 + L1 (all closed).

set -uo pipefail

E2E_SUITE="schema_gate_wal"

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
e2e_banner "Track G — Schema Gate, Migrations & WAL/Sidecar E2E"

EVENTS="${E2E_ARTIFACT_DIR}/events.jsonl"
: >"${EVENTS}"

# sqlite3 is REQUIRED here: every G-scenario builds an on-disk fixture from the
# L1 corruption-corpus SQL recipes.
for cmd in jq git sqlite3; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_skip "${cmd} required"
        log_summary "${E2E_SUITE}" 0 0 0 "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_schema_gate_wal.sh" >>"${EVENTS}"
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

RECIPE_DIR="${PROJECT_ROOT}/tests/fixtures/corruption_corpus/recipes"
if [ ! -f "${RECIPE_DIR}/minimal_mailbox_schema.sql" ]; then
    e2e_skip "L1 corruption-corpus recipes not found at ${RECIPE_DIR}"
    e2e_summary
    exit 0
fi

# WORK lives OUTSIDE the artifact dir (binary SQLite DBs must not be bundled).
WORK="$(e2e_mktemp schema_gate_wal_work)"
ART="${E2E_ARTIFACT_DIR}/scenarios"
mkdir -p "${WORK}/storage" "${WORK}/home" "${ART}"
export AM_INTERFACE_MODE="cli"
export STORAGE_ROOT="${WORK}/storage"
export HOME="${WORK}/home"

AM_CMD_TIMEOUT="${AM_CMD_TIMEOUT:-60}"
# Run `am ...` with an isolated DATABASE_URL (arg 2), bounded; capture stdout/err/exit.
amrun_db() {
    local out="$1" dburl="$2"
    shift 2
    if command -v timeout >/dev/null 2>&1; then
        DATABASE_URL="${dburl}" timeout "${AM_CMD_TIMEOUT}" "${AM_BIN}" "$@" >"${out}" 2>"${out}.err"
    else
        DATABASE_URL="${dburl}" "${AM_BIN}" "$@" >"${out}" 2>"${out}.err"
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

# Assert a sqlite3 scalar query equals an expected value.
sqlite_eq() {
    local label="$1" db="$2" query="$3" expected="$4"
    local got
    got="$(sqlite3 "${db}" "${query}" 2>/dev/null)"
    if [ "${got}" = "${expected}" ]; then
        e2e_pass "${label}"
    else
        e2e_fail "${label} (got '${got}', expected '${expected}')"
    fi
}

# ---------------------------------------------------------------------------
# G2 / G3 — `am migrate` applies the full set and lands in WAL
# ---------------------------------------------------------------------------
e2e_case_banner "G2: am migrate applies the full migration set (recipients_json + message_recipients)"
FRESH_DB="${WORK}/fresh.sqlite3"
amrun_db "${ART}/migrate.out" "sqlite:///${FRESH_DB}" migrate
MIG_RC="$(cat "${ART}/migrate.out.exit" 2>/dev/null || echo 1)"
if [ "${MIG_RC}" = "0" ]; then
    e2e_pass "am migrate succeeded on a fresh database"
else
    e2e_fail "am migrate failed (rc=${MIG_RC})"
fi
sqlite_eq "messages.recipients_json column was added" "${FRESH_DB}" \
    "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name='recipients_json';" "1"
sqlite_eq "message_recipients table exists" "${FRESH_DB}" \
    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='message_recipients';" "1"

e2e_case_banner "G3: post-migration DB is journal_mode=WAL at the compiled schema version"
sqlite_eq "journal_mode is WAL after migration" "${FRESH_DB}" "PRAGMA journal_mode;" "wal"
sqlite_eq "user_version equals the compiled SCHEMA_VERSION (1)" "${FRESH_DB}" "PRAGMA user_version;" "1"

# ---------------------------------------------------------------------------
# G1 — schema gate refuses a future user_version
# ---------------------------------------------------------------------------
e2e_case_banner "G1: schema gate refuses a FUTURE user_version with 'upgrade binary'"
FUT_DB="${WORK}/future_schema.sqlite3"
sqlite3 "${FUT_DB}" <"${RECIPE_DIR}/minimal_mailbox_schema.sql"
sqlite3 "${FUT_DB}" "PRAGMA user_version=99;"
SERVE_LOG="${ART}/serve_gate.log"
SERVE_PORT="${AM_E2E_SERVE_PORT:-8793}"
: >"${SERVE_LOG}"
# The gate fires during pool warmup (before/at bind); the listener then
# bind-degrades rather than wedging (br-5mnkl), so the refusal lands in the log
# within ~1s. Run the server in the FOREGROUND under `timeout` (no backgrounding
# / no manual kill — a backgrounded process-group kill can take out the test
# shell itself); `timeout` reaps `am` after the window and `|| true` absorbs the
# resulting 124.
DATABASE_URL="sqlite:///${FUT_DB}" timeout 8 "${AM_BIN}" serve-http \
    --no-tui --no-auth --host 127.0.0.1 --port "${SERVE_PORT}" </dev/null >"${SERVE_LOG}" 2>&1 || true
if grep -q "upgrade binary" "${SERVE_LOG}" 2>/dev/null; then
    e2e_pass "serve-http startup refuses the future-schema DB with 'upgrade binary'"
else
    e2e_fail "serve-http startup did not surface the future-schema gate refusal"
    printf '      log tail: %s\n' "$(tail -3 "${SERVE_LOG}" 2>/dev/null | head -c 320)"
fi
if grep -q "user_version=99" "${SERVE_LOG}" 2>/dev/null; then
    e2e_pass "gate names the offending on-disk user_version"
else
    e2e_fail "gate did not name the offending user_version"
fi

# ---------------------------------------------------------------------------
# G4 — WAL/SHM sidecar-drift classification
# ---------------------------------------------------------------------------
FM_DRIFT="fm-db-state-files-wal-shm-sidecar-drift"

e2e_case_banner "G4a: a header-only / asymmetric WAL sidecar is flagged as drift"
DRIFT_DB="${WORK}/drift_main.sqlite3"
sqlite3 "${DRIFT_DB}" <"${RECIPE_DIR}/minimal_mailbox_schema.sql"
# A 16-byte WAL (<= 32-byte header) with no matching -shm: header-only AND
# asymmetric. SQLite WAL magic prefix keeps it realistic.
printf '\x37\x7f\x06\x82%012d' 0 >"${DRIFT_DB}-wal"
amrun_db "${ART}/drift_dirty.json" "sqlite:///${DRIFT_DB}" \
    doctor fix --only "${FM_DRIFT}" --list --json
check "sidecar-drift detector flags the bad WAL sidecar" \
    "${ART}/drift_dirty.json" '.findings_count >= 1'
check "the finding is the sidecar-drift FM" \
    "${ART}/drift_dirty.json" '.fm_id == "fm-db-state-files-wal-shm-sidecar-drift"'

e2e_case_banner "G4b: a clean DB (no sidecars) yields no false positive"
CLEAN_DB="${WORK}/clean_main.sqlite3"
sqlite3 "${CLEAN_DB}" <"${RECIPE_DIR}/minimal_mailbox_schema.sql"
amrun_db "${ART}/drift_clean.json" "sqlite:///${CLEAN_DB}" \
    doctor fix --only "${FM_DRIFT}" --list --json
check "no sidecar-drift finding on a clean DB" \
    "${ART}/drift_clean.json" '.findings_count == 0'

# ---------------------------------------------------------------------------
# Out-of-band (no cheap black-box surface) — explicit logged SKIPs
# ---------------------------------------------------------------------------
e2e_case_banner "G4(checkpoint) / G5(reconstruct): no cheap black-box surface"
e2e_skip "G4 safe-checkpoint refusal under a LIVE writer needs a held writer process; covered by mcp_agent_mail_db::wal_classify unit tests (safe_checkpoint refuses while a live writer owns the DB)"
e2e_skip "G5 reconstruct canonical-ID preservation needs a seeded git archive; covered by mcp_agent_mail_db::reconstruct unit test reconstruct_preserves_nontrivial_canonical_message_id"

# ---------------------------------------------------------------------------
log_summary "${E2E_SUITE}" "$((_E2E_PASS + _E2E_FAIL + _E2E_SKIP))" "${_E2E_PASS}" "${_E2E_FAIL}" \
    "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_schema_gate_wal.sh" >>"${EVENTS}"
e2e_summary
