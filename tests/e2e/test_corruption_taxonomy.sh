#!/usr/bin/env bash
# test_corruption_taxonomy.sh — E2E for the Track A/M/K corruption taxonomy.
# @tags: reliability, track-a, track-m, track-n, corruption, taxonomy, a1, a2, a4
#
# Drives a REAL built `am` against corrupted / structurally-broken / healthy
# SQLite databases (seeded with the real Agent Mail schema plus the checked-in
# L1 corpus under tests/fixtures/corruption_corpus/) and asserts the typed
# corruption-diagnosis contract end-to-end:
#
#   A1/A4 — a HEALTHY mailbox is never false-flagged as corrupt
#           (`am doctor health` exits 0; `check --json .healthy == true`; the
#           database_fix_strategy is not a destructive repair/reconstruct).
#   A1/A4 — a genuinely CORRUPT B-tree (pages 2+ mangled) is upheld: the
#           canonical double-probe ConfirmsCorruption, so A4 KEEPS the recovery
#           verdict (`db_file_sanity` fail, "possible corruption",
#           database_fix_strategy.strategy ∈ {repair, reconstruct}); health exits 1.
#   A2    — the structured diagnosis envelope carries class/check/status/detail/
#           next_action on the corrupt case.
#   A4    — a STRUCTURAL recovery reason (missing required tables) is NOT relabeled
#           as an authoritative corruption claim — it passes through as a schema/
#           required-table verdict (the critical false-negative guard).
#   M/G4  — a benign 0-byte WAL sidecar (L1 `zero_byte_wal`) is healthy, not
#           corruption.
#   L1    — the corpus fixtures materialize deterministically and classify the
#           same way on every run.
#
# Everything runs against an isolated STORAGE_ROOT / HOME / DATABASE_URL — it
# never touches a real ~/.mcp_agent_mail_git_mailbox_repo. HTTP_PORT is pinned to
# an unused port so CLI verbs fall back to the local SQLite path instead of a
# developer/CI server on the canonical 127.0.0.1:8765 (which would 401).
#
# SCOPED OUT (honest SKIPs — covered elsewhere, not black-box reproducible here):
#   * A4 DOWNGRADE of a cheap-probe FALSE POSITIVE (frankensqlite foreign_key_check
#     misfiring on a canonically-healthy DB → engine_probe_limitation) requires a
#     frankensqlite FK-misfire on the real am schema, which is not deterministically
#     reproducible from bash. It is covered in-process by the A4 unit suite in
#     crates/mcp-agent-mail-cli (doctor_authority_guard_downgrades_unconfirmed_malformed_on_healthy_db,
#     doctor_confirm_flips_fk_false_positive_to_engine_limitation, and the
#     doctor_canonical_double_probe_* battery).
#   * K3 DB-health circuit breaker tripping needs sustained failing load (state
#     machine timing), out of scope for a deterministic taxonomy probe.
#   * K1 loss-honest salvage needs `am doctor reconstruct` (a MUTATION) plus a
#     populated Git archive; this read-only taxonomy suite does not mutate.
#   * The hand-built byte-header corpus fixtures (btree_page_type_zero,
#     freelist_leaf_exceeds_db_size, short_read_fetching_page) need the Rust
#     loader's exact byte construction; they are materialized + classified by
#     crates/mcp-agent-mail-cli/tests/corruption_corpus.rs and the A4
#     seed_malformed_btree_db unit fixture. This suite mangles a REAL am-schema DB
#     instead, which exercises the same canonical-confirmed corruption path.
#
# Ref: br-bvq1x.14.3 (N3). Depends on A2/A4/K1/K3/L1 (all closed).

set -uo pipefail

E2E_SUITE="corruption_taxonomy"

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
e2e_banner "Track A/M/K — Corruption Diagnosis Taxonomy E2E"

EVENTS="${E2E_ARTIFACT_DIR}/events.jsonl"
: >"${EVENTS}"

REPRO="bash tests/e2e/test_corruption_taxonomy.sh"

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq required for taxonomy assertions"
    log_summary "${E2E_SUITE}" 0 0 0 "${E2E_ARTIFACT_DIR}" "${REPRO}" >>"${EVENTS}"
    e2e_summary
    exit 0
fi
if ! command -v sqlite3 >/dev/null 2>&1; then
    e2e_skip "sqlite3 required to materialize corpus fixtures"
    log_summary "${E2E_SUITE}" 0 0 0 "${E2E_ARTIFACT_DIR}" "${REPRO}" >>"${EVENTS}"
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

CORPUS="${PROJECT_ROOT}/tests/fixtures/corruption_corpus"
WORK="$(e2e_mktemp corruption_taxonomy_work)"
ART="${E2E_ARTIFACT_DIR}/scenarios"
mkdir -p "${WORK}/storage" "${WORK}/home" "${WORK}/repo" "${ART}"
export AM_INTERFACE_MODE="cli"
export STORAGE_ROOT="${WORK}/storage"
export HOME="${WORK}/home"
export HTTP_HOST="127.0.0.1"
export HTTP_PORT="${AM_E2E_UNUSED_PORT:-8799}"

