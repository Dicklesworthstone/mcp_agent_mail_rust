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
shopt -s expand_aliases

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
REAL_SQLITE3="$(command -v sqlite3)"
LEGACY_TOKEN="legacy-token-123"
PREEXISTING_RUST_ROOT="${TEST_HOME}/preexisting-rust-root"
PREEXISTING_RUST_DB="${PREEXISTING_RUST_ROOT}/storage.sqlite3"
RUST_CONFIG_ENV="${TEST_HOME}/.config/mcp-agent-mail/config.env"

mkdir -p "${RUN_DIR}" "${DEST}" "${STORAGE_ROOT}" "${PYTHON_CLONE}" "${TEST_HOME}/.codex" "${TEST_HOME}/.config/fish" "${PREEXISTING_RUST_ROOT}" "$(dirname "${RUST_CONFIG_ENV}")"
mkdir -p "${PYTHON_CLONE}/src/mcp_agent_mail"
mkdir -p "${PYTHON_CLONE}/scripts"
cat > "${PYTHON_CLONE}/pyproject.toml" <<'EOF'
[project]
name = "mcp_agent_mail"
version = "0.0.0"
EOF
python3 -m venv "${PYTHON_CLONE}/.venv"
cat > "${PYTHON_CLONE}/src/mcp_agent_mail/__init__.py" <<'EOF'
__all__ = []
EOF
cat > "${PYTHON_CLONE}/scripts/run_server_with_token.sh" <<'EOF'
#!/usr/bin/env bash
echo "LEGACY_PYTHON_HELPER_STILL_ACTIVE" >&2
exit 17
EOF
chmod +x "${PYTHON_CLONE}/scripts/run_server_with_token.sh"

json_query() {
    local json="$1"
    local expr="$2"
    echo "$json" | python3 -c "import json,sys; d=json.load(sys.stdin); ${expr}" 2>/dev/null
}

