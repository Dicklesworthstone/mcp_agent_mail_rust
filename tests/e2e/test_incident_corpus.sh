#!/usr/bin/env bash
# test_incident_corpus.sh - L4 incident-corpus harness (Track L)
# @tags: reliability, session-history, track-l, corpus, scorecard
#
# Bead: br-bvq1x.12.4 (L4). One command runs the checked-in incident corpus
# and reports pass/fail PER INCIDENT CLASS with the originating anchor,
# aggregated into a release-readiness scorecard artifact.
#
# The six-pass session-history analysis that produced Track L was manual and
# throwaway (the `am_*_pass_*_scan.py` scratch scanners no longer exist).
# This harness is its promoted, repeatable successor: it orchestrates the
# durable L1/L2/L3 fixture families plus the host-pressure evidence emitter,
# so future releases are re-tested against the real historical failure
# shapes and fixed bugs cannot silently return.
#
# Fixture families orchestrated (all checked-in, no network/fleet access):
#   L1  tests/fixtures/corruption_corpus/ - manifest-driven corrupted-DB
#       corpus (one scorecard row per fixture, anchor from the manifest),
#       materialized + classified by crates/mcp-agent-mail-cli/tests/
#       corruption_corpus.rs.
#   L2  protocol & CLI-surface fixtures - CLI/MCP name-mismatch correction
#       matrix, HTTP decode-before-tool read-health regression, and the
#       FD-exhaustion RESOURCE_BUSY classifier (host pressure, not
#       corruption).
#   L3  concurrency & degraded-TUI fixtures - mixed inbox/reservation/
#       search/send load reproducer (c8->c16 write-concurrency cliff),
#       186ms injected render-stall heartbeat fixture, ATC tick-budget
#       overrun replay fixture.
#   EE  the "this failure is host pressure, not corruption" evidence
#       emitter: `am robot health --include-host` must emit a
#       host_pressure_likely verdict with reasons on a hermetic mailbox.
#
# Environment honesty:
#   * The L3 mixed-load reproducer is pinned to a tmpfs TMPDIR and runs
#     locally (not via rch) so the pinning is real. On real-disk TMPDIR it
#     deterministically reproduces a pool 'connection validation failed'
#     panic - that sensitivity is tracked as br-kjta0 and the class is
#     honestly SKIPped (never silently passed) when no tmpfs is available.
#
# Scorecard: ${E2E_ARTIFACT_DIR}/scorecard.json - schema_version, corpus_id,
# environment, one row per incident class {family, id, incident_class,
# anchor, status, evidence}, summary counts, release_ready verdict
# (true iff zero failed classes; skips are listed, never hidden).
#
# One command:
#   am e2e run --project . incident_corpus     (native runner)
#   bash tests/e2e/test_incident_corpus.sh     (direct)

set -uo pipefail

export E2E_SUITE="incident_corpus"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
# shellcheck source=scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Incident-Corpus Harness (Track L4 release-readiness scorecard)"

CORPUS_DIR="${PROJECT_ROOT}/tests/fixtures/corruption_corpus"
MANIFEST="${CORPUS_DIR}/manifest.json"
SCORECARD_ROWS="${E2E_ARTIFACT_DIR}/scorecard_rows.tsv"
SCORECARD_JSON="${E2E_ARTIFACT_DIR}/scorecard.json"
: > "${SCORECARD_ROWS}"

json_field() {
    # json_field <file> <python-expression-over-d>
    python3 -c "
import json, sys
with open('$1') as f:
    d = json.load(f)
print($2)
" 2>/dev/null
}

scorecard_row() {
    # scorecard_row <family> <id> <incident_class> <status> <evidence> <anchor>
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" "$6" \
        >> "${SCORECARD_ROWS}"
}

