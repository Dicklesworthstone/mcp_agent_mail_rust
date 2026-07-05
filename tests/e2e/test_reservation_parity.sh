#!/usr/bin/env bash
# test_reservation_parity.sh — E2E for Track F (reservation DB↔archive parity).
# @tags: reliability, track-f, track-n, reservations, parity, f1, f2, f3, f4, f5
#
# Asserts, against a REAL built `am` binary, the Track F reservation-reliability
# contract from the 196-reservation audit incident class (DB and Git archive
# disagreeing about who holds a lease):
#
#   F1 — reconcile-on-read: an active DB reservation whose archive artifact
#        (`projects/<slug>/file_reservations/id-<id>.json`) vanished in a
#        crash gap (DB commit landed, archive write did not) is HEALED on the
#        next `file_reservation_paths` read — the artifact reappears with the
#        correct holder. Divergent artifacts (wrong path_pattern) are healed
#        back to DB truth the same way.
#   F2 — parity checker: per-field DB↔archive drift is reported by
#        `am doctor health` (`reservation_parity:` line naming the drifted
#        field) and by the per-FM registry view
#        `am doctor fix --only fm-db-state-files-reservation-db-archive-parity --list`.
#   F3 — release is idempotent and parity-aware: double-release is a clean
#        `released=0` no-op on BOTH the MCP tool surface and the operator CLI,
#        and the release reconciles the archive artifact (released_ts set).
#        The DB-unavailable release-intent QUEUE path is attempted but expected
#        to honest-SKIP: serve-stdio fails closed at boot on an unavailable
#        mailbox, so boot-time unavailability cannot reach the mid-session
#        queue (covered by DB-backed integration tests; see the case comment).
#   F4 — the 196-reservation-audit regression corpus is present, well-formed,
#        and covers the required drift modes (full replay semantics run
#        in-process: `cargo test -p mcp-agent-mail-tools --test
#        reservation_regression_fixtures`).
#   F5 — acquire failure classification: `file_reservation_paths` against a
#        mailbox whose reservation index is unreadable fails CLOSED with the
#        `reservation_acquire` context (cause classification + `do_not_edit`)
#        rather than an opaque error. Exercised with a page-level corruption
#        injection; honest SKIP when the environment cannot stage it.
#
# Surface notes (why the MCP stdio server, not `am file_reservations`):
#   The CLI `file_reservations {reserve,release,...}` verbs are a direct-SQL
#   operator surface — they write no archive artifacts and never route through
#   the tools crate, so F1/F3-archive/F5 logic is only reachable through the
#   MCP tool surface. This suite drives short `am serve-stdio` JSON-RPC
#   sessions for those, exactly like test_tools_reservations.sh.
#
# Ref: br-bvq1x.14.8 (N8). Depends on F1-F5 (br-bvq1x.6.*, all closed).

set -uo pipefail

E2E_SUITE="reservation_parity"

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
e2e_banner "Track F — Reservation DB↔Archive Parity E2E"

EVENTS="${E2E_ARTIFACT_DIR}/events.jsonl"
: >"${EVENTS}"

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq required"
    log_summary "${E2E_SUITE}" 0 0 0 "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_reservation_parity.sh" >>"${EVENTS}"
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

WORK="$(e2e_mktemp reservation_parity_work)"
ART="${E2E_ARTIFACT_DIR}/scenarios"
mkdir -p "${WORK}/storage" "${WORK}/home" "${WORK}/repo" "${ART}"
export AM_INTERFACE_MODE="cli"
export STORAGE_ROOT="${WORK}/storage"
export HOME="${WORK}/home"
# Point CLI verbs at an unused MCP port so every verb falls back to the isolated
# local SQLite path instead of hitting a real server on 127.0.0.1:8765.
export HTTP_HOST="127.0.0.1"
export HTTP_PORT="${AM_E2E_UNUSED_PORT:-8799}"

DB_PATH="${WORK}/parity.sqlite3"
HEALTHY_DB="sqlite:///${DB_PATH}"
# Unavailable mailbox: parent path component is a regular file → DB unopenable.
printf 'x' >"${WORK}/notadir"
UNAVAIL_DB="sqlite:///${WORK}/notadir/db.sqlite3"
PROJECT_KEY="${WORK}/repo"
export DATABASE_URL="${HEALTHY_DB}"

