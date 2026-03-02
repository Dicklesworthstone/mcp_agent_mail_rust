#!/usr/bin/env bash
# test_migration.sh - E2E migration test for install-over-Python flow (br-28mgh.8.2)
#
# Validates the end-to-end installer takeover path:
#   - Existing Python alias + legacy MCP config detected
#   - Rust installer displaces Python alias
#   - Legacy SQLite timestamps (TEXT) are converted to i64 micros
#   - Legacy data remains accessible from Rust CLI
#   - MCP config is rewritten away from Python entry
#   - Doctor passes and migration backup artifacts are present

set -euo pipefail

E2E_SUITE="migration"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Installer Migration E2E Suite (br-28mgh.8.2)"

if ! command -v python3 >/dev/null 2>&1; then
    e2e_case_banner "Prerequisites"
    e2e_skip "python3 required"
    e2e_summary
    exit $?
fi

if ! command -v sqlite3 >/dev/null 2>&1; then
    e2e_case_banner "Prerequisites"
    e2e_skip "sqlite3 required"
    e2e_summary
    exit $?
fi

WORK="$(e2e_mktemp "e2e_migration")"
INSTALL_SH="${SCRIPT_DIR}/../../install.sh"
RUN_DIR="${WORK}/project"
TEST_HOME="${WORK}/home"
DEST="${TEST_HOME}/.local/bin"
STORAGE_ROOT="${TEST_HOME}/.mcp_agent_mail_git_mailbox_repo"
PYTHON_CLONE="${TEST_HOME}/legacy_python_clone"
PYTHON_DB="${PYTHON_CLONE}/storage.sqlite3"
MCP_CONFIG="${RUN_DIR}/codex.mcp.json"
PATH_BASE="/usr/bin:/bin"
LEGACY_TOKEN="legacy-token-123"

mkdir -p "${RUN_DIR}" "${DEST}" "${STORAGE_ROOT}" "${PYTHON_CLONE}" "${TEST_HOME}/.codex" "${TEST_HOME}/.config/fish"

json_query() {
    local json="$1"
    local expr="$2"
    echo "$json" | python3 -c "import json,sys; d=json.load(sys.stdin); ${expr}" 2>/dev/null
}

resolve_bin() {
    local env_path="$1"
    local bin_name="$2"
    local resolved=""
    if [ -n "${env_path}" ] && [ -x "${env_path}" ]; then
        resolved="${env_path}"
    else
        resolved="$(e2e_ensure_binary "${bin_name}" | tail -n 1)"
    fi
    if [ ! -x "${resolved}" ]; then
        e2e_fatal "missing binary for ${bin_name}: ${resolved}"
    fi
    echo "${resolved}"
}

run_installer() {
    local case_id="$1"
    local stdout_file="${WORK}/${case_id}_stdout.txt"
    local stderr_file="${WORK}/${case_id}_stderr.txt"
    set +e
    (
        cd "${RUN_DIR}"
        HOME="${TEST_HOME}" \
        PATH="${PATH_BASE}" \
        STORAGE_ROOT="${STORAGE_ROOT}" \
        bash "${INSTALL_SH}" \
            --version "v${TARGET_VERSION}" \
            --artifact-url "file://${ARTIFACT_PATH}" \
            --dest "${DEST}" \
            --offline \
            --no-verify \
            --no-gum \
            --easy-mode
    ) >"${stdout_file}" 2>"${stderr_file}"
    INSTALL_RC=$?
    set -e
    INSTALL_STDOUT="$(cat "${stdout_file}" 2>/dev/null || true)"
    INSTALL_STDERR="$(cat "${stderr_file}" 2>/dev/null || true)"
    e2e_save_artifact "${case_id}_stdout.txt" "${INSTALL_STDOUT}"
    e2e_save_artifact "${case_id}_stderr.txt" "${INSTALL_STDERR}"
}

run_migrated_am() {
    HOME="${TEST_HOME}" \
    PATH="${DEST}:${PATH_BASE}" \
    AM_INTERFACE_MODE=cli \
    DATABASE_URL="sqlite+aiosqlite:///${MIGRATED_DB}" \
    "${DEST}/am" "$@"
}

# Resolve binaries (prefer caller-provided paths for containerized execution).
AM_BIN="$(resolve_bin "${AM_E2E_MIGRATION_AM_BIN:-}" "am")"
SERVER_BIN="$(resolve_bin "${AM_E2E_MIGRATION_SERVER_BIN:-}" "mcp-agent-mail")"
TARGET_VERSION="$("${AM_BIN}" --version 2>/dev/null | awk '{print $2}' | head -1)"
[ -n "${TARGET_VERSION}" ] || TARGET_VERSION="0.0.0"

