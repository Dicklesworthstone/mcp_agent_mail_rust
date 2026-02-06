#!/usr/bin/env bash
# test_guard.sh - E2E test suite for guard hook installation and conflict detection
#
# Tests (br-2ei.9.3):
# 1. Install guard hook into a fresh git repo
# 2. Verify hook files exist and have correct structure
# 3. Pre-commit runs clean with no reservations
# 4. Guard check detects conflict with exclusive reservation
# 5. Release reservation, guard check passes
# 6. Rename scenario: both old and new paths checked
# 7. stdin-nul: NUL-delimited path input
# 8. Uninstall guard and verify cleanup

E2E_SUITE="guard"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Guard E2E Test Suite"

# Build the am binary if needed (also exports PATH so `am` is callable)
e2e_ensure_binary "am" >/dev/null

# ---------------------------------------------------------------------------
# Setup: create temp workspace
# ---------------------------------------------------------------------------
WORK="$(e2e_mktemp "e2e_guard")"
REPO="${WORK}/repo"
GUARD_DB="${WORK}/storage.sqlite3"
mkdir -p "$REPO"

e2e_init_git_repo "$REPO"

# Create initial tracked file and commit
echo "init" > "$REPO/README.md"
mkdir -p "$REPO/app/api" "$REPO/shared"
echo "# app init" > "$REPO/app/api/views.py"
echo "# shared init" > "$REPO/shared/utils.py"
e2e_git_commit "$REPO" "initial commit"

HOOKS_DIR="${REPO}/.git/hooks"
mkdir -p "${REPO}/file_reservations"

# Future and past timestamps for reservations
FUTURE_TS="$(date -u -d '+1 hour' '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
           || date -u -v '+1H' '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
           || echo '2099-01-01T00:00:00Z')"
PAST_TS="$(date -u -d '-1 hour' '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
         || date -u -v '-1H' '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
         || echo '2020-01-01T00:00:00Z')"

# ---------------------------------------------------------------------------
# Case 1: Install guard via CLI
# ---------------------------------------------------------------------------
e2e_case_banner "Install guard into fresh git repo"

set +e
# NOTE: guard plugin queries DB rows by `projects.human_key == <project>`; use repo path.
am guard install "$REPO" "$REPO" 2>"${WORK}/install_stderr.txt"
install_rc=$?
set -e

e2e_assert_exit_code "guard install succeeds" "0" "$install_rc"
e2e_assert_file_exists "chain-runner exists" "${HOOKS_DIR}/pre-commit"
e2e_assert_file_exists "plugin exists" "${HOOKS_DIR}/hooks.d/pre-commit/50-agent-mail.py"

chain_content="$(cat "${HOOKS_DIR}/pre-commit")"
e2e_assert_contains "chain-runner has sentinel" "$chain_content" "mcp-agent-mail chain-runner"

e2e_save_artifact "case1_hooks_tree.txt" "$(e2e_tree "$HOOKS_DIR")"

# ---------------------------------------------------------------------------
# Case 2: Guard status reports installation
# ---------------------------------------------------------------------------
e2e_case_banner "Guard status shows installed"

set +e
status_output="$(am guard status "$REPO" 2>&1)"
status_rc=$?
set -e

e2e_assert_exit_code "guard status succeeds" "0" "$status_rc"
e2e_assert_contains "status shows pre-commit" "$status_output" "installed"

e2e_save_artifact "case2_status.txt" "$status_output"

# ---------------------------------------------------------------------------
# Case 3: Pre-commit hook runs clean with no reservations
# ---------------------------------------------------------------------------
e2e_case_banner "Pre-commit runs clean with no reservations"

echo "new content" > "${REPO}/test.py"
git -C "$REPO" add test.py

set +e
git -C "$REPO" commit -qm "add test.py" 2>"${WORK}/commit_clean_stderr.txt"
commit_rc=$?
set -e

e2e_assert_exit_code "commit succeeds (no reservations)" "0" "$commit_rc"
e2e_save_artifact "case3_commit_clean_stderr.txt" "$(cat "${WORK}/commit_clean_stderr.txt" 2>/dev/null || true)"

# ---------------------------------------------------------------------------
# Case 4: Pre-commit blocks commit when exclusive reservation conflicts
# ---------------------------------------------------------------------------
e2e_case_banner "Pre-commit blocks commit when exclusive reservation conflicts"

# Create a minimal sqlite DB for the guard plugin to query.
GUARD_DB="$GUARD_DB" PROJECT_KEY="$REPO" python3 - <<'PY'
import os
import sqlite3
import time

db_path = os.environ["GUARD_DB"]
project_key = os.environ["PROJECT_KEY"]

conn = sqlite3.connect(db_path, timeout=5)
cur = conn.cursor()