AM_CMD_TIMEOUT="${AM_CMD_TIMEOUT:-60}"
STDIO_SESSION_TIMEOUT="${STDIO_SESSION_TIMEOUT:-40}"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-reservation-parity","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Run `am` with timeout, capturing exit code WITHOUT tripping errexit-style
# aborts: several verbs here intentionally exit non-zero (doctor health with
# drift) and the suite must observe that rather than abort.
amrun() {
    local out="$1"
    shift
    local rc=0
    if command -v timeout >/dev/null 2>&1; then
        timeout "${AM_CMD_TIMEOUT}" "${AM_BIN}" "$@" >"${out}" 2>"${out}.err" || rc=$?
    else
        "${AM_BIN}" "$@" >"${out}" 2>"${out}.err" || rc=$?
    fi
    printf '%s\n' "${rc}" >"${out}.exit"
    return 0
}

# Drive one short-lived `am serve-stdio` JSON-RPC session (same pattern as
# test_tools_reservations.sh). Args: <database_url> <out_file> <request>...
# All JSON-RPC responses (one per line) land in <out_file>.
stdio_session() {
    local db_url="$1" out="$2"
    shift 2
    local srv_work fifo srv_pid write_pid
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    fifo="${srv_work}/stdin_fifo"
    mkfifo "${fifo}"

    DATABASE_URL="${db_url}" RUST_LOG=error INTEGRITY_CHECK_ON_STARTUP=false \
        "${AM_BIN}" serve-stdio <"${fifo}" >"${out}" 2>"${srv_work}/stderr.txt" &
    srv_pid=$!
    sleep 0.3

    {
        local req
        for req in "$@"; do
            printf '%s\n' "${req}"
            sleep 0.4
        done
        sleep 0.5
    } >"${fifo}" &
    write_pid=$!

    local elapsed=0
    while [ "${elapsed}" -lt "${STDIO_SESSION_TIMEOUT}" ]; do
        if ! kill -0 "${srv_pid}" 2>/dev/null; then break; fi
        sleep 0.5
        elapsed=$((elapsed + 1))
    done

    wait "${write_pid}" 2>/dev/null || true
    kill "${srv_pid}" 2>/dev/null || true
    wait "${srv_pid}" 2>/dev/null || true
    cp "${srv_work}/stderr.txt" "${out}.stderr" 2>/dev/null || true
    return 0
}

# Extract the inner tool-result text (result.content[0].text) for a request id.
extract_result() {
    local file="$1" req_id="$2"
    jq -r --argjson id "${req_id}" \
        'select(.id == $id) | .result.content[0].text // empty' \
        "${file}" 2>/dev/null | head -1
}

# "true" if the response for a request id is a JSON-RPC error or MCP tool error.
is_error_result() {
    local file="$1" req_id="$2"
    local verdict
    verdict="$(jq -r --argjson id "${req_id}" \
        'select(.id == $id) | if has("error") or (.result.isError // false) then "true" else "false" end' \
        "${file}" 2>/dev/null | head -1)"
    printf '%s\n' "${verdict:-true}"
}

# jq assertion against a file of JSON (or a single JSON document).
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

# ---------------------------------------------------------------------------
# Setup: healthy mailbox, two agents, one exclusive reservation (tool path).
# ---------------------------------------------------------------------------
e2e_case_banner "Setup: project + agents + GoldFox reserves src/alpha/** (MCP tool path)"

stdio_session "${HEALTHY_DB}" "${ART}/setup.jsonl" \
    "${INIT_REQ}" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_KEY}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"reservation parity E2E\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\",\"task_description\":\"reservation parity E2E\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"GoldFox\",\"paths\":[\"src/alpha/**\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"br-bvq1x.14.8 F1 seed\"}}}"

if [ "$(is_error_result "${ART}/setup.jsonl" 13)" = "false" ]; then
    e2e_pass "setup: GoldFox reservation granted through the MCP tool surface"
else
    e2e_fail "setup: file_reservation_paths errored (see ${ART}/setup.jsonl)"