# Package a release-like artifact for install.sh offline flow.
ARTIFACT_STAGE="${WORK}/artifact"
ARTIFACT_PATH="${WORK}/mcp-agent-mail-v${TARGET_VERSION}.tar.xz"
mkdir -p "${ARTIFACT_STAGE}"
cp "${AM_BIN}" "${ARTIFACT_STAGE}/am"
cp "${SERVER_BIN}" "${ARTIFACT_STAGE}/mcp-agent-mail"
chmod +x "${ARTIFACT_STAGE}/am" "${ARTIFACT_STAGE}/mcp-agent-mail"
tar -cJf "${ARTIFACT_PATH}" -C "${ARTIFACT_STAGE}" am mcp-agent-mail
e2e_assert_file_exists "offline artifact created" "${ARTIFACT_PATH}"

# Seed a legacy Python-like shell alias and config surface.
cat > "${TEST_HOME}/.zshrc" <<EOF
# >>> MCP Agent Mail alias
alias am='cd "${PYTHON_CLONE}" && python3 -m mcp_agent_mail'
# <<< MCP Agent Mail alias
EOF
cat > "${TEST_HOME}/.bashrc" <<'EOF'
# baseline bashrc
EOF

cat > "${MCP_CONFIG}" <<EOF
{
  "mcpServers": {
    "mcp-agent-mail": {
      "command": "python",
      "args": ["-m", "mcp_agent_mail", "serve-http"],
      "env": {
        "HTTP_BEARER_TOKEN": "${LEGACY_TOKEN}",
        "STORAGE_ROOT": "${PYTHON_CLONE}"
      }
    }
  }
}
EOF

# Initialize a valid storage Git repo to validate post-migration fsck.
git -C "${STORAGE_ROOT}" init >/dev/null 2>&1
git -C "${STORAGE_ROOT}" config user.email "e2e@example.com"
git -C "${STORAGE_ROOT}" config user.name "E2E"
echo "seed" > "${STORAGE_ROOT}/README.md"
git -C "${STORAGE_ROOT}" add README.md
git -C "${STORAGE_ROOT}" commit -m "seed storage repo" >/dev/null 2>&1

# Create a legacy-style database with TEXT timestamps.
DATABASE_URL="sqlite+aiosqlite:///${PYTHON_DB}" "${AM_BIN}" migrate >/dev/null 2>&1
cat > "${PYTHON_CLONE}/.env" <<EOF
DATABASE_URL=sqlite+aiosqlite:///${PYTHON_DB}
HTTP_BEARER_TOKEN=${LEGACY_TOKEN}
STORAGE_ROOT=${PYTHON_CLONE}
EOF

sqlite3 "${PYTHON_DB}" <<'SQL'
INSERT INTO projects (id, slug, human_key, created_at)
VALUES (1, 'legacy-project', '/tmp/legacy-project', '2026-02-24 15:30:00.123456');

INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
VALUES
  (1, 1, 'LegacySender', 'python', 'legacy', 'sender', '2026-02-24 15:30:01', '2026-02-24 15:30:02', 'auto', 'auto'),
  (2, 1, 'LegacyReceiver', 'python', 'legacy', 'receiver', '2026-02-24 15:31:01', '2026-02-24 15:31:02', 'auto', 'auto');

INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
VALUES (1, 1, 1, 'br-28mgh.8.2', 'Legacy migration message', 'from python db', 'high', 1, '2026-02-24 15:32:00.654321', '[]');

INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts)
VALUES (1, 2, 'to', NULL, NULL);

INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts)
VALUES (1, 1, 1, 'src/legacy/**', 1, 'legacy reservation', '2026-02-24 15:33:00', '2026-12-24 15:33:00', NULL);
SQL

# ===========================================================================
# Case 1: Run installer takeover flow
# ===========================================================================
e2e_case_banner "install.sh migrates over existing Python setup"
run_installer "case_01_install"
e2e_assert_exit_code "installer exits cleanly" "0" "${INSTALL_RC}"
e2e_assert_contains "installer output mentions install destination" "${INSTALL_STDOUT}" "${DEST}"

MIGRATED_DB="${STORAGE_ROOT}/storage.sqlite3"

