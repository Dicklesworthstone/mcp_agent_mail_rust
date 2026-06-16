#!/usr/bin/env bash
# test_health_and_selftests.sh — E2E for Track C (decomposed health + selftests).
# @tags: reliability, track-c, health, selftest
#
# Asserts, against a REAL built `am` binary and isolated scratch state:
#   C1 — top-level health is NEVER greener than its weakest critical verdict:
#        `am doctor check --json` on a HEALTHY db is `healthy:true`, but on the
#        `missing_required_tables` corruption-corpus fixture it is `healthy:false`
#        with `summary.overall_status:"fail"` and a failing critical check.
#   C2 — the write-path selftest runs against an ISOLATED scratch mailbox and
#        reports the five independent dimensions, isolating the broken one.
#   C3 — the MCP decode selftest negotiates the protocol version and carries a
#        decode-failure (protocol-class) check.
#   C5 — the rolled-up coordination verdict (`am doctor drain --json`) reports
#        safe_to_mutate / read_only / owner_class.
#   chokepoint — `am doctor selftest` verifies the mutate() primitives.
#   C4 — the send_message RefCell re-entrancy fix has NO CLI surface (covered by
#        in-crate unit tests); it is an explicit, logged SKIP here.
#
# Ref: br-bvq1x.14.5 (N5). Depends on C1/C2/C3/C4 + L1/L2 (all closed).

set -uo pipefail

E2E_SUITE="health_and_selftests"

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
e2e_banner "Track C — Decomposed Health & Selftests E2E"

EVENTS="${E2E_ARTIFACT_DIR}/events.jsonl"
: >"${EVENTS}"

for cmd in jq git; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_skip "${cmd} required"
        log_summary "${E2E_SUITE}" 0 0 0 "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_health_and_selftests.sh" >>"${EVENTS}"
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

# Isolated runtime. WORK lives OUTSIDE the artifact dir so the harness's
# recursive artifact-bundling never scans/hashes a binary SQLite DB (a pure-bash
# spin); only the small scenario JSONs belong under E2E_ARTIFACT_DIR.
WORK="$(e2e_mktemp health_and_selftests_work)"
ART="${E2E_ARTIFACT_DIR}/scenarios"
mkdir -p "${WORK}/storage" "${WORK}/home" "${ART}"
export AM_INTERFACE_MODE="cli"
export STORAGE_ROOT="${WORK}/storage"
export DATABASE_URL="sqlite:///${WORK}/mb.sqlite3"
export HOME="${WORK}/home"

# Bounded `am` invocation (never hangs the suite); captures stdout/stderr/exit.
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

# Assert a jq filter is truthy (jq -e); extra args (e.g. --arg) precede the filter.
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
# chokepoint selftest
# ---------------------------------------------------------------------------
e2e_case_banner "chokepoint: am doctor selftest verifies mutate() primitives"
amrun "${ART}/selftest.json" doctor selftest --format json
check "selftest reports ok=true" "${ART}/selftest.json" '.ok == true'
check "selftest enumerates chokepoint checks" "${ART}/selftest.json" '(.checks | length) > 0'

# ---------------------------------------------------------------------------
# C2 — write-path selftest (isolated scratch mailbox, 5 dimensions)
# ---------------------------------------------------------------------------
e2e_case_banner "C2: write-path selftest isolates the broken dimension"
amrun "${ART}/write_selftest.json" doctor write-selftest --format json
check "write-selftest ok on a healthy host" "${ART}/write_selftest.json" '.ok == true'
check "reports the five independent write dimensions" "${ART}/write_selftest.json" \
    '([.result.dimensions|keys]) as $d | (["transport","schema","lock","corruption","permissions"] | all(. as $n | ($d[0]|index($n)) != null))'
check "no failing dimension on a healthy run" "${ART}/write_selftest.json" \
    '.result.failing_dimension == null'
check "scratch mailbox is verified-isolated" "${ART}/write_selftest.json" \
    '.result.isolation_verified == true'
check "scratch_storage_root is NOT the operator STORAGE_ROOT" "${ART}/write_selftest.json" \
    '.scratch_storage_root != $sr' --arg sr "${STORAGE_ROOT}"

# ---------------------------------------------------------------------------
# C3 — MCP decode selftest (protocol-class)
# ---------------------------------------------------------------------------
e2e_case_banner "C3: MCP decode selftest reports protocol negotiation + decode class"
amrun "${ART}/mcp_selftest.json" doctor mcp-selftest --format json
check "mcp-selftest ok on a healthy host" "${ART}/mcp_selftest.json" '.ok == true'
check "negotiates the expected protocol version" "${ART}/mcp_selftest.json" \
    '.result.expected_protocol_version == .result.negotiated_protocol_version'