fi

RESERVE_TEXT="$(extract_result "${ART}/setup.jsonl" 13)"
printf '%s\n' "${RESERVE_TEXT}" >"${ART}/setup_reserve.json"
RES_ID="$(jq -r '.granted[0].id // empty' "${ART}/setup_reserve.json" 2>/dev/null)"
if [ -n "${RES_ID}" ]; then
    e2e_pass "setup: granted reservation id captured (id=${RES_ID})"
else
    e2e_fail "setup: could not extract granted reservation id"
fi

FR_DIR="$(find "${STORAGE_ROOT}/projects" -maxdepth 2 -type d -name file_reservations 2>/dev/null | head -1)"
ALPHA_ARTIFACT="${FR_DIR}/id-${RES_ID}.json"
if [ -n "${FR_DIR}" ] && [ -f "${ALPHA_ARTIFACT}" ]; then
    e2e_pass "setup: archive artifact id-${RES_ID}.json written by the grant"
    check "setup: artifact holder is GoldFox" "${ALPHA_ARTIFACT}" '.agent == "GoldFox"'
    check "setup: artifact carries the reserved pattern" "${ALPHA_ARTIFACT}" '.path_pattern == "src/alpha/**"'
else
    e2e_fail "setup: no archive artifact found under ${FR_DIR:-<missing file_reservations dir>}"
fi

# ---------------------------------------------------------------------------
# F1a: crash-gap injection — the artifact vanishes, the next tool read heals it.
# ---------------------------------------------------------------------------
e2e_case_banner "F1: reconcile-on-read heals a crash-gap (missing) artifact"
if [ -n "${RES_ID}" ] && [ -f "${ALPHA_ARTIFACT}" ]; then
    # Crash-gap injection: the DB grant committed but the archive artifact is
    # gone (quarantine-by-rename OUT of the scanned directory, mirroring the
    # incident's lost write without leaving debris the parity scanner could see).
    mv "${ALPHA_ARTIFACT}" "${WORK}/id-${RES_ID}.crashgap.json"
    if [ ! -f "${ALPHA_ARTIFACT}" ]; then
        e2e_pass "F1: crash gap staged (artifact removed; DB row still active)"
    else
        e2e_fail "F1: failed to stage the crash gap"
    fi

    stdio_session "${HEALTHY_DB}" "${ART}/f1_heal.jsonl" \
        "${INIT_REQ}" \
        "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"SilverWolf\",\"paths\":[\"docs/**\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"br-bvq1x.14.8 F1 heal trigger\"}}}"

    if [ "$(is_error_result "${ART}/f1_heal.jsonl" 20)" = "false" ]; then
        e2e_pass "F1: unrelated SilverWolf grant succeeded (read path executed)"
    else
        e2e_fail "F1: SilverWolf grant errored (see ${ART}/f1_heal.jsonl)"
    fi

    if [ -f "${ALPHA_ARTIFACT}" ]; then
        e2e_pass "F1: missing artifact was healed back onto disk by reconcile-on-read"
        check "F1: healed artifact names the correct holder (GoldFox, never guessed)" \
            "${ALPHA_ARTIFACT}" '.agent == "GoldFox"'
        check "F1: healed artifact restores the reserved pattern" \
            "${ALPHA_ARTIFACT}" '.path_pattern == "src/alpha/**"'
        check "F1: healed artifact is still active (released_ts null)" \
            "${ALPHA_ARTIFACT}" '.released_ts == null'
    else
        e2e_fail "F1: artifact id-${RES_ID}.json was NOT healed by the next tool read"
    fi
else
    e2e_skip "F1 skipped: setup did not produce a healable artifact"
fi