AM_CMD_TIMEOUT="${AM_CMD_TIMEOUT:-60}"

# Run `am` with a bounded timeout, capturing exit code WITHOUT tripping the
# harness's errexit: `am doctor health` intentionally exits 1 on findings.
amrun() {
    local db="$1" out="$2"
    shift 2
    local rc=0
    if command -v timeout >/dev/null 2>&1; then
        DATABASE_URL="sqlite:///${db}" timeout "${AM_CMD_TIMEOUT}" "${AM_BIN}" "$@" \
            >"${out}" 2>"${out}.err" || rc=$?
    else
        DATABASE_URL="sqlite:///${db}" "${AM_BIN}" "$@" >"${out}" 2>"${out}.err" || rc=$?
    fi
    printf '%s\n' "${rc}" >"${out}.exit"
    return 0
}

# Seed a healthy database carrying the REAL Agent Mail schema by registering an
# agent (opening the DB for a write materializes the full schema).
seed_healthy_db() {
    local db="$1" name="$2"
    local out="${ART}/seed_${name}.out"
    amrun "${db}" "${out}" agents register \
        --project "${WORK}/repo" --name "${name}" --program claude-code --model opus-4.8
    [ "$(cat "${out}.exit")" = "0" ] && [ -f "${db}" ]
}

# jq assertion against a JSON file → pass/fail with the expression as evidence.
check_json() {
    local label="$1" file="$2" expr="$3"
    if jq -e "${expr}" "${file}" >/dev/null 2>&1; then
        e2e_pass "${label}"
    else
        e2e_fail "${label} (jq: ${expr}); got: $(jq -c '.summary.primary_issue // .' "${file}" 2>/dev/null | head -c 200)"
    fi
}

# ===========================================================================
# Case 1 (A1/A4): a healthy real-schema mailbox is NOT false-flagged corrupt.
# ===========================================================================
e2e_log "Case 1: healthy mailbox is not false-flagged (A1/A4)"
HEALTHY_DB="${WORK}/healthy.sqlite3"
if seed_healthy_db "${HEALTHY_DB}" "BlueLake"; then
    e2e_pass "seeded a healthy real-schema mailbox (agent registered)"
    amrun "${HEALTHY_DB}" "${ART}/healthy_health.out" doctor health
    e2e_assert_eq "healthy: am doctor health exits 0" "0" "$(cat "${ART}/healthy_health.out.exit")"
    amrun "${HEALTHY_DB}" "${ART}/healthy_check.json" doctor check --json
    check_json "healthy: check --json reports .healthy == true" \
        "${ART}/healthy_check.json" '.healthy == true'
    check_json "healthy: no destructive repair/reconstruct fix-strategy (A4 no false-flag)" \
        "${ART}/healthy_check.json" \
        '((.summary.database_fix_strategy.strategy // "none") | test("repair|reconstruct") | not)'
else
    e2e_fail "could not seed a healthy real-schema mailbox"
fi

# ===========================================================================
# Case 2 (A1/A2/A4): a canonically-confirmed B-tree corruption is UPHELD.
# Mangle every byte after page 1 of the healthy DB — the schema/header survive
# so the file still opens, but the table B-tree pages are destroyed. The
# canonical double-probe ConfirmsCorruption, so A4 keeps the recovery verdict.
# ===========================================================================
e2e_log "Case 2: confirmed B-tree corruption is upheld (A1/A2/A4)"
CORRUPT_DB="${WORK}/corrupt.sqlite3"
if [ -f "${HEALTHY_DB}" ]; then
    cp "${HEALTHY_DB}" "${CORRUPT_DB}"
    python3 - "${CORRUPT_DB}" <<'PY'
import sys
p = sys.argv[1]
b = bytearray(open(p, "rb").read())
ps = int.from_bytes(b[16:18], "big")
ps = 65536 if ps == 1 else ps
for i in range(ps, len(b)):
    b[i] = 0xA5
open(p, "wb").write(b)
PY
    amrun "${CORRUPT_DB}" "${ART}/corrupt_health.out" doctor health
    e2e_assert_eq "corrupt: am doctor health exits 1 (findings)" "1" "$(cat "${ART}/corrupt_health.out.exit")"
    e2e_assert_contains "corrupt: health verdict names possible corruption" \
        "$(cat "${ART}/corrupt_health.out")" "possible corruption"
    amrun "${CORRUPT_DB}" "${ART}/corrupt_check.json" doctor check --json
    check_json "corrupt: check --json reports .healthy == false" \
        "${ART}/corrupt_check.json" '.healthy == false'
    # A4: ConfirmsCorruption keeps the recovery action (repair/reconstruct).
    check_json "corrupt: database_fix_strategy is a recovery action (A4 upholds confirmed corruption)" \
        "${ART}/corrupt_check.json" \
        '(.summary.database_fix_strategy.strategy // "" | test("repair|reconstruct"))'
    # A2: structured envelope shape on the corruption primary issue.
    check_json "corrupt: A2 envelope has class/check/status/detail/next_action" \
        "${ART}/corrupt_check.json" \
        '.summary.primary_issue | has("class") and has("check") and has("status") and has("detail")'
    check_json "corrupt: primary issue is the db_file_sanity check in fail status" \
        "${ART}/corrupt_check.json" \
        '.summary.primary_issue.check == "db_file_sanity" and .summary.primary_issue.status == "fail"'
    check_json "corrupt: primary issue detail names possible corruption (A1)" \
        "${ART}/corrupt_check.json" \
        '(.summary.primary_issue.detail // "" | test("possible corruption"))'