check "carries the decode-failure (protocol-class) check" "${ART}/mcp_selftest.json" \
    '([.result.checks[].name]) | index("decode_failure_detected") != null'
check "no protocol failure_class on a healthy run" "${ART}/mcp_selftest.json" \
    '.result.failure_class == null'

# ---------------------------------------------------------------------------
# C5 — rolled-up coordination verdict (safe-to-edit / read-only / blocked)
# ---------------------------------------------------------------------------
e2e_case_banner "C5: coordination verdict reports safe_to_mutate / read_only / owner_class"
amrun "${ART}/drain.json" doctor drain --json
check "drain reports a boolean safe_to_mutate verdict" "${ART}/drain.json" \
    '(.safe_to_mutate | type) == "boolean"'
check "drain reports a boolean read_only verdict" "${ART}/drain.json" \
    '(.read_only | type) == "boolean"'
check "drain names the owner_class" "${ART}/drain.json" \
    '(.owner_class | type) == "string"'
check "drain offers a safe next command" "${ART}/drain.json" \
    'has("safe_next_command") and has("recommended_next_action")'

# ---------------------------------------------------------------------------
# C1 — top-level health is never greener than its weakest critical verdict.
# Both halves materialize an L1 corruption-corpus recipe (valid vs missing
# tables) with sqlite3 — a fresh never-written DB would fail db_file_sanity for
# the wrong reason, so we build a real on-disk fixture for the "clean" case too.
# ---------------------------------------------------------------------------
RECIPE_DIR="${PROJECT_ROOT}/tests/fixtures/corruption_corpus/recipes"
if command -v sqlite3 >/dev/null 2>&1 && [ -f "${RECIPE_DIR}/missing_required_tables.sql" ]; then
    e2e_case_banner "C1a: a valid-schema database is healthy:true (no critical fail)"
    OK_DB="${WORK}/minimal_valid.sqlite3"
    sqlite3 "${OK_DB}" <"${RECIPE_DIR}/minimal_mailbox_schema.sql" 2>"${ART}/ok_fixture.err"
    DATABASE_URL="sqlite:///${OK_DB}" amrun "${ART}/health_ok.json" doctor check --json
    check "doctor check is healthy:true on the valid-schema fixture" \
        "${ART}/health_ok.json" '.healthy == true'
    check "no critical check is failing on the valid-schema fixture" \
        "${ART}/health_ok.json" '[.checks[] | select(.status == "fail")] | length == 0'

    e2e_case_banner "C1b: missing-required-tables fixture forces health to fail (never greener)"
    BAD_DB="${WORK}/missing_required_tables.sqlite3"
    sqlite3 "${BAD_DB}" <"${RECIPE_DIR}/missing_required_tables.sql" 2>"${ART}/bad_fixture.err"
    DATABASE_URL="sqlite:///${BAD_DB}" amrun "${ART}/health_bad.json" doctor check --json
    check "doctor check is healthy:false on the missing-tables fixture" \
        "${ART}/health_bad.json" '.healthy == false'
    check "overall verdict rolls up to fail (weakest critical wins)" \
        "${ART}/health_bad.json" '.summary.overall_status == "fail"'
    check "at least one critical check is reported failing" \
        "${ART}/health_bad.json" '[.checks[] | select(.status == "fail")] | length >= 1'
else
    e2e_skip "C1 needs the sqlite3 binary + corpus recipes to materialize fixtures (dir: ${RECIPE_DIR})"
fi

# ---------------------------------------------------------------------------
# C4 — send_message RefCell re-entrancy: no CLI surface
# ---------------------------------------------------------------------------
e2e_case_banner "C4: send_message re-entrancy fix (no CLI surface)"
e2e_skip "C4 (send_message RefCell re-entrancy, br-bvq1x.3.4) has no CLI-observable reproduction; it is covered by in-crate unit tests under asupersync worker-blocking and is out of scope for a black-box e2e"

# ---------------------------------------------------------------------------
log_summary "${E2E_SUITE}" "$((_E2E_PASS + _E2E_FAIL + _E2E_SKIP))" "${_E2E_PASS}" "${_E2E_FAIL}" \
    "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_health_and_selftests.sh" >>"${EVENTS}"
e2e_summary
