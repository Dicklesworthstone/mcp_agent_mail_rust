#!/usr/bin/env bash
# test_golden.sh - E2E suite for `am golden` capture/verify/list flows (br-2cdp2)

set -euo pipefail

E2E_SUITE="golden"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Golden E2E Test Suite"

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq required"
    e2e_summary
    exit 0
fi

AM_BIN="$(e2e_ensure_binary "am")"
e2e_log "am binary: ${AM_BIN}"

WORK="$(e2e_mktemp "e2e_golden")"
GOLDEN_A="${WORK}/golden_a"
GOLDEN_B="${WORK}/golden_b"
GOLDEN_C="${WORK}/golden_c"
mkdir -p "${GOLDEN_A}" "${GOLDEN_B}" "${GOLDEN_C}"

run_golden() {
    local out_file="$1"
    shift
    set +e
    AM_INTERFACE_MODE=cli "${AM_BIN}" golden "$@" >"${out_file}" 2>&1
    local rc=$?
    set -e
    return "$rc"
}

# ---------------------------------------------------------------------------
# Case 1: capture (filtered)
# ---------------------------------------------------------------------------

e2e_case_banner "golden capture creates artifacts + checksums"
CAPTURE_OUT="${WORK}/capture_help.out"
if run_golden "${CAPTURE_OUT}" capture --dir "${GOLDEN_A}" --filter "am_help.txt" --json; then
    CAPTURE_RC=0
else
    CAPTURE_RC=$?
fi
e2e_save_artifact "case_capture_help.out" "$(cat "${CAPTURE_OUT}")"
e2e_assert_exit_code "capture --filter am_help.txt" "0" "${CAPTURE_RC}"

e2e_assert_file_exists "am_help.txt captured" "${GOLDEN_A}/am_help.txt"
e2e_assert_file_exists "checksums.sha256 created" "${GOLDEN_A}/checksums.sha256"

TXT_COUNT="$(find "${GOLDEN_A}" -maxdepth 1 -name '*.txt' | wc -l | tr -d ' ')"
e2e_assert_eq "only one .txt captured with exact filter" "1" "${TXT_COUNT}"

if grep -Eq '^[0-9a-f]{64}  am_help\.txt$' "${GOLDEN_A}/checksums.sha256"; then
    e2e_pass "checksums.sha256 line format"
else
    e2e_fail "checksums.sha256 line format"
    e2e_save_artifact "case_checksums_invalid.txt" "$(cat "${GOLDEN_A}/checksums.sha256")"
fi

# ---------------------------------------------------------------------------
# Case 2: verify pass
# ---------------------------------------------------------------------------

e2e_case_banner "golden verify passes immediately after capture"
VERIFY_PASS_OUT="${WORK}/verify_pass.out"
if run_golden "${VERIFY_PASS_OUT}" verify --dir "${GOLDEN_A}" --filter "am_help.txt" --json; then
    VERIFY_PASS_RC=0
else
    VERIFY_PASS_RC=$?
fi
e2e_save_artifact "case_verify_pass.out" "$(cat "${VERIFY_PASS_OUT}")"
e2e_assert_exit_code "verify pass exit" "0" "${VERIFY_PASS_RC}"

VERIFY_FAILED="$(jq -r '.failed' "${VERIFY_PASS_OUT}" 2>/dev/null || echo "parse_error")"
VERIFY_STATUS="$(jq -r '.rows[0].status' "${VERIFY_PASS_OUT}" 2>/dev/null || echo "parse_error")"
e2e_assert_eq "verify pass failed count" "0" "${VERIFY_FAILED}"
e2e_assert_eq "verify pass row status" "ok" "${VERIFY_STATUS}"

# ---------------------------------------------------------------------------
# Case 3: verify fail after tamper
# ---------------------------------------------------------------------------

e2e_case_banner "golden verify fails with mismatch + diff after tamper"
printf '\nTAMPERED\n' >> "${GOLDEN_A}/am_help.txt"

VERIFY_FAIL_OUT="${WORK}/verify_fail.out"
if run_golden "${VERIFY_FAIL_OUT}" verify --dir "${GOLDEN_A}" --filter "am_help.txt" --json; then
    VERIFY_FAIL_RC=0
else
    VERIFY_FAIL_RC=$?
fi
e2e_save_artifact "case_verify_fail.out" "$(cat "${VERIFY_FAIL_OUT}")"
e2e_assert_exit_code "verify fail exit" "1" "${VERIFY_FAIL_RC}"

FAIL_STATUS="$(jq -r '.rows[0].status' "${VERIFY_FAIL_OUT}" 2>/dev/null || echo "parse_error")"
FAIL_DIFF="$(jq -r '.rows[0].diff // ""' "${VERIFY_FAIL_OUT}" 2>/dev/null || true)"
e2e_assert_eq "verify fail row status" "mismatch" "${FAIL_STATUS}"
e2e_assert_contains "verify fail contains inline diff marker" "${FAIL_DIFF}" "@@ mismatch around line"

# ---------------------------------------------------------------------------
# Case 4: list states
# ---------------------------------------------------------------------------

e2e_case_banner "golden list reports stale then missing"
LIST_STALE_OUT="${WORK}/list_stale.out"
if run_golden "${LIST_STALE_OUT}" list --dir "${GOLDEN_A}" --filter "am_help.txt" --json; then
    LIST_STALE_RC=0