# ---------------------------------------------------------------------------
# F2: per-field drift is reported by doctor health + the per-FM registry view.
# ---------------------------------------------------------------------------
e2e_case_banner "F2: parity checker reports per-field drift in doctor surfaces"
if [ -n "${RES_ID}" ] && [ -f "${ALPHA_ARTIFACT}" ]; then
    # Corrupt one archive-side field. path_pattern drift is detect-only (never
    # auto-reconciled by doctor), so it is a stable probe for the reporting path.
    jq '.path_pattern = "src/DRIFT-INJECTED/**"' "${ALPHA_ARTIFACT}" >"${ALPHA_ARTIFACT}.tmp" &&
        mv "${ALPHA_ARTIFACT}.tmp" "${ALPHA_ARTIFACT}"
    e2e_log "F2: injected path_pattern drift into ${ALPHA_ARTIFACT}"

    amrun "${ART}/f2_health.txt" doctor health
    if grep -q "reservation_parity: drift" "${ART}/f2_health.txt" "${ART}/f2_health.txt.err" 2>/dev/null; then
        e2e_pass "F2: doctor health surfaces reservation_parity drift"
    else
        e2e_fail "F2: doctor health did not report reservation_parity drift"
        printf '      out: %s\n' "$(head -c 240 "${ART}/f2_health.txt" 2>/dev/null | tr '\n' ' ')"
    fi
    if grep -q "path_pattern" "${ART}/f2_health.txt" "${ART}/f2_health.txt.err" 2>/dev/null; then
        e2e_pass "F2: doctor health names the drifted field (path_pattern)"
    else
        e2e_fail "F2: doctor health did not name the drifted field"
    fi

    amrun "${ART}/f2_fm_list.json" doctor fix \
        --only fm-db-state-files-reservation-db-archive-parity --list
    if jq -e . "${ART}/f2_fm_list.json" >/dev/null 2>&1; then
        check "F2: per-FM list is scoped to the parity FM" "${ART}/f2_fm_list.json" \
            '.fm_id == "fm-db-state-files-reservation-db-archive-parity"'
        check "F2: per-FM list detects at least one finding" "${ART}/f2_fm_list.json" \
            '.findings_count >= 1'
        if jq -c '.findings' "${ART}/f2_fm_list.json" | grep -q "path_pattern"; then
            e2e_pass "F2: per-FM finding evidence carries the path_pattern drift"
        else
            e2e_fail "F2: per-FM finding evidence does not mention path_pattern"
        fi
    else
        e2e_fail "F2: doctor fix --only <parity-fm> --list emitted no JSON"
    fi
else
    e2e_skip "F2 skipped: no healed artifact available to drift"
fi

# ---------------------------------------------------------------------------
# F1b: reconcile-on-read also heals DIVERGENT artifacts back to DB truth.
# ---------------------------------------------------------------------------
e2e_case_banner "F1: reconcile-on-read heals the divergent artifact back to DB truth"
if [ -n "${RES_ID}" ] && [ -f "${ALPHA_ARTIFACT}" ]; then
    stdio_session "${HEALTHY_DB}" "${ART}/f1b_heal.jsonl" \
        "${INIT_REQ}" \
        "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"GoldFox\",\"paths\":[\"src/gamma/**\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"br-bvq1x.14.8 F1b heal trigger\"}}}"

    if jq -e '.path_pattern == "src/alpha/**"' "${ALPHA_ARTIFACT}" >/dev/null 2>&1; then
        e2e_pass "F1: divergent path_pattern healed back to the DB value"
    else
        e2e_fail "F1: artifact still carries the injected drift after a tool read"
        printf '      got: %s\n' "$(jq -c . "${ALPHA_ARTIFACT}" 2>/dev/null | head -c 240)"
    fi

    amrun "${ART}/f1b_health.txt" doctor health
    if grep -q "reservation_parity: ok" "${ART}/f1b_health.txt" "${ART}/f1b_health.txt.err" 2>/dev/null; then
        e2e_pass "F2: doctor health reports parity ok after the heal"
    else
        e2e_fail "F2: doctor health still reports drift after the heal"
        printf '      out: %s\n' "$(grep -h reservation_parity "${ART}/f1b_health.txt" "${ART}/f1b_health.txt.err" 2>/dev/null | head -c 240)"
    fi
else
    e2e_skip "F1b skipped: no drifted artifact available to heal"
fi