cur.execute("CREATE TABLE projects (id INTEGER PRIMARY KEY, human_key TEXT)")
cur.execute("CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT)")
cur.execute(
    "CREATE TABLE file_reservations ("
    "id INTEGER PRIMARY KEY, "
    "project_id INTEGER, "
    "agent_id INTEGER, "
    "path_pattern TEXT, "
    "exclusive INTEGER, "
    "expires_ts INTEGER, "
    "released_ts INTEGER NULL"
    ")"
)

cur.execute("INSERT INTO projects (id, human_key) VALUES (1, ?)", (project_key,))
cur.execute("INSERT INTO agents (id, name) VALUES (1, 'TestAgent')")
cur.execute("INSERT INTO agents (id, name) VALUES (2, 'OtherAgent')")

now_micros = int(time.time() * 1_000_000)
expires_micros = now_micros + 3600 * 1_000_000
cur.execute(
    "INSERT INTO file_reservations "
    "(id, project_id, agent_id, path_pattern, exclusive, expires_ts, released_ts) "
    "VALUES (1, 1, 2, 'app/api/*.py', 1, ?, NULL)",
    (expires_micros,),
)

conn.commit()
conn.close()
PY

# Stage a conflicting change and attempt commit (should be blocked by pre-commit)
echo "# changed" >> "$REPO/app/api/views.py"
git -C "$REPO" add app/api/views.py

set +e
AGENT_NAME=TestAgent AGENT_MAIL_DB="$GUARD_DB" git -C "$REPO" commit -m "conflicting change" \
  2>"${WORK}/commit_conflict_stderr.txt"
commit_conflict_rc=$?
set -e

e2e_assert_exit_code "commit is blocked (exit 1)" "1" "$commit_conflict_rc"
commit_conflict_stderr="$(cat "${WORK}/commit_conflict_stderr.txt" 2>/dev/null || true)"
e2e_assert_contains "stderr mentions reservation conflict" "$commit_conflict_stderr" "file reservation conflict"
e2e_assert_contains "stderr mentions OtherAgent" "$commit_conflict_stderr" "OtherAgent"
e2e_assert_contains "stderr mentions pattern" "$commit_conflict_stderr" "app/api/*.py"

e2e_save_artifact "case4_commit_blocked_stderr.txt" "$commit_conflict_stderr"

# ---------------------------------------------------------------------------
# Case 5: Release reservation, commit succeeds
# ---------------------------------------------------------------------------
e2e_case_banner "Release reservation then commit succeeds"

GUARD_DB="$GUARD_DB" python3 - <<'PY'
import os
import sqlite3
import time

db_path = os.environ["GUARD_DB"]
conn = sqlite3.connect(db_path, timeout=5)
cur = conn.cursor()
now_micros = int(time.time() * 1_000_000)
cur.execute("UPDATE file_reservations SET released_ts = ? WHERE id = 1", (now_micros,))
conn.commit()
conn.close()
PY

set +e
AGENT_NAME=TestAgent AGENT_MAIL_DB="$GUARD_DB" git -C "$REPO" commit -m "conflicting change (after release)" \
  2>"${WORK}/commit_after_release_stderr.txt"
commit_after_release_rc=$?
set -e

e2e_assert_exit_code "commit succeeds after release" "0" "$commit_after_release_rc"
e2e_save_artifact "case5_commit_after_release_stderr.txt" "$(cat "${WORK}/commit_after_release_stderr.txt" 2>/dev/null || true)"

# ---------------------------------------------------------------------------
# Case 6: Guard check detects exclusive reservation conflict (archive JSON)
# ---------------------------------------------------------------------------
e2e_case_banner "Guard check detects conflict with exclusive reservation (archive JSON)"

# Create an active exclusive reservation by OtherAgent (archive JSON files)
cat > "${REPO}/file_reservations/res_api.json" << EOJSON
{
    "id": 1,
    "path_pattern": "app/api/*.py",
    "agent_name": "OtherAgent",
    "exclusive": true,
    "expires_ts": "${FUTURE_TS}",
    "released_ts": null
}
EOJSON

# Create a non-exclusive reservation (should NOT block)
cat > "${REPO}/file_reservations/res_shared.json" << EOJSON
{
    "id": 2,
    "path_pattern": "shared/*",
    "agent_name": "SharedAgent",
    "exclusive": false,
    "expires_ts": "${FUTURE_TS}",
    "released_ts": null
}
EOJSON

# Create a released reservation (should be ignored)
cat > "${REPO}/file_reservations/res_released.json" << EOJSON
{
    "id": 3,
    "path_pattern": "app/**",
    "agent_name": "ReleasedAgent",
    "exclusive": true,
    "expires_ts": "${FUTURE_TS}",
    "released_ts": "2025-01-01T00:00:00Z"
}
EOJSON