# ===========================================================================
# Case 2: Rust binary takeover and alias displacement
# ===========================================================================
e2e_case_banner "Rust binary is active and Python alias is disabled"
VERSION_OUT="$(HOME="${TEST_HOME}" PATH="${DEST}:${PATH_BASE}" "${DEST}/am" --version 2>&1 || true)"
e2e_save_artifact "case_02_am_version.txt" "${VERSION_OUT}"
e2e_assert_contains "am --version resolves to Rust binary" "${VERSION_OUT}" "${TARGET_VERSION}"
e2e_assert_not_contains "am --version is not Python" "${VERSION_OUT}" "python"

ZSHRC_CONTENT="$(cat "${TEST_HOME}/.zshrc" 2>/dev/null || true)"
if grep -Eq '^[[:space:]]*(alias am=|alias am |function am[[:space:](]|am[[:space:]]*\(\))' "${TEST_HOME}/.zshrc"; then
    e2e_fail "active python alias/function removed from .zshrc"
else
    e2e_pass "active python alias/function removed from .zshrc"
fi
e2e_assert_contains ".zshrc records installer disable marker" "${ZSHRC_CONTENT}" "DISABLED by Rust installer"

# ===========================================================================
# Case 3: Migrated DB has i64 timestamps and preserved content
# ===========================================================================
e2e_case_banner "timestamp migration converts TEXT values to INTEGER micros"
e2e_assert_file_exists "migrated database exists in STORAGE_ROOT" "${MIGRATED_DB}"
e2e_assert_file_exists "original python database still exists" "${PYTHON_DB}"

PROJECT_TS_TYPE="$(sqlite3 "${MIGRATED_DB}" "SELECT typeof(created_at) FROM projects WHERE id=1;")"
MESSAGE_TS_TYPE="$(sqlite3 "${MIGRATED_DB}" "SELECT typeof(created_ts) FROM messages WHERE id=1;")"
RES_TS_TYPE="$(sqlite3 "${MIGRATED_DB}" "SELECT typeof(created_ts) FROM file_reservations WHERE id=1;")"
e2e_assert_eq "projects.created_at migrated to integer" "integer" "${PROJECT_TS_TYPE}"
e2e_assert_eq "messages.created_ts migrated to integer" "integer" "${MESSAGE_TS_TYPE}"
e2e_assert_eq "file_reservations.created_ts migrated to integer" "integer" "${RES_TS_TYPE}"

MIGRATED_SUBJECT="$(sqlite3 "${MIGRATED_DB}" "SELECT subject FROM messages WHERE id=1;")"
e2e_assert_eq "message subject preserved across migration" "Legacy migration message" "${MIGRATED_SUBJECT}"

# ===========================================================================
# Case 4: Rust CLI can read migrated data end-to-end
# ===========================================================================
e2e_case_banner "CLI can access migrated projects/agents/messages/reservations"
PROJECTS_JSON="$(run_migrated_am list-projects --json 2>/dev/null || true)"
e2e_save_artifact "case_04_projects.json" "${PROJECTS_JSON}"
if json_query "${PROJECTS_JSON}" "assert any(p.get('human_key') == '/tmp/legacy-project' for p in d)"; then
    e2e_pass "list-projects includes migrated legacy project"
else
    e2e_fail "list-projects includes migrated legacy project"
fi

AGENTS_JSON="$(run_migrated_am agents list --project /tmp/legacy-project --json 2>/dev/null || true)"
e2e_save_artifact "case_04_agents.json" "${AGENTS_JSON}"
if json_query "${AGENTS_JSON}" "names={a.get('name') for a in d}; assert {'LegacySender','LegacyReceiver'}.issubset(names)"; then
    e2e_pass "agents list includes migrated legacy agents"
else
    e2e_fail "agents list includes migrated legacy agents"
fi

INBOX_JSON="$(run_migrated_am mail inbox --project /tmp/legacy-project --agent LegacyReceiver --json --include-bodies 2>/dev/null || true)"
e2e_save_artifact "case_04_inbox.json" "${INBOX_JSON}"
if json_query "${INBOX_JSON}" "assert any(m.get('subject') == 'Legacy migration message' for m in d)"; then
    e2e_pass "mail inbox exposes migrated legacy message"
else
    e2e_fail "mail inbox exposes migrated legacy message"
fi

RES_LIST="$(run_migrated_am file_reservations list /tmp/legacy-project --all 2>/dev/null || true)"
e2e_save_artifact "case_04_reservations.txt" "${RES_LIST}"
e2e_assert_contains "file_reservations list includes migrated reservation pattern" "${RES_LIST}" "src/legacy/**"