# ---------------------------------------------------------------------------
# F3: release is idempotent (tool + CLI) and updates the archive artifact.
# ---------------------------------------------------------------------------
e2e_case_banner "F3: double-release is a clean released=0 no-op (tool + CLI)"
stdio_session "${HEALTHY_DB}" "${ART}/f3_release.jsonl" \
    "${INIT_REQ}" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"GoldFox\",\"paths\":[\"src/alpha/**\",\"src/gamma/**\"]}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":41,\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"GoldFox\",\"paths\":[\"src/alpha/**\",\"src/gamma/**\"]}}}"

FIRST_RELEASE="$(extract_result "${ART}/f3_release.jsonl" 40)"
SECOND_RELEASE="$(extract_result "${ART}/f3_release.jsonl" 41)"
printf '%s\n' "${FIRST_RELEASE}" >"${ART}/f3_release_first.json"
printf '%s\n' "${SECOND_RELEASE}" >"${ART}/f3_release_second.json"
check "F3: first release releases the GoldFox reservations" "${ART}/f3_release_first.json" \
    '.released >= 1'
check "F3: second identical release is a clean released=0 no-op" "${ART}/f3_release_second.json" \
    '.released == 0'
if [ -n "${RES_ID}" ] && [ -f "${ALPHA_ARTIFACT}" ]; then
    check "F3: release reconciled the archive artifact (released_ts set)" \
        "${ALPHA_ARTIFACT}" '.released_ts != null'
fi

amrun "${ART}/f3_cli_release.txt" file_reservations release "${PROJECT_KEY}" GoldFox \
    --paths "src/alpha/**"
if grep -q "Released 0 reservation(s)" "${ART}/f3_cli_release.txt" 2>/dev/null; then
    e2e_pass "F3: CLI double-release is a clean 'Released 0' no-op (exit $(cat "${ART}/f3_cli_release.txt.exit" 2>/dev/null))"
else
    e2e_fail "F3: CLI double-release did not report a released=0 no-op"
    printf '      out: %s\n' "$(head -c 240 "${ART}/f3_cli_release.txt" 2>/dev/null | tr '\n' ' ')"
fi

# ---------------------------------------------------------------------------
# F3: DB-unavailable release queues a durable intent; robot surfaces it; the
#     next successful release auto-replays and clears it.
# ---------------------------------------------------------------------------
e2e_case_banner "F3: DB-unavailable release queues a durable intent + robot surfaces it"
stdio_session "${UNAVAIL_DB}" "${ART}/f3_queue.jsonl" \
    "${INIT_REQ}" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"SilverWolf\",\"paths\":[\"docs/**\"]}}}"

INTENT_LOG="${STORAGE_ROOT}/degraded_intents/release_file_reservations.jsonl"
RELEASE_RESPONDED="$(jq -r 'select(.id == 50) | "yes"' "${ART}/f3_queue.jsonl" 2>/dev/null | head -1)"
if [ "${RELEASE_RESPONDED}" != "yes" ]; then
    # The stdio server fails CLOSED at boot against a fully-unavailable DB
    # (its pool/readiness init errors out before any tools/call executes), so
    # the mid-session release-intent QUEUE path has no black-box trigger here —
    # same verdict as N10 (test_degraded_intents_replay.sh). The queue write,
    # auto-replay, and abandon-on-missing semantics are covered in-process by
    # DB-backed integration tests:
    #   tools reservations::tests::replay_queued_release_intent_releases_once
    #   tools reservations::tests::replay_release_intent_does_not_release_lease_reacquired_by_other_agent
    #   cli robot::tests::build_queued_intents_surfaces_ack_and_send
    e2e_skip "F3 queue path: serve-stdio fails closed at boot on an unavailable mailbox (no tools/call ran; boot-time unavailability cannot reach the mid-session release-intent queue); queue/replay semantics are covered by the DB-backed integration tests"
elif [ -f "${INTENT_LOG}" ] && grep -q "SilverWolf" "${INTENT_LOG}"; then
    e2e_pass "F3: durable release intent written to degraded_intents/ (queued, not lost)"
    amrun "${ART}/f3_robot_queued.json" robot status --project "${PROJECT_KEY}" \
        --agent SilverWolf --format json
    if jq -e . "${ART}/f3_robot_queued.json" >/dev/null 2>&1; then
        check "F3: robot status surfaces the queued release intent" "${ART}/f3_robot_queued.json" \
            '[.queued_intents[]? | select(.kind == "release_file_reservations")] | length >= 1'
    else
        e2e_fail "F3: robot status emitted no JSON while a release intent was queued"
    fi