# Create an expired reservation (should be ignored)
cat > "${REPO}/file_reservations/res_expired.json" << EOJSON
{
    "id": 4,
    "path_pattern": "app/api/*",
    "agent_name": "ExpiredAgent",
    "exclusive": true,
    "expires_ts": "${PAST_TS}",
    "released_ts": null
}
EOJSON

# Guard check with a conflicting path (matches OtherAgent's exclusive pattern)
set +e
check_output="$(echo "app/api/views.py" | AGENT_NAME=TestAgent am guard check --repo "$REPO" 2>&1)"
check_rc=$?
set -e

e2e_assert_exit_code "guard check detects conflict (exit 1)" "1" "$check_rc"
e2e_assert_contains "conflict mentions OtherAgent" "$check_output" "OtherAgent"
e2e_assert_contains "conflict mentions pattern" "$check_output" "app/api/*.py"

e2e_save_artifact "case4_conflict_output.txt" "$check_output"
e2e_save_artifact "case4_reservations.txt" "$(e2e_tree "${REPO}/file_reservations")"

# Non-exclusive path should NOT conflict
set +e
shared_output="$(echo "shared/utils.py" | AGENT_NAME=TestAgent am guard check --repo "$REPO" 2>&1)"
shared_rc=$?
set -e

e2e_assert_exit_code "non-exclusive reservation does not block" "0" "$shared_rc"
e2e_assert_contains "no conflicts for shared" "$shared_output" "No file reservation conflicts"

# ---------------------------------------------------------------------------
# Case 7: Release reservation JSON, guard check passes
# ---------------------------------------------------------------------------
e2e_case_banner "Release reservation JSON then guard check passes"

# Simulate release by adding released_ts to the reservation
cat > "${REPO}/file_reservations/res_api.json" << EOJSON
{
    "id": 1,
    "path_pattern": "app/api/*.py",
    "agent_name": "OtherAgent",
    "exclusive": true,
    "expires_ts": "${FUTURE_TS}",
    "released_ts": "$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || echo '2026-02-06T00:00:00Z')"
}
EOJSON

set +e
release_output="$(echo "app/api/views.py" | AGENT_NAME=TestAgent am guard check --repo "$REPO" 2>&1)"
release_rc=$?
set -e

e2e_assert_exit_code "guard check passes after release" "0" "$release_rc"
e2e_assert_contains "no conflicts after release" "$release_output" "No file reservation conflicts"

e2e_save_artifact "case5_release_output.txt" "$release_output"

# ---------------------------------------------------------------------------
# Case 8: Rename scenario - stage rename; both old and new paths checked
# ---------------------------------------------------------------------------
e2e_case_banner "Rename scenario: stage rename; guard checks both old and new paths"

# Create an active reservation matching ONLY the old path.
cat > "${REPO}/file_reservations/res_rename.json" << EOJSON
{
    "id": 6,
    "path_pattern": "lib/original_module.py",
    "agent_name": "OtherAgent",
    "exclusive": true,
    "expires_ts": "${FUTURE_TS}",
    "released_ts": null
}
EOJSON

# Create + commit the file, then stage a rename.
mkdir -p "$REPO/lib"
echo "x" > "$REPO/lib/original_module.py"
e2e_git_commit "$REPO" "add original module"
git -C "$REPO" mv lib/original_module.py lib/renamed_module.py
# Extract touched paths from staged rename (NUL-delimited parsing, includes old+new).
rename_paths="$(
    git -C "$REPO" diff --cached --name-status -M -z | python3 -c '
import sys
raw = sys.stdin.buffer.read().split(b"\0")
paths = []
i = 0
while i < len(raw):
    if not raw[i]:
        break
    status = raw[i].decode("utf-8", "ignore")
    i += 1
    if status.startswith(("R", "C")):
        if i + 1 >= len(raw):
            break
        oldp = raw[i].decode("utf-8", "ignore")
        newp = raw[i + 1].decode("utf-8", "ignore")
        i += 2
        if oldp:
            paths.append(oldp)
        if newp:
            paths.append(newp)
    else:
        if i >= len(raw):
            break
        p = raw[i].decode("utf-8", "ignore")
        i += 1
        if p:
            paths.append(p)
print("\n".join(paths))
'
)"

e2e_assert_contains "rename path list includes old path" "$rename_paths" "lib/original_module.py"
e2e_assert_contains "rename path list includes new path" "$rename_paths" "lib/renamed_module.py"
e2e_save_artifact "case6_rename_paths.txt" "$rename_paths"

# Guard check should consider both old+new; conflict should be detected due to reservation on old path.
set +e
both_output="$(printf '%s\n' "$rename_paths" | AGENT_NAME=TestAgent am guard check --repo "$REPO" 2>&1)"
both_rc=$?
set -e
e2e_assert_exit_code "both paths checked, conflict found" "1" "$both_rc"
e2e_assert_contains "rename conflict mentions holder" "$both_output" "OtherAgent"