run_cargo_fixture() {
    # run_cargo_fixture <log-basename> <min-passed> <cargo-args...>
    # Passes iff no test binary reports FAILED and the summed pass count
    # across 'test result: ok' lines reaches <min-passed> (a cargo build
    # failure produces no result lines and therefore fails).
    local log="${E2E_ARTIFACT_DIR}/$1"
    local min="$2"
    shift 2
    e2e_run_cargo "$@" 2>&1 | tee "${log}" >/dev/null
    local cargo_status=${PIPESTATUS[0]}
    if [ "${cargo_status}" -ne 0 ]; then
        return 1
    fi
    local passed
    passed=$(grep -oP 'test result: ok\. \K\d+(?= passed)' "${log}" \
        | awk '{s+=$1} END {print s+0}')
    if grep -q 'test result: FAILED' "${log}"; then
        return 1
    fi
    if [ "${passed:-0}" -lt "${min}" ]; then
        return 1
    fi
    return 0
}

# ── Phase 0: Corpus inventory sanity ─────────────────────────────────

e2e_section "Phase 0: L1 corpus manifest inventory"

e2e_assert_file_exists "L1 corpus manifest present" "${MANIFEST}"

FIXTURE_COUNT="$(json_field "${MANIFEST}" "len(d['fixtures'])")"
if [ -n "${FIXTURE_COUNT}" ] && [ "${FIXTURE_COUNT}" -ge 12 ] 2>/dev/null; then
    e2e_pass "manifest parses; ${FIXTURE_COUNT} fixtures (>= 12 required incident classes)"
else
    e2e_fail "manifest missing/short: got '${FIXTURE_COUNT:-unparseable}' fixtures, need >= 12"
fi

if [ "$(json_field "${MANIFEST}" "all(f.get('id') and f.get('incident_anchor') and f.get('track_a_classification') for f in d['fixtures'])")" = "True" ]; then
    e2e_pass "every fixture declares id + incident_anchor + track_a_classification"
else
    e2e_fail "manifest fixture(s) missing id/incident_anchor/track_a_classification"
fi

# ── Phase 1: L1 corrupted-DB corpus (materialize + classify) ─────────

e2e_section "Phase 1: L1 corpus materialization + classification"

L1_STATUS="fail"
if run_cargo_fixture "l1_corruption_corpus.log" 3 \
    test -p mcp-agent-mail-cli --test corruption_corpus; then
    L1_STATUS="pass"
    e2e_pass "L1 corpus: coverage + documentation + deterministic materialization green"
else
    e2e_fail "L1 corpus tests failed - see ${E2E_ARTIFACT_DIR}/l1_corruption_corpus.log"
fi

# One scorecard row per manifest fixture: the incident class and anchor come
# from the manifest itself, the status from the corpus test run above.
python3 - "${MANIFEST}" "${L1_STATUS}" "l1_corruption_corpus.log" >> "${SCORECARD_ROWS}" <<'PY'
import json, sys

manifest_path, status, evidence = sys.argv[1], sys.argv[2], sys.argv[3]
with open(manifest_path) as f:
    manifest = json.load(f)
for fx in manifest["fixtures"]:
    anchor = " ".join(str(fx["incident_anchor"]).split())
    print("\t".join(["L1", fx["id"], fx["track_a_classification"], status,
                     evidence, anchor]))
PY

# ── Phase 2: L2 protocol & CLI-surface fixtures ──────────────────────

e2e_section "Phase 2: L2 CLI/MCP name-mismatch correction matrix"

MATRIX_STATUS="pass"
if run_cargo_fixture "l2_name_mismatch_mcp_binary.log" 1 \
    test -p mcp-agent-mail --bin mcp-agent-mail \
    command_correction_covers_protocol_and_cli_mismatch_names; then
    e2e_pass "MCP-mode denial emits exact corrected commands (server binary matrix)"
else
    MATRIX_STATUS="fail"
    e2e_fail "server-binary name-mismatch matrix failed - see ${E2E_ARTIFACT_DIR}/l2_name_mismatch_mcp_binary.log"
fi
if run_cargo_fixture "l2_name_mismatch_cli_matrix.log" 1 \
    test -p mcp-agent-mail-cli --test mode_matrix_harness \
    matrix_mcp_name_mismatch_denials_print_exact_corrections; then
    e2e_pass "CLI mode-matrix harness pins the mismatch->correction table"