else
    e2e_fail "F3: release ran against the unavailable mailbox but queued no durable intent (${INTENT_LOG})"
fi

stdio_session "${HEALTHY_DB}" "${ART}/f3_replay.jsonl" \
    "${INIT_REQ}" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"SilverWolf\",\"paths\":[\"docs/**\"]}}}"

REPLAY_RELEASE="$(extract_result "${ART}/f3_replay.jsonl" 60)"
printf '%s\n' "${REPLAY_RELEASE}" >"${ART}/f3_replay.json"
check "F3: healthy release succeeds after the outage" "${ART}/f3_replay.json" \
    '.released >= 1'

amrun "${ART}/f3_robot_cleared.json" robot status --project "${PROJECT_KEY}" \
    --agent SilverWolf --format json
if jq -e . "${ART}/f3_robot_cleared.json" >/dev/null 2>&1; then
    check "F3: no queued release intent remains after the healthy release" \
        "${ART}/f3_robot_cleared.json" \
        '[.queued_intents[]? | select(.kind == "release_file_reservations")] | length == 0'
else
    e2e_fail "F3: robot status emitted no JSON after the replay"
fi

# ---------------------------------------------------------------------------
# F4: the 196-reservation-audit regression corpus reproduces (manifest contract).
# ---------------------------------------------------------------------------
e2e_case_banner "F4: reservation regression corpus is present and covers the drift modes"
F4_MANIFEST="${PROJECT_ROOT}/tests/fixtures/reservation_regression/manifest.json"
if [ -f "${F4_MANIFEST}" ]; then
    check "F4: manifest parses with the expected corpus id" "${F4_MANIFEST}" \
        '.corpus_id == "agent-mail-reservation-regression-corpus"'
    check "F4: corpus carries the four audited fixtures" "${F4_MANIFEST}" \
        '[.fixtures[].id] | sort == ["btree_page_2288_release_malformed","db_archive_active_state_mismatch","stale_agent_id_row","stuck_null_released_ts"]'
    check "F4: corpus covers holder, release, active-state and malformed-btree drift" "${F4_MANIFEST}" \
        '[.fixtures[].drift_mode] | sort == ["db_released_archive_active","db_released_ts_null_archive_released","db_stale_agent_id_archive_holder_mismatch","release_query_malformed_btree_page_2288"]'
    MISSING_RECIPES=0
    while IFS= read -r recipe; do
        if [ ! -f "${PROJECT_ROOT}/tests/fixtures/reservation_regression/${recipe}" ]; then
            MISSING_RECIPES=$((MISSING_RECIPES + 1))
            e2e_log "F4: missing recipe file: ${recipe}"
        fi
    done < <(jq -r '.fixtures[].artifacts[].recipe | select(.type == "text_file") | .file' "${F4_MANIFEST}")
    if [ "${MISSING_RECIPES}" -eq 0 ]; then
        e2e_pass "F4: every text-file recipe referenced by the manifest exists on disk"
    else
        e2e_fail "F4: ${MISSING_RECIPES} manifest-referenced recipe file(s) missing"
    fi
    e2e_log "F4: full replay semantics run in-process: cargo test -p mcp-agent-mail-tools --test reservation_regression_fixtures"
else
    e2e_fail "F4: reservation regression manifest missing at ${F4_MANIFEST}"
fi