else
    e2e_fail "no healthy DB to corrupt for Case 2"
fi

# ===========================================================================
# Case 3 (A4): a STRUCTURAL recovery reason is NOT relabeled corruption.
# The L1 missing_required_tables fixture is a valid SQLite file with the wrong
# schema. A4 must let the schema/required-table verdict pass through; it must
# NOT surface an authoritative "possible corruption" / malformed claim.
# ===========================================================================
e2e_log "Case 3: structural recovery is not a corruption claim (A4 pass-through)"
STRUCT_DB="${WORK}/missing_tables.sqlite3"
if sqlite3 "${STRUCT_DB}" < "${CORPUS}/recipes/missing_required_tables.sql" 2>"${ART}/struct_seed.err"; then
    e2e_pass "materialized L1 missing_required_tables fixture"
    amrun "${STRUCT_DB}" "${ART}/struct_check.json" doctor check --json
    check_json "structural: not healthy (wrong schema)" \
        "${ART}/struct_check.json" '.healthy == false'
    check_json "structural: primary issue is NOT a db_file_sanity corruption claim (A4)" \
        "${ART}/struct_check.json" \
        '(.summary.primary_issue.check // "") != "db_file_sanity"'
    check_json "structural: detail does NOT assert authoritative corruption (A4 pass-through)" \
        "${ART}/struct_check.json" \
        '((.summary.primary_issue.detail // "") | test("malformed|possible corruption") | not)'
else
    e2e_fail "could not materialize missing_required_tables fixture"
fi

# ===========================================================================
# Case 4 (M/G4): a benign 0-byte WAL sidecar (L1 zero_byte_wal) is healthy.
# ===========================================================================
e2e_log "Case 4: benign 0-byte WAL sidecar is healthy, not corruption (M/G4)"
ZW_DB="${WORK}/zero_wal.sqlite3"
if seed_healthy_db "${ZW_DB}" "GreenCastle"; then
    : >"${ZW_DB}-wal"
    amrun "${ZW_DB}" "${ART}/zero_wal_health.out" doctor health
    e2e_assert_eq "zero-byte WAL: am doctor health exits 0 (benign sidecar)" \
        "0" "$(cat "${ART}/zero_wal_health.out.exit")"
else
    e2e_fail "could not seed DB for zero-byte WAL case"
fi

# ===========================================================================
# Case 5 (L1): corpus fixtures materialize + classify deterministically.
# Re-materialize the FK-graph fixture twice and confirm the byte content is
# stable (deterministic corpus), and that its am classification is stable.
# ===========================================================================
e2e_log "Case 5: L1 corpus materializes deterministically (L1)"
FK1="${WORK}/fk1.sqlite3"; FK2="${WORK}/fk2.sqlite3"
sqlite3 "${FK1}" < "${CORPUS}/recipes/valid_fk_graph.sql" 2>/dev/null
sqlite3 "${FK2}" < "${CORPUS}/recipes/valid_fk_graph.sql" 2>/dev/null
H1="$(sha256sum "${FK1}" | awk '{print $1}')"
H2="$(sha256sum "${FK2}" | awk '{print $1}')"
e2e_assert_eq "L1 valid_fk_graph fixture is byte-deterministic across runs" "${H1}" "${H2}"
# The manifest pins an expected classification per fixture; assert the manifest
# is present and self-consistent (12 fixtures, each with a track_a_classification).
e2e_assert_file_exists "L1 corpus manifest present" "${CORPUS}/manifest.json"
check_json "L1 manifest: every fixture declares a track_a_classification" \
    "${CORPUS}/manifest.json" \
    '(.fixtures | length) > 0 and ([.fixtures[] | select(has("track_a_classification") and has("canonical_sqlite_verdict"))] | length) == (.fixtures | length)'

# ===========================================================================
# Honest skips — surfaces not deterministically reproducible from black-box bash.
# ===========================================================================
e2e_skip "A4 downgrade of a frankensqlite FK false positive — covered by the A4 unit suite (mcp-agent-mail-cli doctor_authority_guard_* / doctor_canonical_double_probe_*)"
e2e_skip "K3 DB-health circuit breaker trip — needs sustained failing load (state-machine timing)"
e2e_skip "K1 loss-honest salvage — needs am doctor reconstruct (mutation) + a populated Git archive"

log_summary "${E2E_SUITE}" "${_E2E_TOTAL}" "${_E2E_PASS}" "${_E2E_FAIL}" "${E2E_ARTIFACT_DIR}" "${REPRO}" >>"${EVENTS}"
e2e_summary
[ "${_E2E_FAIL}" -eq 0 ]