else
    MATRIX_STATUS="fail"
    e2e_fail "CLI mode-matrix mismatch fixture failed - see ${E2E_ARTIFACT_DIR}/l2_name_mismatch_cli_matrix.log"
fi
scorecard_row "L2" "cli_mcp_name_mismatch_matrix" "cli_mcp_name_mismatch_matrix" \
    "${MATRIX_STATUS}" "l2_name_mismatch_mcp_binary.log,l2_name_mismatch_cli_matrix.log" \
    "Track E: agents invoked CLI verbs with MCP tool names (reserve/file-reserve/file_reservation_paths/macro_start_session/send_message/send/inbox/serve/reservations) and needed exact corrected-command output instead of unusable denials"

e2e_section "Phase 2: L2 HTTP decode-before-tool read-health regression"

DECODE_STATUS="fail"
if run_cargo_fixture "l2_decode_before_tool.log" 1 \
    test -p mcp-agent-mail-server --lib \
    http_tools_call_param_decode_error_preserves_read_health; then
    DECODE_STATUS="pass"
    e2e_pass "malformed tools/call params fail before tool execution; reads stay healthy"
else
    e2e_fail "decode-before-tool regression failed - see ${E2E_ARTIFACT_DIR}/l2_decode_before_tool.log"
fi
scorecard_row "L2" "http_decode_before_tool" "http_decode_before_tool" \
    "${DECODE_STATUS}" "l2_decode_before_tool.log" \
    "ts2 anchor: MCP write failed with an rmcp JsonRpcMessage deserialization error BEFORE tool execution while reads still succeeded"

e2e_section "Phase 2: L2 FD-exhaustion RESOURCE_BUSY classifier"

FD_STATUS="pass"
if run_cargo_fixture "l2_fd_exhaustion_db.log" 1 \
    test -p mcp-agent-mail-db --lib fd_exhaustion; then
    e2e_pass "db classifier: FD exhaustion is retryable host pressure, not corruption"
else
    FD_STATUS="fail"
    e2e_fail "db fd_exhaustion classifier failed - see ${E2E_ARTIFACT_DIR}/l2_fd_exhaustion_db.log"
fi
if run_cargo_fixture "l2_fd_exhaustion_tools.log" 1 \
    test -p mcp-agent-mail-tools --lib fd_exhaustion; then
    e2e_pass "tools mapping: FD exhaustion -> RESOURCE_BUSY with file_descriptors guidance"
else
    FD_STATUS="fail"
    e2e_fail "tools fd_exhaustion mapping failed - see ${E2E_ARTIFACT_DIR}/l2_fd_exhaustion_tools.log"
fi
scorecard_row "L2" "fd_exhaustion_resource_busy" "fd_exhaustion_resource_busy" \
    "${FD_STATUS}" "l2_fd_exhaustion_db.log,l2_fd_exhaustion_tools.log" \
    "send_message retry loop under FD exhaustion ('Too many open files. Freed 0 cached repos') must classify as RESOURCE_BUSY host pressure with file_descriptors guidance, not corruption"

# ── Phase 3: L3 concurrency & degraded-TUI fixtures ──────────────────

e2e_section "Phase 3: L3 mixed-load write-concurrency reproducer (tmpfs-pinned)"

MIXED_TMPDIR=""
if [ "$(stat -f -c %T /tmp 2>/dev/null)" = "tmpfs" ]; then
    MIXED_TMPDIR="/tmp"
elif [ "$(stat -f -c %T /dev/shm 2>/dev/null)" = "tmpfs" ]; then
    MIXED_TMPDIR="/dev/shm"
fi

MIXED_STATUS="fail"
if [ -z "${MIXED_TMPDIR}" ]; then
    MIXED_STATUS="skip"
    e2e_skip "mixed-load reproducer: no tmpfs TMPDIR available (br-kjta0: real-disk TMPDIR deterministically reproduces pool 'connection validation failed')"
    scorecard_row "L3" "mixed_load_write_concurrency_cliff" "mixed_load_write_concurrency_cliff" \
        "skip" "no tmpfs available; see br-kjta0" \
        "historical c8->c16 write-concurrency cliff: concurrent inbox-fetch + reservation acquire/release + search + send exposing pool/lock contention"