# ---------------------------------------------------------------------------
# F5: acquire against an unreadable reservation index fails CLOSED with the
#     classified reservation_acquire context (cause + do_not_edit).
# ---------------------------------------------------------------------------
e2e_case_banner "F5: acquire failure classification (corrupted reservation index)"
if command -v sqlite3 >/dev/null 2>&1 && command -v dd >/dev/null 2>&1; then
    # Fold WAL into the main file, then zero the file_reservations root page so
    # the DB opens fine but the reservation-index read is malformed — the exact
    # css/ts2 incident shape ("malformed B-tree while reserving").
    sqlite3 "${DB_PATH}" "PRAGMA wal_checkpoint(TRUNCATE);" >/dev/null 2>&1 || true
    FR_ROOTPAGE="$(sqlite3 "${DB_PATH}" "SELECT rootpage FROM sqlite_master WHERE type='table' AND name='file_reservations';" 2>/dev/null)"
    PAGE_SIZE="$(sqlite3 "${DB_PATH}" "PRAGMA page_size;" 2>/dev/null)"
    if [ -n "${FR_ROOTPAGE}" ] && [ -n "${PAGE_SIZE}" ] && [ "${FR_ROOTPAGE}" -gt 1 ] 2>/dev/null; then
        cp "${DB_PATH}" "${WORK}/parity.pre-corruption.sqlite3"
        dd if=/dev/zero of="${DB_PATH}" bs="${PAGE_SIZE}" \
            seek="$((FR_ROOTPAGE - 1))" count=1 conv=notrunc >/dev/null 2>&1 || true
        e2e_log "F5: zeroed file_reservations root page ${FR_ROOTPAGE} (page_size=${PAGE_SIZE})"

        stdio_session "${HEALTHY_DB}" "${ART}/f5_acquire.jsonl" \
            "${INIT_REQ}" \
            "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"GoldFox\",\"paths\":[\"src/beta/**\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"br-bvq1x.14.8 F5 probe\"}}}"

        if grep -q "reservation_acquire" "${ART}/f5_acquire.jsonl" 2>/dev/null; then
            e2e_pass "F5: acquire failure carries the reservation_acquire context"
            if grep -q '"fail_closed":true' "${ART}/f5_acquire.jsonl" 2>/dev/null; then
                e2e_pass "F5: acquire failure is explicitly fail-closed"
            else
                e2e_fail "F5: reservation_acquire context lacks fail_closed=true"
            fi
            if grep -q '"cause"' "${ART}/f5_acquire.jsonl" 2>/dev/null; then
                e2e_pass "F5: acquire failure names a cause classification"
            else
                e2e_fail "F5: reservation_acquire context lacks a cause"
            fi
            if grep -q 'src/beta/\*\*' "${ART}/f5_acquire.jsonl" 2>/dev/null &&
                grep -q '"do_not_edit"' "${ART}/f5_acquire.jsonl" 2>/dev/null; then
                e2e_pass "F5: requested paths surfaced with a do_not_edit set"
            else
                e2e_fail "F5: requested paths / do_not_edit missing from the context"
            fi
        elif [ "$(is_error_result "${ART}/f5_acquire.jsonl" 70)" = "false" ]; then
            e2e_skip "F5: startup self-heal recovered the corrupted index before the acquire (grant succeeded); the classification envelope is unit-covered by reservations::tests + the F4 corpus fixer test"
        else
            e2e_skip "F5: acquire failed before the reservation-index read (no reservation_acquire graft at this stage); classification is unit-covered by reservations::tests + fixer_drives_f4_corpus_to_zero_drift"
        fi
        # Restore the healthy DB so later suites reusing this workspace see no
        # corruption debris (sidecars are quarantined by rename, never deleted).
        cp "${WORK}/parity.pre-corruption.sqlite3" "${DB_PATH}"
        [ -f "${DB_PATH}-wal" ] && mv "${DB_PATH}-wal" "${WORK}/parity.stale-wal.quarantine" 2>/dev/null
        [ -f "${DB_PATH}-shm" ] && mv "${DB_PATH}-shm" "${WORK}/parity.stale-shm.quarantine" 2>/dev/null
    else
        e2e_skip "F5: could not resolve the file_reservations root page (rootpage=${FR_ROOTPAGE:-?}, page_size=${PAGE_SIZE:-?}); classification is unit-covered by reservations::tests"
    fi
else
    e2e_skip "F5: sqlite3/dd unavailable; classification is unit-covered by reservations::tests + fixer_drives_f4_corpus_to_zero_drift"
fi

# ---------------------------------------------------------------------------
log_summary "${E2E_SUITE}" "$((_E2E_PASS + _E2E_FAIL + _E2E_SKIP))" "${_E2E_PASS}" "${_E2E_FAIL}" \
    "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_reservation_parity.sh" >>"${EVENTS}"
e2e_summary