else
    LIST_STALE_RC=$?
fi
e2e_assert_exit_code "list stale exit" "0" "${LIST_STALE_RC}"
LIST_STALE_STATUS="$(jq -r '.rows[0].status' "${LIST_STALE_OUT}" 2>/dev/null || echo "parse_error")"
e2e_assert_eq "list stale status" "stale" "${LIST_STALE_STATUS}"

rm -f "${GOLDEN_A}/am_help.txt"
LIST_MISSING_OUT="${WORK}/list_missing.out"
if run_golden "${LIST_MISSING_OUT}" list --dir "${GOLDEN_A}" --filter "am_help.txt" --json; then
    LIST_MISSING_RC=0
else
    LIST_MISSING_RC=$?
fi
e2e_assert_exit_code "list missing exit" "0" "${LIST_MISSING_RC}"
LIST_MISSING_STATUS="$(jq -r '.rows[0].status' "${LIST_MISSING_OUT}" 2>/dev/null || echo "parse_error")"
e2e_assert_eq "list missing status" "missing" "${LIST_MISSING_STATUS}"

# ---------------------------------------------------------------------------
# Case 5: MCP denial capture
# ---------------------------------------------------------------------------

e2e_case_banner "golden capture includes deterministic MCP denial output"
MCP_CAPTURE_OUT="${WORK}/capture_mcp_deny.out"
if run_golden "${MCP_CAPTURE_OUT}" capture --dir "${GOLDEN_B}" --filter "mcp_deny_share.txt" --json; then
    MCP_CAPTURE_RC=0
else
    MCP_CAPTURE_RC=$?
fi
e2e_save_artifact "case_capture_mcp_deny.out" "$(cat "${MCP_CAPTURE_OUT}")"
e2e_assert_exit_code "capture mcp_deny_share" "0" "${MCP_CAPTURE_RC}"
e2e_assert_file_exists "mcp_deny_share.txt captured" "${GOLDEN_B}/mcp_deny_share.txt"
MCP_DENIAL_TEXT="$(cat "${GOLDEN_B}/mcp_deny_share.txt" 2>/dev/null || true)"
e2e_assert_contains "mcp denial text contract" "${MCP_DENIAL_TEXT}" "not an MCP server command"

# ---------------------------------------------------------------------------
# Case 6: Stub encoder capture (if present)
# ---------------------------------------------------------------------------

e2e_case_banner "golden capture includes stub encoder output"
if [ -x "${E2E_PROJECT_ROOT}/scripts/toon_stub_encoder.sh" ]; then
    STUB_CAPTURE_OUT="${WORK}/capture_stub.out"
    if run_golden "${STUB_CAPTURE_OUT}" capture --dir "${GOLDEN_C}" --filter "stub_encode_stats_stderr.txt" --json; then
        STUB_CAPTURE_RC=0
    else
        STUB_CAPTURE_RC=$?
    fi
    e2e_save_artifact "case_capture_stub.out" "$(cat "${STUB_CAPTURE_OUT}")"
    e2e_assert_exit_code "capture stub stderr" "0" "${STUB_CAPTURE_RC}"
    e2e_assert_file_exists "stub_encode_stats_stderr captured" "${GOLDEN_C}/stub_encode_stats_stderr.txt"
    STUB_TEXT="$(cat "${GOLDEN_C}/stub_encode_stats_stderr.txt" 2>/dev/null || true)"
    e2e_assert_contains "stub stats text present" "${STUB_TEXT}" "Token estimates"
else
    e2e_skip "toon_stub_encoder.sh not executable"
fi

# ---------------------------------------------------------------------------
# Case 7: normalization invariants
# ---------------------------------------------------------------------------

e2e_case_banner "captured outputs are normalized (no ANSI / ISO timestamps / pid=123)"
NORM_DIR="${WORK}/norm"
mkdir -p "${NORM_DIR}"
NORM_OUT="${WORK}/capture_norm.out"
if run_golden "${NORM_OUT}" capture --dir "${NORM_DIR}" --filter "am_help.txt" --json; then
    NORM_RC=0
else
    NORM_RC=$?
fi
e2e_assert_exit_code "capture for normalization check" "0" "${NORM_RC}"
NORM_TEXT="$(cat "${NORM_DIR}/am_help.txt" 2>/dev/null || true)"

if printf '%s' "${NORM_TEXT}" | grep -Eq $'\x1b\[[0-9;]*m'; then
    e2e_fail "ANSI escape sequences should be stripped"
else
    e2e_pass "ANSI escape sequences stripped"
fi
if printf '%s' "${NORM_TEXT}" | grep -Eq '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.Z+-]+'; then
    e2e_fail "ISO-8601 timestamps should be normalized"
else
    e2e_pass "ISO-8601 timestamps normalized"
fi
if printf '%s' "${NORM_TEXT}" | grep -Eq 'pid=[0-9]+'; then
    e2e_fail "pid=### should be normalized"
else
    e2e_pass "pid=### normalized"
fi

# ---------------------------------------------------------------------------
# Finish
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1 || true)"
if e2e_summary; then
    exit 0
else
    exit 1
fi