else
    # Runs locally (not via rch) so the tmpfs TMPDIR pin is real on the
    # machine executing the test; br-kjta0 tracks the real-disk sensitivity.
    # shellcheck disable=SC2030  # subshell-local override is intentional
    if ( export TMPDIR="${MIXED_TMPDIR}" E2E_CARGO_FORCE_LOCAL=1
        run_cargo_fixture "l3_mixed_load.log" 1 \
            test -p mcp-agent-mail-db --test load_concurrency \
            mixed_inbox_reservation_search_send_load_reproducer -- --exact ); then
        MIXED_STATUS="pass"
        e2e_pass "mixed inbox/reservation/search/send load reproducer green (TMPDIR=${MIXED_TMPDIR})"
    else
        e2e_fail "mixed-load reproducer failed on tmpfs TMPDIR=${MIXED_TMPDIR} - see ${E2E_ARTIFACT_DIR}/l3_mixed_load.log (real-disk failures are br-kjta0; a tmpfs failure is a REGRESSION)"
    fi
    scorecard_row "L3" "mixed_load_write_concurrency_cliff" "mixed_load_write_concurrency_cliff" \
        "${MIXED_STATUS}" "l3_mixed_load.log (TMPDIR=${MIXED_TMPDIR})" \
        "historical c8->c16 write-concurrency cliff: concurrent inbox-fetch + reservation acquire/release + search + send exposing pool/lock contention"
fi

e2e_section "Phase 3: L3 injected render-stall heartbeat fixture"

STALL_STATUS="fail"
if run_cargo_fixture "l3_render_stall.log" 1 \
    test -p mcp-agent-mail-server --lib 'tui_bridge::tests::loop_heartbeat' \
    -- --test-threads=4; then
    STALL_STATUS="pass"
    e2e_pass "186ms injected render stall captured by loop heartbeats; input progress stays visible"
else
    e2e_fail "render-stall heartbeat fixture failed - see ${E2E_ARTIFACT_DIR}/l3_render_stall.log"
fi
scorecard_row "L3" "tui_render_stall_heartbeat" "tui_render_stall_heartbeat" \
    "${STALL_STATUS}" "l3_render_stall.log" \
    "TUI freeze reports (Track I1): a 186ms injected render stall must be captured by loop heartbeats without hiding input progress"

e2e_section "Phase 3: L3 ATC tick-budget overrun replay fixture"

ATC_STATUS="fail"
if run_cargo_fixture "l3_atc_budget.log" 1 \
    test -p mcp-agent-mail-server --lib \
    atc_replay_tick_fixture_records_budget_overrun_debt; then
    ATC_STATUS="pass"
    e2e_pass "ATC replay fixture records the 186ms-vs-5ms tick-budget overrun debt"
else
    e2e_fail "ATC budget-overrun replay fixture failed - see ${E2E_ARTIFACT_DIR}/l3_atc_budget.log"
fi
scorecard_row "L3" "atc_tick_budget_overrun" "atc_tick_budget_overrun" \
    "${ATC_STATUS}" "l3_atc_budget.log" \
    "ATC operator-tick budget overrun (Track I3): 186ms observed vs 5ms budget must be recorded as replay debt, not dropped"

# ── Phase 4: Host-pressure evidence emitter ──────────────────────────

e2e_section "Phase 4: host-pressure-not-corruption evidence emitter"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

EMITTER_WORK="$(e2e_mktemp "e2e_incident_corpus_host")"
mkdir -p "${EMITTER_WORK}/storage" "${EMITTER_WORK}/repo"
# HTTP_PORT=1 is never listening: forces local SQLite fallback, keeping the
# probe hermetic (no developer/CI server on the canonical port is touched).
EMITTER_ENV=(
    "DATABASE_URL=sqlite:///${EMITTER_WORK}/db.sqlite3"
    "STORAGE_ROOT=${EMITTER_WORK}/storage"
    "HTTP_HOST=127.0.0.1"
    "HTTP_PORT=1"
    "AM_INTERFACE_MODE=cli"
)