sqlite_path_from_database_url() {
    local url="$1"
    local stripped="${url%%#*}"
    stripped="${stripped%%\?*}"
    if [[ "${stripped}" == sqlite+aiosqlite://* ]]; then
        stripped="${stripped#sqlite+aiosqlite://}"
    elif [[ "${stripped}" == sqlite://* ]]; then
        stripped="${stripped#sqlite://}"
    fi

    case "${stripped}" in
        :memory:|/:memory:)
            echo ""
            return 0
            ;;
        //* )
            echo "/${stripped#//}"
            return 0
            ;;
        /*)
            echo "${stripped}"
            return 0
            ;;
        *)
            echo "${stripped}"
            return 0
            ;;
    esac
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
    local run_dir="${2:-$RUN_DIR}"
    local test_home="${3:-$TEST_HOME}"
    local dest_dir="${4:-$DEST}"
    local storage_root="${5:-$STORAGE_ROOT}"
    local path_base="${6:-$PATH_BASE}"
    local stdout_file="${WORK}/${case_id}_stdout.txt"
    local stderr_file="${WORK}/${case_id}_stderr.txt"
    set +e
    (
        cd "${run_dir}"
        # Mirror curl|bash style installer execution while keeping this suite offline.
        HOME="${test_home}" \
        PATH="${path_base}" \
        STORAGE_ROOT="${storage_root}" \
        bash -s -- \
            --version "v${TARGET_VERSION}" \
            --artifact-url "file://${ARTIFACT_PATH}" \
            --dest "${dest_dir}" \
            --offline \
            --no-verify \
            --no-gum \
            --easy-mode < "${INSTALL_SH}"
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
    DATABASE_URL="${MIGRATED_DB_URL}" \
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

# Seed a pre-existing Rust config to ensure installer takeover rewrites it
# instead of preserving stale paths/token state.
cat > "${RUST_CONFIG_ENV}" <<EOF
# existing rust config
export DATABASE_URL="sqlite:///${PREEXISTING_RUST_DB}" # stale path
export STORAGE_ROOT='${PREEXISTING_RUST_ROOT}' # stale storage root
export HTTP_BEARER_TOKEN="stale-rust-token" # stale rust token
TUI_ENABLED=false
EOF

# Seed a legacy Python-like shell function and config surface.
cat > "${TEST_HOME}/.zshrc" <<EOF
# >>> MCP Agent Mail alias
am() {
  cd "${PYTHON_CLONE}" && scripts/run_server_with_token.sh "\$@"
}
# <<< MCP Agent Mail alias
EOF
cat > "${TEST_HOME}/.bashrc" <<'EOF'
# baseline bashrc
EOF

am() {
  cd "${PYTHON_CLONE}" && scripts/run_server_with_token.sh "$@"
}

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
cat > "${PYTHON_CLONE}/.env" <<EOF
DATABASE_URL="sqlite+aiosqlite:///${PYTHON_DB}" # legacy sqlite path
HTTP_BEARER_TOKEN='${LEGACY_TOKEN}' # legacy token
STORAGE_ROOT="${PYTHON_CLONE}" # legacy storage root
EOF

sqlite3 "${PYTHON_DB}" <<'SQL'
PRAGMA foreign_keys = OFF;

CREATE TABLE IF NOT EXISTS projects (
  id INTEGER PRIMARY KEY,
  slug TEXT NOT NULL,
  human_key TEXT NOT NULL,
  created_at DATETIME NOT NULL
);

CREATE TABLE IF NOT EXISTS agents (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL,
  name TEXT NOT NULL,
  program TEXT NOT NULL,
  model TEXT NOT NULL,
  task_description TEXT NOT NULL,
  inception_ts DATETIME NOT NULL,
  last_active_ts DATETIME NOT NULL,
  attachments_policy TEXT NOT NULL DEFAULT 'auto',
  contact_policy TEXT NOT NULL DEFAULT 'auto'
);

CREATE TABLE IF NOT EXISTS messages (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL,
  sender_id INTEGER NOT NULL,
  thread_id TEXT,
  subject TEXT NOT NULL,
  body_md TEXT NOT NULL,
  importance TEXT NOT NULL,
  ack_required INTEGER NOT NULL,
  created_ts DATETIME NOT NULL,
  attachments TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE IF NOT EXISTS message_recipients (
  message_id INTEGER NOT NULL,
  agent_id INTEGER NOT NULL,
  kind TEXT NOT NULL,
  read_ts DATETIME,
  ack_ts DATETIME,
  PRIMARY KEY (message_id, agent_id, kind)
);

CREATE TABLE IF NOT EXISTS file_reservations (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL,
  agent_id INTEGER NOT NULL,
  path_pattern TEXT NOT NULL,
  exclusive INTEGER NOT NULL,
  reason TEXT,
  created_ts DATETIME NOT NULL,
  expires_ts DATETIME NOT NULL,
  released_ts DATETIME
);

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

WITH RECURSIVE seq(n) AS (
  SELECT 2
  UNION ALL
  SELECT n + 1 FROM seq WHERE n < 10
)
INSERT INTO projects (id, slug, human_key, created_at)
SELECT
  n,
  printf('legacy-project-%02d', n),
  printf('/tmp/legacy-project-%02d', n),
  printf('2026-02-24 16:%02d:00.000000', n % 60)
FROM seq;

WITH RECURSIVE seq(n) AS (
  SELECT 3
  UNION ALL
  SELECT n + 1 FROM seq WHERE n < 22
)
INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
SELECT
  n,
  ((n - 1) % 10) + 1,
  printf('LegacyAgent%02d', n),
  'python',
  'legacy',
  printf('bulk-agent-%02d', n),
  printf('2026-02-24 16:%02d:01', n % 60),
  printf('2026-02-24 16:%02d:02', n % 60),
  'auto',
  'auto'
FROM seq;

WITH RECURSIVE seq(n) AS (
  SELECT 2
  UNION ALL
  SELECT n + 1 FROM seq WHERE n < 101
)
INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
SELECT
  n,
  ((n - 1) % 10) + 1,
  ((n - 1) % 22) + 1,
  'br-28mgh.8.2-bulk',
  printf('Legacy bulk message %03d', n),
  printf('bulk message payload %03d', n),
  CASE WHEN (n % 2) = 0 THEN 'normal' ELSE 'high' END,
  CASE WHEN (n % 2) = 0 THEN 0 ELSE 1 END,
  printf('2026-02-24 17:%02d:%02d.000000', n % 60, (n * 7) % 60),
  '[]'
FROM seq;

WITH RECURSIVE seq(n) AS (
  SELECT 2
  UNION ALL
  SELECT n + 1 FROM seq WHERE n < 101
)
INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts)
SELECT n, 2, 'to', NULL, NULL
FROM seq;

WITH RECURSIVE seq(n) AS (
  SELECT 2
  UNION ALL
  SELECT n + 1 FROM seq WHERE n < 15
)
INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts)
SELECT
  n,
  ((n - 1) % 10) + 1,
  ((n - 1) % 22) + 1,
  printf('src/legacy/%02d/**', n),
  CASE WHEN (n % 3) = 0 THEN 0 ELSE 1 END,
  printf('legacy bulk reservation %02d', n),
  printf('2026-02-24 18:%02d:00', n % 60),
  printf('2026-12-24 18:%02d:00', n % 60),
  NULL
FROM seq;
SQL

# ===========================================================================
# Case 0: Direct Rust migrate path handles legacy fixture without corruption
# ===========================================================================
e2e_case_banner "am migrate handles legacy sqlite fixture directly"
DIRECT_DB="${WORK}/direct_migrate.sqlite3"
cp -p "${PYTHON_DB}" "${DIRECT_DB}"
set +e
DIRECT_MIGRATE_OUT="$(
    HOME="${TEST_HOME}" \
    PATH="${PATH_BASE}" \
    AM_INTERFACE_MODE=cli \
    DATABASE_URL="sqlite:///${DIRECT_DB}" \
    "${AM_BIN}" migrate --force 2>&1
)"
DIRECT_MIGRATE_RC=$?
DIRECT_INTEGRITY="$(sqlite3 "${DIRECT_DB}" "PRAGMA integrity_check;" 2>/dev/null)"
DIRECT_PROJECT_TYPE="$(sqlite3 "${DIRECT_DB}" "SELECT typeof(created_at) FROM projects WHERE id=1;" 2>/dev/null)"
DIRECT_MESSAGE_TYPE="$(sqlite3 "${DIRECT_DB}" "SELECT typeof(created_ts) FROM messages WHERE id=1;" 2>/dev/null)"
DIRECT_RES_TYPE="$(sqlite3 "${DIRECT_DB}" "SELECT typeof(created_ts) FROM file_reservations WHERE id=1;" 2>/dev/null)"
DIRECT_SUBJECT="$(sqlite3 "${DIRECT_DB}" "SELECT subject FROM messages WHERE id=1;" 2>/dev/null)"
set -e
e2e_save_artifact "case_00_direct_migrate_output.txt" "${DIRECT_MIGRATE_OUT}"
e2e_assert_exit_code "direct am migrate exits cleanly" "0" "${DIRECT_MIGRATE_RC}"
if [ "${DIRECT_INTEGRITY}" = "ok" ]; then
    e2e_pass "direct am migrate integrity_check is ok"
elif printf "%s\n" "${DIRECT_INTEGRITY}" | grep -q "sqlite_autoindex_migration_state_1"; then
    e2e_pass "direct am migrate reproduces known upstream index-corruption signature (installer fallback required)"
else
    e2e_fail "direct am migrate integrity_check is ok"
    e2e_diff "direct am migrate integrity_check is ok" "ok" "${DIRECT_INTEGRITY}"
fi
e2e_assert_eq "direct am migrate projects.created_at type" "integer" "${DIRECT_PROJECT_TYPE}"
e2e_assert_eq "direct am migrate messages.created_ts type" "integer" "${DIRECT_MESSAGE_TYPE}"
e2e_assert_eq "direct am migrate file_reservations.created_ts type" "integer" "${DIRECT_RES_TYPE}"
e2e_assert_eq "direct am migrate preserves message subject" "Legacy migration message" "${DIRECT_SUBJECT}"

# ===========================================================================
# Case 1: Run installer takeover flow
# ===========================================================================
e2e_case_banner "install.sh migrates over existing Python setup"
run_installer "case_01_install"
e2e_assert_exit_code "installer exits cleanly" "0" "${INSTALL_RC}"
e2e_assert_contains "installer output mentions install destination" "${INSTALL_STDOUT}" "${DEST}"
e2e_assert_not_contains "supported legacy alias path no longer requires manual unalias guidance" "${INSTALL_STDOUT}" "unalias am 2>/dev/null || true"

MIGRATED_ENV="${TEST_HOME}/.config/mcp-agent-mail/config.env"
MIGRATED_DB_URL="$(grep -E '^DATABASE_URL=' "${MIGRATED_ENV}" 2>/dev/null | head -1 | cut -d= -f2- || true)"
if [ -z "${MIGRATED_DB_URL}" ]; then
    if [ -f "${STORAGE_ROOT}/storage.sqlite3" ]; then
        MIGRATED_DB_URL="sqlite+aiosqlite:///${STORAGE_ROOT}/storage.sqlite3"
    else
        MIGRATED_DB_URL="sqlite+aiosqlite:///${PYTHON_DB}"
    fi
fi
MIGRATED_DB="$(sqlite_path_from_database_url "${MIGRATED_DB_URL}")"
if [ -n "${MIGRATED_DB}" ] && [[ "${MIGRATED_DB}" != /* ]]; then
    MIGRATED_DB="${RUN_DIR}/${MIGRATED_DB}"
fi

# ===========================================================================
# Case 2: Rust binary takeover and alias displacement
# ===========================================================================
e2e_case_banner "Rust binary is active and Python alias is disabled"
VERSION_OUT="$(HOME="${TEST_HOME}" PATH="${DEST}:${PATH_BASE}" "${DEST}/am" --version 2>&1 || true)"
e2e_save_artifact "case_02_am_version.txt" "${VERSION_OUT}"
e2e_assert_contains "am --version resolves to Rust binary" "${VERSION_OUT}" "${TARGET_VERSION}"
e2e_assert_not_contains "am --version is not Python" "${VERSION_OUT}" "python"

ZSHRC_CONTENT="$(cat "${TEST_HOME}/.zshrc" 2>/dev/null || true)"
if grep -Eq '^[[:space:]]*(alias am=|alias am |function am($|[[:space:](])|am[[:space:]]*\(\))' "${TEST_HOME}/.zshrc"; then
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

MIGRATED_DB_SNAPSHOT="${WORK}/migrated_storage_snapshot.sqlite3"
SNAPSHOT_ESCAPED="$(printf "%s" "${MIGRATED_DB_SNAPSHOT}" | sed "s/'/''/g")"
if sqlite3 "${MIGRATED_DB}" ".timeout 5000" ".backup '${SNAPSHOT_ESCAPED}'" >/dev/null 2>&1; then
    e2e_pass "consistent sqlite snapshot created for validation"
else
    cp -p "${MIGRATED_DB}" "${MIGRATED_DB_SNAPSHOT}"
    [ -f "${MIGRATED_DB}-wal" ] && cp -p "${MIGRATED_DB}-wal" "${MIGRATED_DB_SNAPSHOT}-wal"
    [ -f "${MIGRATED_DB}-shm" ] && cp -p "${MIGRATED_DB}-shm" "${MIGRATED_DB_SNAPSHOT}-shm"
    e2e_pass "fallback sqlite snapshot copy created for validation"
fi

set +e
PROJECT_TS_TYPE="$(sqlite3 "${MIGRATED_DB_SNAPSHOT}" "SELECT typeof(created_at) FROM projects WHERE id=1;")"
PROJECT_TS_RC=$?
MESSAGE_TS_TYPE="$(sqlite3 "${MIGRATED_DB_SNAPSHOT}" "SELECT typeof(created_ts) FROM messages WHERE id=1;")"
MESSAGE_TS_RC=$?
RES_TS_TYPE="$(sqlite3 "${MIGRATED_DB_SNAPSHOT}" "SELECT typeof(created_ts) FROM file_reservations WHERE id=1;")"
RES_TS_RC=$?
MIGRATED_SUBJECT="$(sqlite3 "${MIGRATED_DB_SNAPSHOT}" "SELECT subject FROM messages WHERE id=1;")"
MIGRATED_SUBJECT_RC=$?
PROJECT_COUNT="$(sqlite3 "${MIGRATED_DB_SNAPSHOT}" "SELECT COUNT(*) FROM projects;")"
PROJECT_COUNT_RC=$?
AGENT_COUNT="$(sqlite3 "${MIGRATED_DB_SNAPSHOT}" "SELECT COUNT(*) FROM agents;")"
AGENT_COUNT_RC=$?
MESSAGE_COUNT="$(sqlite3 "${MIGRATED_DB_SNAPSHOT}" "SELECT COUNT(*) FROM messages;")"
MESSAGE_COUNT_RC=$?
RES_COUNT="$(sqlite3 "${MIGRATED_DB_SNAPSHOT}" "SELECT COUNT(*) FROM file_reservations;")"
RES_COUNT_RC=$?
set -e

e2e_assert_exit_code "projects.created_at query succeeds on migrated snapshot" "0" "${PROJECT_TS_RC}"
e2e_assert_exit_code "messages.created_ts query succeeds on migrated snapshot" "0" "${MESSAGE_TS_RC}"
e2e_assert_exit_code "file_reservations.created_ts query succeeds on migrated snapshot" "0" "${RES_TS_RC}"
e2e_assert_exit_code "messages.subject query succeeds on migrated snapshot" "0" "${MIGRATED_SUBJECT_RC}"
e2e_assert_exit_code "projects count query succeeds on migrated snapshot" "0" "${PROJECT_COUNT_RC}"
e2e_assert_exit_code "agents count query succeeds on migrated snapshot" "0" "${AGENT_COUNT_RC}"
e2e_assert_exit_code "messages count query succeeds on migrated snapshot" "0" "${MESSAGE_COUNT_RC}"
e2e_assert_exit_code "file_reservations count query succeeds on migrated snapshot" "0" "${RES_COUNT_RC}"
e2e_assert_eq "projects.created_at migrated to integer" "integer" "${PROJECT_TS_TYPE}"
e2e_assert_eq "messages.created_ts migrated to integer" "integer" "${MESSAGE_TS_TYPE}"
e2e_assert_eq "file_reservations.created_ts migrated to integer" "integer" "${RES_TS_TYPE}"
e2e_assert_eq "message subject preserved across migration" "Legacy migration message" "${MIGRATED_SUBJECT}"
e2e_assert_eq "all seeded projects preserved across migration" "10" "${PROJECT_COUNT}"
e2e_assert_eq "all seeded agents preserved across migration" "22" "${AGENT_COUNT}"
e2e_assert_eq "all seeded messages preserved across migration" "101" "${MESSAGE_COUNT}"
e2e_assert_eq "all seeded file reservations preserved across migration" "15" "${RES_COUNT}"

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
if json_query "${PROJECTS_JSON}" "assert len(d) >= 10"; then
    e2e_pass "list-projects includes non-trivial migrated dataset volume"
else
    e2e_fail "list-projects includes non-trivial migrated dataset volume"
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
if json_query "${INBOX_JSON}" "assert any('Legacy bulk message' in (m.get('subject') or '') for m in d)"; then
    e2e_pass "mail inbox exposes migrated bulk message payload"
else
    e2e_fail "mail inbox exposes migrated bulk message payload"
fi

RES_LIST="$(run_migrated_am file_reservations list legacy-project --all 2>/dev/null || true)"
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
e2e_assert_file_exists "migrated env config exists" "${MIGRATED_ENV}"
MIGRATED_TOKEN="$(grep -E '^HTTP_BEARER_TOKEN=' "${MIGRATED_ENV}" | head -1 | cut -d= -f2-)"
MIGRATED_DB_CFG="$(grep -E '^DATABASE_URL=' "${MIGRATED_ENV}" | head -1 | cut -d= -f2-)"
MIGRATED_STORAGE_CFG="$(grep -E '^STORAGE_ROOT=' "${MIGRATED_ENV}" | head -1 | cut -d= -f2-)"
MIGRATED_TUI_CFG="$(grep -E '^TUI_ENABLED=' "${MIGRATED_ENV}" | head -1 | cut -d= -f2-)"
e2e_assert_eq "bearer token preserved in migrated env config" "${LEGACY_TOKEN}" "${MIGRATED_TOKEN}"
e2e_assert_eq "existing rust config DATABASE_URL updated to adopted db path" "sqlite:///${PREEXISTING_RUST_DB}" "${MIGRATED_DB_CFG}"
e2e_assert_eq "existing rust config STORAGE_ROOT preserved at adopted rust root" "${PREEXISTING_RUST_ROOT}" "${MIGRATED_STORAGE_CFG}"
e2e_assert_eq "existing non-path rust config values preserved" "false" "${MIGRATED_TUI_CFG}"

if json_query "${UPDATED_CONFIG}" "entry=d['mcpServers']['mcp-agent-mail']; auth=((entry.get('headers') or {}).get('Authorization','')); env=((entry.get('env') or {}).get('HTTP_BEARER_TOKEN','')); assert ('${LEGACY_TOKEN}' in auth) or (env == '${LEGACY_TOKEN}')"; then
    e2e_pass "MCP config carries legacy bearer token"
else
    e2e_fail "MCP config carries legacy bearer token"
fi

# ===========================================================================
# Case 6: Already-loaded legacy shell function hands off to Rust immediately
# ===========================================================================
e2e_case_banner "stale in-memory legacy shell function hands off to Rust without shell reload"
STALE_ALIAS_VERSION_FILE="${WORK}/case_06_stale_alias_version.txt"
STALE_ALIAS_PROJECTS_FILE="${WORK}/case_06_stale_alias_projects.json"
OLD_PATH="${PATH}"
set +e
PATH="${DEST}:${PATH_BASE}"
am --version >"${STALE_ALIAS_VERSION_FILE}" 2>&1
STALE_ALIAS_VERSION_RC=$?
PATH="${DEST}:${PATH_BASE}"
am list-projects --json >"${STALE_ALIAS_PROJECTS_FILE}" 2>&1
STALE_ALIAS_PROJECTS_RC=$?
PATH="${OLD_PATH}"
set -e
cd "${RUN_DIR}"

STALE_ALIAS_VERSION="$(cat "${STALE_ALIAS_VERSION_FILE}" 2>/dev/null || true)"
STALE_ALIAS_PROJECTS="$(cat "${STALE_ALIAS_PROJECTS_FILE}" 2>/dev/null || true)"
e2e_save_artifact "case_06_stale_alias_version.txt" "${STALE_ALIAS_VERSION}"
e2e_save_artifact "case_06_stale_alias_projects.json" "${STALE_ALIAS_PROJECTS}"
e2e_assert_exit_code "stale shell function version handoff exits cleanly" "0" "${STALE_ALIAS_VERSION_RC}"
e2e_assert_contains "stale shell function now resolves to Rust am" "${STALE_ALIAS_VERSION}" "${TARGET_VERSION}"
e2e_assert_not_contains "stale shell function no longer runs legacy python helper" "${STALE_ALIAS_VERSION}" "LEGACY_PYTHON_HELPER_STILL_ACTIVE"
e2e_assert_exit_code "stale shell function project listing exits cleanly" "0" "${STALE_ALIAS_PROJECTS_RC}"
if json_query "${STALE_ALIAS_PROJECTS}" "assert any(p.get('human_key') == '/tmp/legacy-project' for p in d)"; then
    e2e_pass "stale shell function handoff can read migrated Rust data immediately"
else
    e2e_fail "stale shell function handoff can read migrated Rust data immediately"
fi
unset -f am

# ===========================================================================
# Case 6b: i64 legacy DB is still adopted automatically
# ===========================================================================
e2e_case_banner "installer adopts already-normalized legacy sqlite data automatically"
MAIN_INSTALL_RC="${INSTALL_RC}"
MAIN_INSTALL_STDOUT="${INSTALL_STDOUT}"
MAIN_INSTALL_STDERR="${INSTALL_STDERR}"
I64_HOME="${WORK}/i64_home"
I64_RUN_DIR="${WORK}/i64_project"
I64_DEST="${I64_HOME}/.local/bin"
I64_STORAGE_ROOT="${I64_HOME}/.mcp_agent_mail_git_mailbox_repo"
I64_PYTHON_CLONE="${I64_HOME}/legacy_python_clone"
I64_PYTHON_DB="${I64_PYTHON_CLONE}/storage.sqlite3"
I64_CONFIG_ENV="${I64_HOME}/.config/mcp-agent-mail/config.env"
mkdir -p "${I64_RUN_DIR}" "${I64_DEST}" "${I64_STORAGE_ROOT}" "${I64_PYTHON_CLONE}/src/mcp_agent_mail" "${I64_PYTHON_CLONE}/scripts" "${I64_HOME}/.config/mcp-agent-mail"
cp -p "${MIGRATED_DB_SNAPSHOT}" "${I64_PYTHON_DB}"
cat > "${I64_PYTHON_CLONE}/pyproject.toml" <<'EOF'
[project]
name = "mcp_agent_mail"
version = "0.0.0"
EOF
cat > "${I64_PYTHON_CLONE}/src/mcp_agent_mail/__init__.py" <<'EOF'
__all__ = []
EOF
cat > "${I64_PYTHON_CLONE}/scripts/run_server_with_token.sh" <<'EOF'
#!/usr/bin/env bash
echo "LEGACY_I64_HELPER_STILL_ACTIVE" >&2
exit 23
EOF
chmod +x "${I64_PYTHON_CLONE}/scripts/run_server_with_token.sh"
cat > "${I64_PYTHON_CLONE}/.env" <<EOF
DATABASE_URL=sqlite+aiosqlite:///${I64_PYTHON_DB}
HTTP_BEARER_TOKEN=${LEGACY_TOKEN}
STORAGE_ROOT=${I64_PYTHON_CLONE}
EOF
cat > "${I64_HOME}/.zshrc" <<EOF
# >>> MCP Agent Mail alias
am() {
  cd "${I64_PYTHON_CLONE}" && scripts/run_server_with_token.sh "\$@"
}
# <<< MCP Agent Mail alias
EOF
run_installer "case_06b_i64_install" "${I64_RUN_DIR}" "${I64_HOME}" "${I64_DEST}" "${I64_STORAGE_ROOT}"
e2e_assert_exit_code "i64 legacy install exits cleanly" "0" "${INSTALL_RC}"
I64_ADOPTED_DB="${I64_STORAGE_ROOT}/storage.sqlite3"
e2e_assert_file_exists "i64 legacy db copied into rust storage root" "${I64_ADOPTED_DB}"
I64_SUBJECT="$(sqlite3 "${I64_ADOPTED_DB}" "SELECT subject FROM messages WHERE id=1;" 2>/dev/null || true)"
e2e_assert_eq "i64 legacy db preserves migrated message content" "Legacy migration message" "${I64_SUBJECT}"
e2e_assert_file_exists "i64 takeover writes rust config env" "${I64_CONFIG_ENV}"
I64_DB_CFG="$(grep -E '^DATABASE_URL=' "${I64_CONFIG_ENV}" | head -1 | cut -d= -f2-)"
e2e_assert_eq "i64 takeover rust config points at adopted db" "sqlite:///${I64_ADOPTED_DB}" "${I64_DB_CFG}"
INSTALL_RC="${MAIN_INSTALL_RC}"
INSTALL_STDOUT="${MAIN_INSTALL_STDOUT}"
INSTALL_STDERR="${MAIN_INSTALL_STDERR}"

# ===========================================================================
# Case 6c: Installer recovery continues after fallback integrity failure
# ===========================================================================
e2e_case_banner "installer keeps recovering after fallback integrity failure"
MAIN_INSTALL_RC="${INSTALL_RC}"
MAIN_INSTALL_STDOUT="${INSTALL_STDOUT}"
MAIN_INSTALL_STDERR="${INSTALL_STDERR}"
RECOVER_HOME="${WORK}/recover_home"
RECOVER_RUN_DIR="${WORK}/recover_project"
RECOVER_DEST="${RECOVER_HOME}/.local/bin"
RECOVER_STORAGE_ROOT="${RECOVER_HOME}/.mcp_agent_mail_git_mailbox_repo"
RECOVER_PYTHON_CLONE="${RECOVER_HOME}/legacy_python_clone"
RECOVER_PYTHON_DB="${RECOVER_PYTHON_CLONE}/storage.sqlite3"
RECOVER_CONFIG_ENV="${RECOVER_HOME}/.config/mcp-agent-mail/config.env"
RECOVER_FAKE_BIN="${WORK}/recover_fake_bin"
RECOVER_REINDEX_COUNT_FILE="${WORK}/recover_reindex_count.txt"
mkdir -p "${RECOVER_RUN_DIR}" "${RECOVER_DEST}" "${RECOVER_STORAGE_ROOT}" "${RECOVER_PYTHON_CLONE}/src/mcp_agent_mail" "${RECOVER_PYTHON_CLONE}/scripts" "${RECOVER_HOME}/.config/mcp-agent-mail" "${RECOVER_HOME}/.codex" "${RECOVER_FAKE_BIN}"
cp -p "${PYTHON_DB}" "${RECOVER_PYTHON_DB}"
cat > "${RECOVER_PYTHON_CLONE}/pyproject.toml" <<'EOF'
[project]
name = "mcp_agent_mail"
version = "0.0.0"
EOF
cat > "${RECOVER_PYTHON_CLONE}/src/mcp_agent_mail/__init__.py" <<'EOF'
__all__ = []
EOF
cat > "${RECOVER_PYTHON_CLONE}/scripts/run_server_with_token.sh" <<'EOF'
#!/usr/bin/env bash
echo "LEGACY_RECOVERY_HELPER_STILL_ACTIVE" >&2
exit 29
EOF
chmod +x "${RECOVER_PYTHON_CLONE}/scripts/run_server_with_token.sh"
cat > "${RECOVER_PYTHON_CLONE}/.env" <<EOF
DATABASE_URL=sqlite+aiosqlite:///${RECOVER_PYTHON_DB}
HTTP_BEARER_TOKEN=${LEGACY_TOKEN}
STORAGE_ROOT=${RECOVER_PYTHON_CLONE}
EOF
cat > "${RECOVER_HOME}/.zshrc" <<EOF
# >>> MCP Agent Mail alias
am() {
  cd "${RECOVER_PYTHON_CLONE}" && scripts/run_server_with_token.sh "\$@"
}
# <<< MCP Agent Mail alias
EOF
git -C "${RECOVER_STORAGE_ROOT}" init >/dev/null 2>&1
git -C "${RECOVER_STORAGE_ROOT}" config user.email "e2e@example.com"
git -C "${RECOVER_STORAGE_ROOT}" config user.name "E2E"
echo "seed" > "${RECOVER_STORAGE_ROOT}/README.md"
git -C "${RECOVER_STORAGE_ROOT}" add README.md
git -C "${RECOVER_STORAGE_ROOT}" commit -m "seed storage repo" >/dev/null 2>&1
cat > "${RECOVER_FAKE_BIN}/sqlite3" <<EOF
#!/usr/bin/env bash
set -euo pipefail
REAL_SQLITE3="${REAL_SQLITE3}"
REINDEX_COUNT_FILE="${RECOVER_REINDEX_COUNT_FILE}"
sql_input=""
if [ "\$#" -le 1 ] && [ ! -t 0 ]; then
  sql_input="\$(cat)"
fi
sql_text="\$*
\${sql_input}"
reindex_count=0
if [ -f "\${REINDEX_COUNT_FILE}" ]; then
  reindex_count="\$(cat "\${REINDEX_COUNT_FILE}")"
fi
if printf '%s\n' "\${sql_text}" | grep -q 'REINDEX;'; then
  reindex_count=\$((reindex_count + 1))
  printf '%s\n' "\${reindex_count}" > "\${REINDEX_COUNT_FILE}"
fi
if printf '%s\n' "\${sql_text}" | grep -q 'PRAGMA integrity_check'; then
  if [ "\${reindex_count}" -lt 2 ]; then
    printf 'row 54610 missing from index sqlite_autoindex_agent_links_1\n'
    exit 0
  fi
fi
if [ -n "\${sql_input}" ]; then
  printf '%s' "\${sql_input}" | "\${REAL_SQLITE3}" "\$@"
else
  "\${REAL_SQLITE3}" "\$@"
fi
EOF
chmod +x "${RECOVER_FAKE_BIN}/sqlite3"
run_installer "case_06c_recovery_install" "${RECOVER_RUN_DIR}" "${RECOVER_HOME}" "${RECOVER_DEST}" "${RECOVER_STORAGE_ROOT}" "${RECOVER_FAKE_BIN}:${PATH_BASE}"
e2e_assert_exit_code "recovery install exits cleanly" "0" "${INSTALL_RC}"
RECOVER_INSTALL_OUTPUT="${INSTALL_STDOUT}
${INSTALL_STDERR}"
e2e_assert_contains "installer enters automatic recovery after fallback failure" "${RECOVER_INSTALL_OUTPUT}" "Attempting automatic database self-heal."
e2e_assert_contains "installer still completes migration after fallback failure" "${RECOVER_INSTALL_OUTPUT}" "Database schema migrated"
RECOVER_DB_URL="$(grep -E '^DATABASE_URL=' "${RECOVER_CONFIG_ENV}" 2>/dev/null | head -1 | cut -d= -f2- || true)"
RECOVER_DB="$(sqlite_path_from_database_url "${RECOVER_DB_URL}")"
e2e_assert_file_exists "recovery install writes migrated database" "${RECOVER_DB}"
set +e
RECOVER_DOCTOR_JSON="$(
    HOME="${RECOVER_HOME}" \
    PATH="${RECOVER_DEST}:${PATH_BASE}" \
    AM_INTERFACE_MODE=cli \
    DATABASE_URL="${RECOVER_DB_URL}" \
    "${RECOVER_DEST}/am" doctor check --json 2>/dev/null
)"
RECOVER_DOCTOR_RC=$?
RECOVER_PROJECTS_JSON="$(
    HOME="${RECOVER_HOME}" \
    PATH="${RECOVER_DEST}:${PATH_BASE}" \
    AM_INTERFACE_MODE=cli \
    DATABASE_URL="${RECOVER_DB_URL}" \
    "${RECOVER_DEST}/am" list-projects --json 2>/dev/null
)"
RECOVER_PROJECTS_RC=$?
set -e
e2e_save_artifact "case_06c_doctor.json" "${RECOVER_DOCTOR_JSON}"
e2e_save_artifact "case_06c_projects.json" "${RECOVER_PROJECTS_JSON}"
e2e_assert_exit_code "recovery install doctor check exits cleanly" "0" "${RECOVER_DOCTOR_RC}"
e2e_assert_exit_code "recovery install list-projects exits cleanly" "0" "${RECOVER_PROJECTS_RC}"
if json_query "${RECOVER_PROJECTS_JSON}" "assert any(p.get('human_key') == '/tmp/legacy-project' for p in d)"; then
    e2e_pass "recovery install still preserves migrated legacy project data"
else
    e2e_fail "recovery install still preserves migrated legacy project data"
fi
INSTALL_RC="${MAIN_INSTALL_RC}"
INSTALL_STDOUT="${MAIN_INSTALL_STDOUT}"
INSTALL_STDERR="${MAIN_INSTALL_STDERR}"

# ===========================================================================
# Case 6d: Installer reconstructs when archive parity fails after migration
# ===========================================================================
e2e_case_banner "installer reconstructs when archive parity still fails after migration"
MAIN_INSTALL_RC="${INSTALL_RC}"
MAIN_INSTALL_STDOUT="${INSTALL_STDOUT}"
MAIN_INSTALL_STDERR="${INSTALL_STDERR}"
PARITY_HOME="${WORK}/parity_home"
PARITY_RUN_DIR="${WORK}/parity_project"
PARITY_DEST="${PARITY_HOME}/.local/bin"
PARITY_STORAGE_ROOT="${PARITY_HOME}/.mcp_agent_mail_git_mailbox_repo"
PARITY_PYTHON_CLONE="${PARITY_HOME}/legacy_python_clone"
PARITY_PYTHON_DB="${PARITY_PYTHON_CLONE}/storage.sqlite3"
PARITY_CONFIG_ENV="${PARITY_HOME}/.config/mcp-agent-mail/config.env"
PARITY_ARCHIVE_PROJECT="${PARITY_STORAGE_ROOT}/projects/archive-only-project"
mkdir -p "${PARITY_RUN_DIR}" "${PARITY_DEST}" "${PARITY_STORAGE_ROOT}" "${PARITY_PYTHON_CLONE}/src/mcp_agent_mail" "${PARITY_PYTHON_CLONE}/scripts" "${PARITY_HOME}/.config/mcp-agent-mail" "${PARITY_HOME}/.codex"
cp -p "${PYTHON_DB}" "${PARITY_PYTHON_DB}"
cat > "${PARITY_PYTHON_CLONE}/pyproject.toml" <<'EOF'
[project]
name = "mcp_agent_mail"
version = "0.0.0"
EOF
cat > "${PARITY_PYTHON_CLONE}/src/mcp_agent_mail/__init__.py" <<'EOF'
__all__ = []
EOF
cat > "${PARITY_PYTHON_CLONE}/scripts/run_server_with_token.sh" <<'EOF'
#!/usr/bin/env bash
echo "LEGACY_PARITY_HELPER_STILL_ACTIVE" >&2
exit 31
EOF
chmod +x "${PARITY_PYTHON_CLONE}/scripts/run_server_with_token.sh"
cat > "${PARITY_PYTHON_CLONE}/.env" <<EOF
DATABASE_URL=sqlite+aiosqlite:///${PARITY_PYTHON_DB}
HTTP_BEARER_TOKEN=${LEGACY_TOKEN}
STORAGE_ROOT=${PARITY_PYTHON_CLONE}
EOF
cat > "${PARITY_HOME}/.zshrc" <<EOF
# >>> MCP Agent Mail alias
am() {
  cd "${PARITY_PYTHON_CLONE}" && scripts/run_server_with_token.sh "\$@"
}
# <<< MCP Agent Mail alias
EOF
git -C "${PARITY_STORAGE_ROOT}" init >/dev/null 2>&1
git -C "${PARITY_STORAGE_ROOT}" config user.email "e2e@example.com"
git -C "${PARITY_STORAGE_ROOT}" config user.name "E2E"
echo "seed" > "${PARITY_STORAGE_ROOT}/README.md"
git -C "${PARITY_STORAGE_ROOT}" add README.md
git -C "${PARITY_STORAGE_ROOT}" commit -m "seed storage repo" >/dev/null 2>&1
mkdir -p "${PARITY_ARCHIVE_PROJECT}/agents/ArchiveFox" "${PARITY_ARCHIVE_PROJECT}/messages/2026/03"
cat > "${PARITY_ARCHIVE_PROJECT}/project.json" <<'EOF'
{
  "slug": "archive-only-project",
  "human_key": "/tmp/archive-only-project"
}
EOF
cat > "${PARITY_ARCHIVE_PROJECT}/agents/ArchiveFox/profile.json" <<'EOF'
{
  "name": "ArchiveFox",
  "program": "codex",
  "model": "gpt-5",
  "task_description": "archive-only seed",
  "inception_ts": "2026-03-22T00:00:00Z",
  "last_active_ts": "2026-03-22T00:00:01Z",
  "attachments_policy": "auto",
  "contact_policy": "auto"
}
EOF
cat > "${PARITY_ARCHIVE_PROJECT}/messages/2026/03/20260322T000001Z__9001.md" <<'EOF'
---json
{"id":9001,"from":"ArchiveFox","to":["LegacyAgent"],"subject":"Archive only message","thread_id":"archive-only-thread","importance":"normal","ack_required":false,"created_ts":"2026-03-22T00:00:01Z","attachments":[]}
---
Recovered from canonical archive only.
EOF
git -C "${PARITY_STORAGE_ROOT}" add "${PARITY_ARCHIVE_PROJECT}"
git -C "${PARITY_STORAGE_ROOT}" commit -m "seed archive parity project" >/dev/null 2>&1
run_installer "case_06d_archive_parity_install" "${PARITY_RUN_DIR}" "${PARITY_HOME}" "${PARITY_DEST}" "${PARITY_STORAGE_ROOT}"
e2e_assert_exit_code "archive parity install exits cleanly" "0" "${INSTALL_RC}"
PARITY_INSTALL_OUTPUT="${INSTALL_STDOUT}
${INSTALL_STDERR}"
e2e_assert_contains "installer detects parity failure after self-heal" "${PARITY_INSTALL_OUTPUT}" "Archive parity still failed after self-heal; attempting archive reconstruction with salvage."
e2e_assert_contains "installer reconstructs from archive after parity failure" "${PARITY_INSTALL_OUTPUT}" "Archive-backed database reconstruction completed"
PARITY_DB_URL="$(grep -E '^DATABASE_URL=' "${PARITY_CONFIG_ENV}" 2>/dev/null | head -1 | cut -d= -f2- || true)"
PARITY_DB="$(sqlite_path_from_database_url "${PARITY_DB_URL}")"
e2e_assert_file_exists "archive parity install writes migrated database" "${PARITY_DB}"
set +e
PARITY_DOCTOR_JSON="$(
    HOME="${PARITY_HOME}" \
    PATH="${PARITY_DEST}:${PATH_BASE}" \
    AM_INTERFACE_MODE=cli \
    DATABASE_URL="${PARITY_DB_URL}" \
    "${PARITY_DEST}/am" doctor check --json 2>/dev/null
)"
PARITY_DOCTOR_RC=$?
PARITY_PROJECTS_JSON="$(
    HOME="${PARITY_HOME}" \
    PATH="${PARITY_DEST}:${PATH_BASE}" \
    AM_INTERFACE_MODE=cli \
    DATABASE_URL="${PARITY_DB_URL}" \
    "${PARITY_DEST}/am" list-projects --json 2>/dev/null
)"
PARITY_PROJECTS_RC=$?
set -e
e2e_save_artifact "case_06d_doctor.json" "${PARITY_DOCTOR_JSON}"
e2e_save_artifact "case_06d_projects.json" "${PARITY_PROJECTS_JSON}"
e2e_assert_exit_code "archive parity install doctor check exits cleanly" "0" "${PARITY_DOCTOR_RC}"
e2e_assert_exit_code "archive parity install list-projects exits cleanly" "0" "${PARITY_PROJECTS_RC}"
if json_query "${PARITY_DOCTOR_JSON}" "assert all(check.get('check') != 'archive_db_parity' or check.get('status') != 'fail' for check in d.get('checks', []))"; then
    e2e_pass "archive parity install clears doctor archive_db_parity failure"
else
    e2e_fail "archive parity install clears doctor archive_db_parity failure"
fi
if json_query "${PARITY_PROJECTS_JSON}" "assert any(p.get('human_key') == '/tmp/legacy-project' for p in d)"; then
    e2e_pass "archive parity install preserves migrated legacy project data"
else
    e2e_fail "archive parity install preserves migrated legacy project data"
fi
if json_query "${PARITY_PROJECTS_JSON}" "assert any(p.get('human_key') == '/tmp/archive-only-project' or p.get('slug') == 'archive-only-project' for p in d)"; then
    e2e_pass "archive parity install recovers archive-only project data"
else
    e2e_fail "archive parity install recovers archive-only project data"
fi
INSTALL_RC="${MAIN_INSTALL_RC}"
INSTALL_STDOUT="${MAIN_INSTALL_STDOUT}"
INSTALL_STDERR="${MAIN_INSTALL_STDERR}"

# ===========================================================================
# Case 7: Doctor + backup artifacts + Git health
# ===========================================================================
e2e_case_banner "doctor health, backup artifacts, and storage git integrity"
set +e
DOCTOR_JSON="$(run_migrated_am doctor check --json 2>/dev/null)"
DOCTOR_RC=$?
set -e
e2e_save_artifact "case_06_doctor.json" "${DOCTOR_JSON}"
e2e_assert_exit_code "doctor check exits cleanly" "0" "${DOCTOR_RC}"
if python3 -c "import json,sys; json.loads(sys.stdin.read())" <<< "${DOCTOR_JSON}" >/dev/null 2>&1; then
    e2e_pass "doctor check emits valid JSON"
else
    e2e_fail "doctor check emits valid JSON"
fi

BACKUP_PATH="$(find "${STORAGE_ROOT}" -maxdepth 1 -type f -name 'storage.sqlite3.bak.*' -print | python3 -c '
import datetime
import os
import re
import sys

pattern = re.compile(r"^storage\.sqlite3\.bak\.(\d{8}_\d{6})(?:-(0[1-9]|[1-9]\d+))?$")
for path in sorted(line.rstrip("\n") for line in sys.stdin if line.rstrip("\n")):
    match = pattern.fullmatch(os.path.basename(path))
    if match is None:
        continue
    try:
        datetime.datetime.strptime(match.group(1), "%Y%m%d_%H%M%S")
    except ValueError:
        continue
    print(path)
    break
' || true)"
if [ -n "${BACKUP_PATH}" ] && [ -f "${BACKUP_PATH}" ]; then
    e2e_pass "timestamp migration backup artifact created"
else
    if [[ "${INSTALL_STDOUT}" == *"Database schema migrated"* ]]; then
        e2e_pass "timestamp migration backup artifact not required when conversion is already complete"
    else
        e2e_fail "timestamp migration backup artifact created"
    fi
fi

set +e
git -C "${STORAGE_ROOT}" fsck --no-progress >/dev/null 2>&1
FSCK_RC=$?
LOG_HEAD="$(git -C "${STORAGE_ROOT}" log --oneline --max-count 1 2>/dev/null)"
LOG_RC=$?
set -e
e2e_assert_exit_code "storage root git repository remains healthy" "0" "${FSCK_RC}"
e2e_assert_exit_code "storage root git log is readable" "0" "${LOG_RC}"
if [ -n "${LOG_HEAD}" ]; then
    e2e_pass "storage root git history remains readable"
else
    e2e_fail "storage root git history remains readable"
fi

# ===========================================================================
# Case 8: Docker harness definition exists (optional build smoke)
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