# ===========================================================================
# Case 5: MCP config rewritten away from Python entry
# ===========================================================================
e2e_case_banner "MCP config migration rewrites Python entry to Rust setup"
UPDATED_CONFIG="$(cat "${MCP_CONFIG}" 2>/dev/null || true)"
e2e_save_artifact "case_05_mcp_config.json" "${UPDATED_CONFIG}"
if json_query "${UPDATED_CONFIG}" "entry=d.get('mcpServers',{}).get('mcp-agent-mail',{}); assert entry"; then
    e2e_pass "mcp-agent-mail entry present after installer setup/update"
else
    e2e_fail "mcp-agent-mail entry present after installer setup/update"
fi

if json_query "${UPDATED_CONFIG}" "entry=d['mcpServers']['mcp-agent-mail']; cmd=entry.get('command',''); assert cmd != 'python'"; then
    e2e_pass "mcp-agent-mail config no longer points to python command"
else
    e2e_fail "mcp-agent-mail config no longer points to python command"
fi

# Token parity check: migrated env + MCP config should still reference legacy token.
MIGRATED_ENV="${TEST_HOME}/.config/mcp-agent-mail/config.env"
e2e_assert_file_exists "migrated env config exists" "${MIGRATED_ENV}"
MIGRATED_TOKEN="$(grep -E '^HTTP_BEARER_TOKEN=' "${MIGRATED_ENV}" | head -1 | cut -d= -f2-)"
e2e_assert_eq "bearer token preserved in migrated env config" "${LEGACY_TOKEN}" "${MIGRATED_TOKEN}"

if json_query "${UPDATED_CONFIG}" "entry=d['mcpServers']['mcp-agent-mail']; auth=((entry.get('headers') or {}).get('Authorization','')); env=((entry.get('env') or {}).get('HTTP_BEARER_TOKEN','')); assert ('${LEGACY_TOKEN}' in auth) or (env == '${LEGACY_TOKEN}')"; then
    e2e_pass "MCP config carries legacy bearer token"
else
    e2e_fail "MCP config carries legacy bearer token"
fi

# ===========================================================================
# Case 6: Doctor + backup artifacts + Git health
# ===========================================================================
e2e_case_banner "doctor health, backup artifacts, and storage git integrity"
DOCTOR_JSON="$(run_migrated_am doctor check --json 2>/dev/null || true)"
DOCTOR_RC=$?
e2e_save_artifact "case_06_doctor.json" "${DOCTOR_JSON}"
e2e_assert_exit_code "doctor check exits cleanly" "0" "${DOCTOR_RC}"
if python3 -c "import json,sys; json.loads(sys.stdin.read())" <<< "${DOCTOR_JSON}" >/dev/null 2>&1; then
    e2e_pass "doctor check emits valid JSON"
else
    e2e_fail "doctor check emits valid JSON"
fi

BACKUP_PATH="$(find "${STORAGE_ROOT}" -maxdepth 1 -type f -name 'storage.sqlite3.backup-*' | sort | head -1 || true)"
if [ -n "${BACKUP_PATH}" ] && [ -f "${BACKUP_PATH}" ]; then
    e2e_pass "timestamp migration backup artifact created"
else
    e2e_fail "timestamp migration backup artifact created"
fi

set +e
git -C "${STORAGE_ROOT}" fsck --no-progress >/dev/null 2>&1
FSCK_RC=$?
set -e
e2e_assert_exit_code "storage root git repository remains healthy" "0" "${FSCK_RC}"

# ===========================================================================
# Case 7: Docker harness definition exists (optional build smoke)
# ===========================================================================
e2e_case_banner "docker harness file is present for containerized migration runs"
DOCKERFILE_PATH="${SCRIPT_DIR}/Dockerfile.migration"
e2e_assert_file_exists "Dockerfile.migration exists" "${DOCKERFILE_PATH}"

if command -v docker >/dev/null 2>&1 && [ "${AM_E2E_VALIDATE_DOCKER:-0}" = "1" ]; then
    set +e
    docker build -f "${DOCKERFILE_PATH}" -t mcp-agent-mail-migration-e2e "${E2E_PROJECT_ROOT}" >/dev/null 2>&1
    DOCKER_RC=$?
    set -e
    e2e_assert_exit_code "Dockerfile.migration builds successfully" "0" "${DOCKER_RC}"
else
    e2e_skip "docker build validation skipped (set AM_E2E_VALIDATE_DOCKER=1 to enable)"
fi

e2e_summary
exit $?
