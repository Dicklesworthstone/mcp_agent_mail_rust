#!/usr/bin/env bash
# e2e_share_verify_live.sh - Dedicated E2E matrix for `am share deploy verify-live`
#
# Run via (authoritative):
#   am e2e run --project . share_verify_live
# Compatibility fallback:
#   AM_E2E_FORCE_LEGACY=1 ./scripts/e2e_test.sh share_verify_live
#
# Coverage:
# - Success path with local+remote+security checks
# - Warning path (header misconfiguration) with strict/non-strict exit semantics
# - Failure matrix: partial deploy, timeout, redirect loop, connection refused
# - Fail-fast short-circuit behavior for local errors
# - Compatibility wrapper behavior (validate_deploy.sh -> native verify-live)

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-share_verify_live}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Share Deploy Verify-Live E2E Matrix"
e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

for cmd in python3 sqlite3; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

AM_BIN="$(e2e_ensure_binary "am" | tail -n 1)"
e2e_log "am binary: ${AM_BIN}"

if ! "${AM_BIN}" share deploy verify-live --help >/dev/null 2>&1; then
    e2e_log "verify-live subcommand unavailable in ${AM_BIN} (stale binary or build blockers)"
    e2e_skip "am share deploy verify-live unavailable"
    e2e_summary
    exit 0
fi

WORK="$(e2e_mktemp "e2e_share_verify_live")"
DB_PATH="${WORK}/storage.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
mkdir -p "${STORAGE_ROOT}"
sqlite3 "${DB_PATH}" "PRAGMA journal_mode=WAL;" >/dev/null 2>&1 || true

e2e_log "Workspace: ${WORK}"
e2e_log "DB: ${DB_PATH}"
e2e_log "Storage root: ${STORAGE_ROOT}"

am_env() {
    DATABASE_URL="sqlite:////${DB_PATH}" \
    STORAGE_ROOT="${STORAGE_ROOT}" \
    "$@"
}

now_ms() {
    python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
}