EMITTER_STATUS="fail"
if env "${EMITTER_ENV[@]}" am macros start-session --project "${EMITTER_WORK}/repo" \
    --program incident-corpus --model probe \
    >"${E2E_ARTIFACT_DIR}/emitter_seed.out" 2>"${E2E_ARTIFACT_DIR}/emitter_seed.stderr"; then
    e2e_pass "hermetic mailbox seeded via macros start-session"
else
    e2e_fail "failed to seed hermetic mailbox - see ${E2E_ARTIFACT_DIR}/emitter_seed.stderr"
fi

HOST_JSON="${E2E_ARTIFACT_DIR}/robot_health_host.json"
if env "${EMITTER_ENV[@]}" am robot health --include-host --format json \
    >"${HOST_JSON}" 2>"${E2E_ARTIFACT_DIR}/robot_health_host.stderr"; then
    e2e_pass "am robot health --include-host exits 0 with no live server"
else
    e2e_fail "am robot health --include-host exited non-zero"
fi

# The emitter contract: a host section with a boolean host_pressure_likely
# verdict, a reasons list, and the probed path. The verdict VALUE is not
# asserted - on a genuinely loaded host it is honestly true, and that is
# exactly the evidence this emitter exists to produce.
if [ "$(json_field "${HOST_JSON}" "isinstance(d.get('host'), dict) and isinstance(d['host'].get('host_pressure_likely'), bool) and isinstance(d['host'].get('reasons'), list) and bool(d['host'].get('probe_path'))")" = "True" ]; then
    EMITTER_STATUS="pass"
    e2e_pass "host section emits host_pressure_likely verdict + reasons + probe_path"
else
    e2e_fail "host evidence emitter shape wrong - see ${HOST_JSON}"
fi
scorecard_row "EE" "host_pressure_not_corruption" "host_pressure_not_corruption" \
    "${EMITTER_STATUS}" "robot_health_host.json" \
    "'Tools time out / Database corruption detected' under heavy load: am robot health --include-host must emit a host_pressure_likely verdict with reasons so host overload is not misdiagnosed as mailbox corruption"

# ── Phase 5: Release-readiness scorecard ─────────────────────────────

e2e_section "Phase 5: release-readiness scorecard"

CARGO_MODE="local-fallback"
# shellcheck disable=SC2031  # the Phase 3 override was deliberately subshell-local
if [ "${E2E_CARGO_FORCE_LOCAL}" = "1" ]; then
    CARGO_MODE="forced-local"
elif command -v rch >/dev/null 2>&1; then
    CARGO_MODE="rch"
fi

if python3 - "${SCORECARD_ROWS}" "${MANIFEST}" "${SCORECARD_JSON}" \
    "${CARGO_MODE}" "${MIXED_TMPDIR}" <<'PY'
import json
import hashlib
import os
import sys

rows_path, manifest_path, out_path, cargo_mode, mixed_tmpdir = sys.argv[1:6]

rows = []
with open(rows_path) as f:
    for line in f:
        line = line.rstrip("\n")
        if not line:
            continue
        family, fid, incident_class, status, evidence, anchor = line.split("\t")
        rows.append({
            "family": family,
            "id": fid,
            "incident_class": incident_class,
            "anchor": anchor,
            "status": status,
            "evidence": evidence,
        })

with open(manifest_path) as f:
    manifest = json.load(f)

problems = []

manifest_ids = {fx["id"] for fx in manifest["fixtures"]}
l1_ids = {r["id"] for r in rows if r["family"] == "L1"}
if l1_ids != manifest_ids:
    problems.append(
        f"L1 coverage mismatch: missing={sorted(manifest_ids - l1_ids)} "
        f"extra={sorted(l1_ids - manifest_ids)}"
    )