e2e_save_artifact "case6_rename_both.txt" "$both_output"

# ---------------------------------------------------------------------------
# Case 9: stdin-nul input (NUL-delimited paths)
# ---------------------------------------------------------------------------
e2e_case_banner "stdin-nul: NUL-delimited path input"

# Restore an active api reservation for this test
cat > "${REPO}/file_reservations/res_api.json" << EOJSON
{
    "id": 1,
    "path_pattern": "app/api/*.py",
    "agent_name": "OtherAgent",
    "exclusive": true,
    "expires_ts": "${FUTURE_TS}",
    "released_ts": null
}
EOJSON

# NUL-delimited: conflicting path among non-conflicting ones
set +e
nul_output="$(printf 'README.md\0app/api/views.py\0test.py\0' | AGENT_NAME=TestAgent am guard check --stdin-nul --repo "$REPO" 2>&1)"
nul_rc=$?
set -e

e2e_assert_exit_code "stdin-nul detects conflict" "1" "$nul_rc"
e2e_assert_contains "stdin-nul conflict mentions OtherAgent" "$nul_output" "OtherAgent"

# NUL-delimited: no conflicting paths
set +e
nul_clean_output="$(printf 'README.md\0test.py\0' | AGENT_NAME=TestAgent am guard check --stdin-nul --repo "$REPO" 2>&1)"
nul_clean_rc=$?
set -e

e2e_assert_exit_code "stdin-nul passes with clean paths" "0" "$nul_clean_rc"

e2e_save_artifact "case7_stdin_nul.txt" "$nul_output"

# ---------------------------------------------------------------------------
# Case 10: Advisory mode (warn but don't fail)
# ---------------------------------------------------------------------------
e2e_case_banner "Advisory mode warns but exits 0"

set +e
advisory_output="$(echo "app/api/views.py" | AGENT_NAME=TestAgent am guard check --advisory --repo "$REPO" 2>&1)"
advisory_rc=$?
set -e

e2e_assert_exit_code "advisory mode exits 0 despite conflict" "0" "$advisory_rc"

e2e_save_artifact "case8_advisory.txt" "$advisory_output"

# ---------------------------------------------------------------------------
# Case 11: core.ignorecase (case-insensitive match)
# ---------------------------------------------------------------------------
e2e_case_banner "core.ignorecase toggles case-insensitive matching"

cat > "${REPO}/file_reservations/res_ignorecase.json" << EOJSON
{
    "id": 10,
    "path_pattern": "CaseTest/*.TXT",
    "agent_name": "OtherAgent",
    "exclusive": true,
    "expires_ts": "${FUTURE_TS}",
    "released_ts": null
}
EOJSON

# With core.ignorecase=false: should NOT match differing case.
git -C "$REPO" config core.ignorecase false
set +e
ic_false_output="$(echo "casetest/file.txt" | AGENT_NAME=TestAgent am guard check --repo "$REPO" 2>&1)"
ic_false_rc=$?
set -e
e2e_assert_exit_code "ignorecase=false does not match" "0" "$ic_false_rc"

# With core.ignorecase=true: should match differing case.
git -C "$REPO" config core.ignorecase true
set +e
ic_true_output="$(echo "casetest/file.txt" | AGENT_NAME=TestAgent am guard check --repo "$REPO" 2>&1)"
ic_true_rc=$?
set -e
e2e_assert_exit_code "ignorecase=true matches" "1" "$ic_true_rc"
e2e_assert_contains "ignorecase conflict mentions pattern" "$ic_true_output" "CaseTest/*.TXT"

e2e_save_artifact "case11_ignorecase_false.txt" "$ic_false_output"
e2e_save_artifact "case11_ignorecase_true.txt" "$ic_true_output"

# ---------------------------------------------------------------------------
# Case 12: Uninstall guard
# ---------------------------------------------------------------------------
e2e_case_banner "Uninstall guard removes hook files"

set +e
am guard uninstall "$REPO" 2>"${WORK}/uninstall_stderr.txt"
uninstall_rc=$?
set -e

e2e_assert_exit_code "guard uninstall succeeds" "0" "$uninstall_rc"
e2e_assert_eq "plugin removed" "false" "$([ -f "${HOOKS_DIR}/hooks.d/pre-commit/50-agent-mail.py" ] && echo true || echo false)"

e2e_save_artifact "case9_hooks_tree_after_uninstall.txt" "$(e2e_tree "$HOOKS_DIR" 2>/dev/null || echo "(empty)")"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "repo_git_status.txt" "$(git -C "$REPO" status --porcelain=v1 2>/dev/null || true)"
e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