reserve_port() {
    python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

write_bundle_fixture() {
    local bundle_dir="$1"
    local root_label="$2"
    mkdir -p "${bundle_dir}/viewer/vendor" "${bundle_dir}/viewer/data" "${bundle_dir}/viewer/pages"

    cat > "${bundle_dir}/manifest.json" <<JSON
{
  "schema_version": "1.0.0",
  "generated_at": "2026-02-12T00:00:00Z",
  "database": {"path": "mailbox.sqlite3"},
  "export_config": {"scrub_preset": "archive"},
  "label": "${root_label}"
}
JSON

    cat > "${bundle_dir}/index.html" <<HTML
<!doctype html>
<html>
  <head><meta charset="utf-8"><title>${root_label}</title></head>
  <body><main>${root_label}</main></body>
</html>
HTML

    cat > "${bundle_dir}/viewer/index.html" <<HTML
<!doctype html>
<html>
  <head><meta charset="utf-8"><title>Viewer ${root_label}</title><link rel="stylesheet" href="/viewer/styles.css"></head>
  <body><h1>Viewer ${root_label}</h1></body>
</html>
HTML

    cat > "${bundle_dir}/viewer/styles.css" <<'CSS'
body { font-family: monospace; background: #fafafa; color: #222; }
CSS

    cat > "${bundle_dir}/viewer/vendor/runtime.js" <<'JS'
console.log("verify-live fixture");
JS

    cat > "${bundle_dir}/viewer/data/messages.json" <<'JSON'
[]
JSON

    cat > "${bundle_dir}/viewer/data/meta.json" <<JSON
{"suite":"share_verify_live","label":"${root_label}"}
JSON

    cat > "${bundle_dir}/viewer/pages/index.html" <<HTML
<!doctype html><html><body><article>Page ${root_label}</article></body></html>
HTML

    cat > "${bundle_dir}/_headers" <<'HDR'
/*
  Cross-Origin-Opener-Policy: same-origin
  Cross-Origin-Embedder-Policy: require-corp
  Cross-Origin-Resource-Policy: same-origin
  Strict-Transport-Security: max-age=31536000
  X-Content-Type-Options: nosniff
  X-Frame-Options: DENY
HDR

    touch "${bundle_dir}/.nojekyll"
    sqlite3 "${bundle_dir}/mailbox.sqlite3" "VACUUM;" >/dev/null 2>&1
}

parse_verify_report() {
    local case_id="$1"
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    python3 - "${case_dir}/stdout.txt" "${case_dir}/report.json" "${case_dir}/summary.json" "${case_dir}/check_trace.jsonl" <<'PY'
import json
import pathlib
import sys

stdout_path = pathlib.Path(sys.argv[1])
report_path = pathlib.Path(sys.argv[2])
summary_path = pathlib.Path(sys.argv[3])
trace_path = pathlib.Path(sys.argv[4])

raw = stdout_path.read_text(encoding="utf-8", errors="replace").strip()
if not raw:
    raise SystemExit("empty verify-live stdout")

report = json.loads(raw)

required_top = ["schema_version", "generated_at", "url", "verdict", "stages", "summary", "config"]
missing = [k for k in required_top if k not in report]
if missing:
    raise SystemExit(f"missing top-level keys: {missing}")

for stage in ("local", "remote", "security"):
    if stage not in report["stages"]:
        raise SystemExit(f"missing stage: {stage}")
    if "ran" not in report["stages"][stage] or "checks" not in report["stages"][stage]:
        raise SystemExit(f"invalid stage schema: {stage}")

report_path.write_text(json.dumps(report, indent=2, sort_keys=True), encoding="utf-8")

summary = {
    "verdict": report.get("verdict"),
    "total": report.get("summary", {}).get("total"),
    "passed": report.get("summary", {}).get("passed"),
    "failed": report.get("summary", {}).get("failed"),
    "warnings": report.get("summary", {}).get("warnings"),
    "skipped": report.get("summary", {}).get("skipped"),
    "elapsed_ms": report.get("summary", {}).get("elapsed_ms"),
}
summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True), encoding="utf-8")

with trace_path.open("w", encoding="utf-8") as out:
    for stage_name in ("local", "remote", "security"):
        stage = report["stages"][stage_name]
        for check in stage.get("checks", []):
            entry = {
                "stage": stage_name,
                "id": check.get("id"),
                "severity": check.get("severity"),
                "passed": check.get("passed"),
                "message": check.get("message"),
                "elapsed_ms": check.get("elapsed_ms"),
                "http_status": check.get("http_status"),
            }
            out.write(json.dumps(entry, sort_keys=True) + "\n")
PY
}

assert_report_condition() {
    local case_id="$1"
    local label="$2"
    local expr="$3"
    local report_path="${E2E_ARTIFACT_DIR}/${case_id}/report.json"
    if python3 - "${report_path}" "${expr}" <<'PY'
import json
import sys

report = json.load(open(sys.argv[1], "r", encoding="utf-8"))
expr = sys.argv[2]
env = {
    "r": report,
    "any": any,
    "all": all,
    "len": len,
    "sum": sum,
    "str": str,
}
ok = bool(eval(expr, {"__builtins__": {}}, env))
sys.exit(0 if ok else 1)
PY
    then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
    fi
}

assert_case_output_contains() {
    local case_id="$1"
    local label="$2"
    local file_name="$3"
    local needle="$4"
    local content
    content="$(cat "${E2E_ARTIFACT_DIR}/${case_id}/${file_name}" 2>/dev/null || true)"
    e2e_assert_contains "${label}" "${content}" "${needle}"
}

run_verify_live_case() {
    local case_id="$1"
    local expected_rc="$2"
    local url="$3"
    local bundle_dir="$4"
    shift 4

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    mkdir -p "${case_dir}"
    e2e_mark_case_start "${case_id}"

    local -a cmd=("${AM_BIN}" "share" "deploy" "verify-live" "${url}" "--bundle" "${bundle_dir}" "--json")
    if [ "$#" -gt 0 ]; then
        cmd+=("$@")
    fi

    printf 'DATABASE_URL=%q STORAGE_ROOT=%q ' "sqlite:////${DB_PATH}" "${STORAGE_ROOT}" > "${case_dir}/command.txt"
    printf '%q ' "${cmd[@]}" >> "${case_dir}/command.txt"
    echo "" >> "${case_dir}/command.txt"

    local started_ms ended_ms elapsed_ms rc
    started_ms="$(now_ms)"
    set +e
    am_env "${cmd[@]}" > "${case_dir}/stdout.txt" 2> "${case_dir}/stderr.txt"
    rc=$?
    set -e
    ended_ms="$(now_ms)"
    elapsed_ms=$(( ended_ms - started_ms ))

    echo "${expected_rc}" > "${case_dir}/expected_exit_code.txt"
    echo "${rc}" > "${case_dir}/exit_code.txt"
    echo "${elapsed_ms}" > "${case_dir}/elapsed_ms.txt"

    cat > "${case_dir}/metadata.json" <<JSON
{
  "case_id": "${case_id}",
  "url": "${url}",
  "bundle_dir": "${bundle_dir}",
  "expected_exit_code": ${expected_rc},
  "actual_exit_code": ${rc},
  "elapsed_ms": ${elapsed_ms},
  "seed": "${E2E_SEED}",
  "clock_mode": "${E2E_CLOCK_MODE}"
}
JSON

    e2e_assert_exit_code "${case_id}: verify-live exit code" "${expected_rc}" "${rc}"

    if parse_verify_report "${case_id}" > "${case_dir}/parse_report.log" 2>&1; then
        e2e_pass "${case_id}: JSON report schema parsed"
    else
        e2e_fail "${case_id}: JSON report schema parsed"
        e2e_log "${case_id}: parse_report.log => ${case_dir}/parse_report.log"
    fi

    e2e_mark_case_end "${case_id}"
}

run_shell_case() {
    local case_id="$1"
    local expected_rc="$2"
    shift 2

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    mkdir -p "${case_dir}"
    e2e_mark_case_start "${case_id}"

    printf '%q ' "$@" > "${case_dir}/command.txt"
    echo "" >> "${case_dir}/command.txt"

    local started_ms ended_ms elapsed_ms rc
    started_ms="$(now_ms)"
    set +e
    "$@" > "${case_dir}/stdout.txt" 2> "${case_dir}/stderr.txt"
    rc=$?
    set -e
    ended_ms="$(now_ms)"
    elapsed_ms=$(( ended_ms - started_ms ))

    echo "${expected_rc}" > "${case_dir}/expected_exit_code.txt"
    echo "${rc}" > "${case_dir}/exit_code.txt"
    echo "${elapsed_ms}" > "${case_dir}/elapsed_ms.txt"

    e2e_assert_exit_code "${case_id}: command exit code" "${expected_rc}" "${rc}"
    e2e_mark_case_end "${case_id}"
}

FIXTURE_SERVER="${WORK}/verify_live_fixture_server.py"
cat > "${FIXTURE_SERVER}" <<'PY'
#!/usr/bin/env python3
import argparse
import functools
import time
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer


class FixtureHandler(SimpleHTTPRequestHandler):
    mode = "static"
    header_profile = "none"
    delay_ms = 0

    def end_headers(self):
        profile = self.header_profile.lower()
        if profile == "full":
            self.send_header("Cross-Origin-Opener-Policy", "same-origin")
            self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
            self.send_header("Cross-Origin-Resource-Policy", "same-origin")
            self.send_header("Strict-Transport-Security", "max-age=31536000")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("X-Frame-Options", "DENY")
        elif profile == "coop_only":
            self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        super().end_headers()

    def do_GET(self):
        if self.mode == "redirect_loop":
            self.send_response(302)
            self.send_header("Location", "/loop")
            self.end_headers()
            return

        if self.mode == "timeout" and self.delay_ms > 0:
            time.sleep(self.delay_ms / 1000.0)

        if self.mode == "partial_manifest" and self.path.startswith("/manifest.json"):
            self.send_error(404, "manifest intentionally missing")
            return

        if self.mode == "content_mismatch" and self.path in ("/", "/index.html"):
            body = b"<!doctype html><html><body><main>remote-mismatch</main></body></html>"
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        super().do_GET()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True)
    parser.add_argument("--port", required=True, type=int)
    parser.add_argument("--mode", default="static")
    parser.add_argument("--header-profile", default="none")
    parser.add_argument("--delay-ms", default=0, type=int)
    args = parser.parse_args()

    handler = functools.partial(FixtureHandler, directory=args.root)
    handler.mode = args.mode
    handler.header_profile = args.header_profile
    handler.delay_ms = args.delay_ms

    server = ThreadingHTTPServer(("127.0.0.1", args.port), handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
PY
chmod +x "${FIXTURE_SERVER}"

_SERVER_PIDS=()

cleanup_fixture_servers() {
    local pid
    for pid in "${_SERVER_PIDS[@]}"; do
        if kill -0 "${pid}" 2>/dev/null; then
            kill "${pid}" 2>/dev/null || true
            sleep 0.1
            kill -9 "${pid}" 2>/dev/null || true
            wait "${pid}" 2>/dev/null || true
        fi
    done
}
trap cleanup_fixture_servers EXIT

start_fixture_server() {
    local case_id="$1"
    local root_dir="$2"
    local mode="$3"
    local header_profile="$4"
    local delay_ms="${5:-0}"
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    mkdir -p "${case_dir}"

    local port log_file pid
    port="$(reserve_port)"
    log_file="${case_dir}/fixture_server.log"

    python3 "${FIXTURE_SERVER}" \
        --root "${root_dir}" \
        --port "${port}" \
        --mode "${mode}" \
        --header-profile "${header_profile}" \
        --delay-ms "${delay_ms}" > "${log_file}" 2>&1 &
    pid=$!
    _SERVER_PIDS+=("${pid}")

    if ! e2e_wait_port 127.0.0.1 "${port}" 8; then
        e2e_fail "${case_id}: fixture server failed to start"
        e2e_log "${case_id}: fixture log ${log_file}"
        echo "${pid}|${port}|${log_file}"
        return 1
    fi

    echo "${pid}|${port}|${log_file}"
}

stop_fixture_server() {
    local pid="$1"
    if kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        sleep 0.1
        kill -9 "${pid}" 2>/dev/null || true
        wait "${pid}" 2>/dev/null || true
    fi
}

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

BUNDLE_BASE="${WORK}/bundle_base"
write_bundle_fixture "${BUNDLE_BASE}" "bundle-root-v1"

DEPLOY_PASS="${WORK}/deploy_pass"
cp -R "${BUNDLE_BASE}" "${DEPLOY_PASS}"

DEPLOY_PARTIAL="${WORK}/deploy_partial"
cp -R "${BUNDLE_BASE}" "${DEPLOY_PARTIAL}"
rm -f "${DEPLOY_PARTIAL}/manifest.json"

DEPLOY_MISMATCH="${WORK}/deploy_mismatch"
cp -R "${BUNDLE_BASE}" "${DEPLOY_MISMATCH}"
cat > "${DEPLOY_MISMATCH}/index.html" <<'HTML'
<!doctype html><html><body><main>remote-mismatch</main></body></html>
HTML

BUNDLE_FAILFAST="${WORK}/bundle_failfast"
cp -R "${BUNDLE_BASE}" "${BUNDLE_FAILFAST}"
rm -f "${BUNDLE_FAILFAST}/manifest.json"

TOOLING_OUT="$(am_env "${AM_BIN}" share deploy tooling --bundle "${BUNDLE_BASE}" 2>&1)" || true
e2e_save_artifact "tooling_stdout.txt" "${TOOLING_OUT}"
VALIDATE_SCRIPT="${BUNDLE_BASE}/scripts/validate_deploy.sh"
e2e_assert_file_exists "validate_deploy.sh generated" "${VALIDATE_SCRIPT}"

CASE_IDS=()
record_case() {
    CASE_IDS+=("$1")
}

# ---------------------------------------------------------------------------
# Case 1: Happy path (pass)
# ---------------------------------------------------------------------------

e2e_case_banner "case_01_pass: full checks pass (local + remote + security)"
record_case "case_01_pass"
IFS='|' read -r pid port _log < <(start_fixture_server "case_01_pass" "${DEPLOY_PASS}" "static" "full" "0")
run_verify_live_case "case_01_pass" 0 "http://127.0.0.1:${port}" "${BUNDLE_BASE}" --security --retries 0
assert_report_condition "case_01_pass" "case_01_pass verdict=pass" "r['verdict'] == 'pass'"
assert_report_condition "case_01_pass" "case_01_pass security stage ran" "r['stages']['security']['ran'] is True"
assert_report_condition "case_01_pass" "case_01_pass remote.content_match passed" "any((c['id']=='remote.content_match' and c['passed']) for c in r['stages']['remote']['checks'])"
stop_fixture_server "${pid}"

# ---------------------------------------------------------------------------
# Case 2: Missing COOP/COEP headers (warn, non-strict)
# ---------------------------------------------------------------------------

e2e_case_banner "case_02_warn_headers: warning verdict returns exit 0 without --strict"
record_case "case_02_warn_headers"
IFS='|' read -r pid port _log < <(start_fixture_server "case_02_warn_headers" "${DEPLOY_PASS}" "static" "none" "0")
run_verify_live_case "case_02_warn_headers" 0 "http://127.0.0.1:${port}" "${BUNDLE_BASE}" --retries 0
assert_report_condition "case_02_warn_headers" "case_02_warn_headers verdict=warn" "r['verdict'] == 'warn'"
assert_report_condition "case_02_warn_headers" "case_02_warn_headers has warning failures" "r['summary']['warnings'] >= 2"
assert_report_condition "case_02_warn_headers" "case_02_warn_headers COOP check failed" "any((c['id']=='remote.coop' and (not c['passed']) and c['severity']=='warning') for c in r['stages']['remote']['checks'])"
assert_report_condition "case_02_warn_headers" "case_02_warn_headers COEP check failed" "any((c['id']=='remote.coep' and (not c['passed']) and c['severity']=='warning') for c in r['stages']['remote']['checks'])"

# ---------------------------------------------------------------------------
# Case 3: Missing COOP/COEP headers (warn + strict => exit 1)
# ---------------------------------------------------------------------------

e2e_case_banner "case_03_warn_headers_strict: warning verdict exits 1 with --strict"
record_case "case_03_warn_headers_strict"
run_verify_live_case "case_03_warn_headers_strict" 1 "http://127.0.0.1:${port}" "${BUNDLE_BASE}" --strict --retries 0
assert_report_condition "case_03_warn_headers_strict" "case_03_warn_headers_strict verdict=warn" "r['verdict'] == 'warn'"
assert_report_condition "case_03_warn_headers_strict" "case_03_warn_headers_strict config.strict=true" "r['config']['strict'] is True"
stop_fixture_server "${pid}"

# ---------------------------------------------------------------------------
# Case 4: Partial deploy missing manifest (error)
# ---------------------------------------------------------------------------

e2e_case_banner "case_04_partial_missing_manifest: remote error yields exit 1"
record_case "case_04_partial_missing_manifest"
IFS='|' read -r pid port _log < <(start_fixture_server "case_04_partial_missing_manifest" "${DEPLOY_PARTIAL}" "partial_manifest" "full" "0")
run_verify_live_case "case_04_partial_missing_manifest" 1 "http://127.0.0.1:${port}" "${BUNDLE_BASE}" --retries 0
assert_report_condition "case_04_partial_missing_manifest" "case_04_partial_missing_manifest verdict=fail" "r['verdict'] == 'fail'"
assert_report_condition "case_04_partial_missing_manifest" "case_04_partial_missing_manifest remote.manifest failed as error" "any((c['id']=='remote.manifest' and (not c['passed']) and c['severity']=='error') for c in r['stages']['remote']['checks'])"
stop_fixture_server "${pid}"

# ---------------------------------------------------------------------------
# Case 5: Content mismatch (warning)
# ---------------------------------------------------------------------------

e2e_case_banner "case_05_content_mismatch: checksum mismatch produces warning"
record_case "case_05_content_mismatch"
IFS='|' read -r pid port _log < <(start_fixture_server "case_05_content_mismatch" "${DEPLOY_MISMATCH}" "content_mismatch" "full" "0")
run_verify_live_case "case_05_content_mismatch" 0 "http://127.0.0.1:${port}" "${BUNDLE_BASE}" --retries 0
assert_report_condition "case_05_content_mismatch" "case_05_content_mismatch verdict=warn" "r['verdict'] == 'warn'"
assert_report_condition "case_05_content_mismatch" "case_05_content_mismatch remote.content_match failed warning" "any((c['id']=='remote.content_match' and (not c['passed']) and c['severity']=='warning') for c in r['stages']['remote']['checks'])"
stop_fixture_server "${pid}"

# ---------------------------------------------------------------------------
# Case 6: Timeout failure
# ---------------------------------------------------------------------------

e2e_case_banner "case_06_timeout: low timeout + slow server fails probes"
record_case "case_06_timeout"
IFS='|' read -r pid port _log < <(start_fixture_server "case_06_timeout" "${DEPLOY_PASS}" "timeout" "full" "900")
run_verify_live_case "case_06_timeout" 1 "http://127.0.0.1:${port}" "${BUNDLE_BASE}" --timeout 150 --retries 0 --retry-delay 0
assert_report_condition "case_06_timeout" "case_06_timeout verdict=fail" "r['verdict'] == 'fail'"
assert_report_condition "case_06_timeout" "case_06_timeout remote.root timeout message" "any((c['id']=='remote.root' and (not c['passed']) and ('timed out' in c['message'])) for c in r['stages']['remote']['checks'])"
stop_fixture_server "${pid}"

# ---------------------------------------------------------------------------
# Case 7: Redirect loop failure
# ---------------------------------------------------------------------------

e2e_case_banner "case_07_redirect_loop: redirect loop is surfaced as failure"
record_case "case_07_redirect_loop"
IFS='|' read -r pid port _log < <(start_fixture_server "case_07_redirect_loop" "${DEPLOY_PASS}" "redirect_loop" "none" "0")
run_verify_live_case "case_07_redirect_loop" 1 "http://127.0.0.1:${port}" "${BUNDLE_BASE}" --retries 0
assert_report_condition "case_07_redirect_loop" "case_07_redirect_loop verdict=fail" "r['verdict'] == 'fail'"
assert_report_condition "case_07_redirect_loop" "case_07_redirect_loop remote.root shows redirect error" "any((c['id']=='remote.root' and (not c['passed']) and ('redirect' in c['message'].lower())) for c in r['stages']['remote']['checks'])"
stop_fixture_server "${pid}"

# ---------------------------------------------------------------------------
# Case 8: Connection refused failure
# ---------------------------------------------------------------------------

e2e_case_banner "case_08_connection_refused: unreachable host fails"
record_case "case_08_connection_refused"
UNUSED_PORT="$(reserve_port)"
run_verify_live_case "case_08_connection_refused" 1 "http://127.0.0.1:${UNUSED_PORT}" "${BUNDLE_BASE}" --timeout 200 --retries 0
assert_report_condition "case_08_connection_refused" "case_08_connection_refused verdict=fail" "r['verdict'] == 'fail'"
assert_report_condition "case_08_connection_refused" "case_08_connection_refused remote.root connection error" "any((c['id']=='remote.root' and (not c['passed']) and ('connection' in c['message'].lower())) for c in r['stages']['remote']['checks'])"

# ---------------------------------------------------------------------------
# Case 9: Fail-fast short-circuit on local errors
# ---------------------------------------------------------------------------

e2e_case_banner "case_09_fail_fast_local: local error short-circuits remote stage"
record_case "case_09_fail_fast_local"
UNUSED_PORT="$(reserve_port)"
run_verify_live_case "case_09_fail_fast_local" 1 "http://127.0.0.1:${UNUSED_PORT}" "${BUNDLE_FAILFAST}" --fail-fast --retries 0
assert_report_condition "case_09_fail_fast_local" "case_09_fail_fast_local local stage ran" "r['stages']['local']['ran'] is True"
assert_report_condition "case_09_fail_fast_local" "case_09_fail_fast_local remote stage skipped" "r['stages']['remote']['ran'] is False"
assert_report_condition "case_09_fail_fast_local" "case_09_fail_fast_local has local error check" "any(((not c['passed']) and c['severity']=='error') for c in r['stages']['local']['checks'])"

# ---------------------------------------------------------------------------
# Case 10: Compatibility wrapper delegates to native verify-live
# ---------------------------------------------------------------------------

e2e_case_banner "case_10_wrapper_delegate: validate_deploy.sh delegates to native command"
record_case "case_10_wrapper_delegate"
IFS='|' read -r pid port _log < <(start_fixture_server "case_10_wrapper_delegate" "${DEPLOY_PASS}" "static" "full" "0")
run_shell_case "case_10_wrapper_delegate" 0 bash "${VALIDATE_SCRIPT}" "${BUNDLE_BASE}" "http://127.0.0.1:${port}"
assert_case_output_contains "case_10_wrapper_delegate" "case_10_wrapper_delegate shows delegation line" "stdout.txt" "Delegating to native command:"
assert_case_output_contains "case_10_wrapper_delegate" "case_10_wrapper_delegate invokes verify-live" "stdout.txt" "verify-live:"
stop_fixture_server "${pid}"

# ---------------------------------------------------------------------------
# Case 11: Compatibility wrapper strict passthrough
# ---------------------------------------------------------------------------

e2e_case_banner "case_11_wrapper_strict: AM_VERIFY_LIVE_STRICT maps to native --strict"
record_case "case_11_wrapper_strict"
IFS='|' read -r pid port _log < <(start_fixture_server "case_11_wrapper_strict" "${DEPLOY_PASS}" "static" "none" "0")
run_shell_case "case_11_wrapper_strict" 1 env AM_VERIFY_LIVE_STRICT=1 bash "${VALIDATE_SCRIPT}" "${BUNDLE_BASE}" "http://127.0.0.1:${port}"
assert_case_output_contains "case_11_wrapper_strict" "case_11_wrapper_strict shows delegation line" "stdout.txt" "Delegating to native command:"
assert_case_output_contains "case_11_wrapper_strict" "case_11_wrapper_strict still runs verify-live" "stdout.txt" "verify-live:"
stop_fixture_server "${pid}"

# ---------------------------------------------------------------------------
# Matrix summary artifact
# ---------------------------------------------------------------------------

python3 - "${E2E_ARTIFACT_DIR}" "${CASE_IDS[@]}" > "${E2E_ARTIFACT_DIR}/verify_live_matrix.json" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
cases = sys.argv[2:]
rows = []

for case_id in cases:
    case_dir = root / case_id
    row = {
        "case_id": case_id,
        "exit_code": None,
        "expected_exit_code": None,
        "elapsed_ms": None,
        "verdict": None,
        "summary": None,
    }

    for key, name in (("exit_code", "exit_code.txt"), ("expected_exit_code", "expected_exit_code.txt"), ("elapsed_ms", "elapsed_ms.txt")):
        p = case_dir / name
        if p.exists():
            try:
                row[key] = int(p.read_text(encoding="utf-8").strip())
            except Exception:
                row[key] = p.read_text(encoding="utf-8", errors="replace").strip()

    report_path = case_dir / "report.json"
    if report_path.exists():
        try:
            report = json.loads(report_path.read_text(encoding="utf-8"))
            row["verdict"] = report.get("verdict")
            row["summary"] = report.get("summary")
        except Exception as exc:
            row["summary"] = {"parse_error": str(exc)}

    rows.append(row)

print(json.dumps({"suite": "share_verify_live", "cases": rows}, indent=2, sort_keys=True))
PY

e2e_summary