required = {
    "cli_mcp_name_mismatch_matrix",
    "http_decode_before_tool",
    "fd_exhaustion_resource_busy",
    "mixed_load_write_concurrency_cliff",
    "tui_render_stall_heartbeat",
    "atc_tick_budget_overrun",
    "host_pressure_not_corruption",
}
present = {r["id"] for r in rows if r["family"] != "L1"}
if not required <= present:
    problems.append(f"missing incident classes: {sorted(required - present)}")

bad_status = [r["id"] for r in rows if r["status"] not in ("pass", "fail", "skip")]
if bad_status:
    problems.append(f"invalid statuses on: {bad_status}")

summary = {
    "total": len(rows),
    "pass": sum(1 for r in rows if r["status"] == "pass"),
    "fail": sum(1 for r in rows if r["status"] == "fail"),
    "skip": sum(1 for r in rows if r["status"] == "skip"),
}

scorecard = {
    "schema_version": 1,
    "suite": "incident_corpus",
    "corpus_id": manifest.get("corpus_id"),
    "environment": {
        "cargo_mode": cargo_mode,
        "mixed_load_tmpdir": mixed_tmpdir or None,
    },
    "classes": rows,
    "summary": summary,
    "skipped_classes": sorted(r["id"] for r in rows if r["status"] == "skip"),
    "failed_classes": sorted(r["id"] for r in rows if r["status"] == "fail"),
    "release_ready": summary["total"] > 0 and summary["fail"] == 0 and summary["skip"] == 0 and not problems,
    "consistency_problems": problems,
}

with open(out_path + ".pending", "x") as f:
    json.dump(scorecard, f, indent=2)
    f.write("\n")
    f.flush()
    os.fsync(f.fileno())
os.link(out_path + ".pending", out_path)

# The parent accepts only this terminal producer's exact bytes and invocation.
# Keep the staged file: publication never overwrites or removes old evidence.
release_run = os.environ.get("AM_E2E_RELEASE_RUN")
if release_run:
    with open(out_path, "rb") as f:
        digest = hashlib.sha256(f.read()).hexdigest()
    receipt = {
        "schema_version": 1,
        "run": json.loads(release_run),
        "scorecard_path": os.path.realpath(out_path),
        "scorecard_sha256": digest,
    }
    receipt_path = os.environ["AM_E2E_RELEASE_RECEIPT"]
    with open(receipt_path + ".pending", "x") as f:
        json.dump(receipt, f)
        f.write("\n")
        f.flush()
        os.fsync(f.fileno())
    os.link(receipt_path + ".pending", receipt_path)
    print("AM_E2E_RELEASE_RECEIPT " + json.dumps(receipt), flush=True)

width = max((len(r["id"]) for r in rows), default=10)
for r in rows:
    print(f"  [{r['status']:>4}] {r['family']:<2} {r['id']:<{width}}  {r['anchor'][:70]}")
print(
    f"  classes={summary['total']} pass={summary['pass']} "
    f"fail={summary['fail']} skip={summary['skip']} "
    f"release_ready={scorecard['release_ready']}"
)

sys.exit(1 if problems else 0)
PY
then
    e2e_pass "scorecard complete + consistent: every manifest fixture and required incident class present"
else
    e2e_fail "scorecard incomplete/inconsistent - see ${SCORECARD_JSON}"
fi

e2e_assert_file_exists "scorecard.json written" "${SCORECARD_JSON}"

VERDICT="$(json_field "${SCORECARD_JSON}" "'ready' if d['release_ready'] else 'not-ready'")"
FAILED_CLASSES="$(json_field "${SCORECARD_JSON}" "','.join(d['failed_classes']) or 'none'")"
if [ "${VERDICT}" = "ready" ]; then
    e2e_pass "release-readiness verdict: READY (failed classes: ${FAILED_CLASSES})"
else
    e2e_fail "release-readiness verdict: NOT READY (failed classes: ${FAILED_CLASSES})"
fi

e2e_summary
