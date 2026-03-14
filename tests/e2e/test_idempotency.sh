#!/usr/bin/env bash
# test_idempotency.sh - E2E installer idempotency coverage (br-28mgh.8.7)
#
# Runs install.sh in a controlled offline environment using local release artifacts.
# Validates same-version idempotency and upgrade-path safety.

set -euo pipefail

E2E_SUITE="idempotency"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Installer Idempotency E2E Suite (br-28mgh.8.7)"

WORK="$(e2e_mktemp "e2e_idempotency")"
INSTALL_SH="${SCRIPT_DIR}/../../install.sh"
RUN_DIR="${WORK}/project"
TEST_HOME="${WORK}/home"
DEST="${TEST_HOME}/.local/bin"
STORAGE_ROOT="${TEST_HOME}/storage_root"
MCP_CONFIG="${RUN_DIR}/codex.mcp.json"
PATH_BASE="/usr/bin:/bin"
TEST_SHELL="$(command -v zsh 2>/dev/null || command -v bash 2>/dev/null || echo /bin/sh)"

mkdir -p "${RUN_DIR}" "${DEST}" "${TEST_HOME}/.config/fish"

sha256_file() {
    local file="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    else
        shasum -a 256 "$file" | awk '{print $1}'
    fi
}

count_literal_in_file() {
    local file="$1"
    local literal="$2"
    grep -F -c "$literal" "$file" 2>/dev/null || true
}

build_mock_release_artifact() {
    local version="$1"
    local artifact_path="$2"
    local stage="${WORK}/artifact_${version}"

    rm -rf "$stage"
    mkdir -p "$stage"

    cat > "${stage}/am" <<EOF
#!/usr/bin/env bash
set -euo pipefail
VERSION="${version}"
cmd="\${1:-}"
case "\$cmd" in
  --version|-V|version)
    echo "am \${VERSION}"
    ;;
  --help|-h|help)
    cat <<'EOH'
Usage: am [COMMAND]

Commands:
  serve-http
  doctor
  list-projects
  migrate
EOH
    ;;
  doctor)
    echo "all green"
    ;;
  list-projects)
    echo "[]"
    ;;
  migrate)
    echo "migrate:\${VERSION}" >> "\${HOME}/.mock_am_migrate_calls.log"
    echo "migration-ok"
    ;;
  *)
    echo "am \${VERSION}"
    ;;
esac
EOF
    chmod +x "${stage}/am"

    cat > "${stage}/mcp-agent-mail" <<EOF
#!/usr/bin/env bash
set -euo pipefail
VERSION="${version}"
cmd="\${1:-}"
case "\$cmd" in
  --version|-V|version)
    echo "mcp-agent-mail \${VERSION}"
    ;;
  --help|-h|help)
    cat <<'EOH'
Usage: mcp-agent-mail [COMMAND]

Commands:
  serve
  config
EOH
    ;;
  *)
    echo "mcp-agent-mail \${VERSION}"
    ;;
esac
EOF
    chmod +x "${stage}/mcp-agent-mail"

    tar -cJf "$artifact_path" -C "$stage" am mcp-agent-mail
}

run_installer() {
    local case_name="$1"
    local version="$2"
    local artifact_path="$3"

    local stdout_file="${WORK}/${case_name}_stdout.txt"
    local stderr_file="${WORK}/${case_name}_stderr.txt"

    set +e
    (
        cd "$RUN_DIR"
        HOME="$TEST_HOME" \
        SHELL="$TEST_SHELL" \
        STORAGE_ROOT="$STORAGE_ROOT" \
        PATH="$PATH_BASE" \
        bash "$INSTALL_SH" \
            --version "v${version}" \
            --artifact-url "file://${artifact_path}" \
            --dest "$DEST" \
            --offline \
            --no-verify \
            --no-gum
    ) >"$stdout_file" 2>"$stderr_file"
    LAST_INSTALL_RC=$?
    set -e

    LAST_INSTALL_STDOUT="$(cat "$stdout_file" 2>/dev/null || true)"
    LAST_INSTALL_STDERR="$(cat "$stderr_file" 2>/dev/null || true)"
    e2e_save_artifact "${case_name}_stdout.txt" "$LAST_INSTALL_STDOUT"
    e2e_save_artifact "${case_name}_stderr.txt" "$LAST_INSTALL_STDERR"
}

# ---------------------------------------------------------------------------
# Fixture setup: baseline shell/config state + storage git repo
# ---------------------------------------------------------------------------
cat > "${TEST_HOME}/.zshrc" <<'EOF'
# Disabled by mcp-agent-mail Rust installer: alias am='python -m mcp_agent_mail'
EOF
cat > "${TEST_HOME}/.bashrc" <<'EOF'
# shell baseline
EOF

mkdir -p "${STORAGE_ROOT}"
git -C "${STORAGE_ROOT}" init >/dev/null 2>&1
git -C "${STORAGE_ROOT}" config user.email "e2e@example.com"
git -C "${STORAGE_ROOT}" config user.name "E2E"
echo "seed" > "${STORAGE_ROOT}/README.md"
git -C "${STORAGE_ROOT}" add README.md
git -C "${STORAGE_ROOT}" commit -m "seed storage repo" >/dev/null 2>&1
echo "stable-db-seed" > "${STORAGE_ROOT}/storage.sqlite3"

RUST_ENV="${TEST_HOME}/.config/mcp-agent-mail/config.env"
mkdir -p "$(dirname "$RUST_ENV")"
cat > "$RUST_ENV" <<'EOF'
HTTP_BEARER_TOKEN=test-token-123
STORAGE_ROOT=/tmp/placeholder
EOF

cat > "${MCP_CONFIG}" <<'EOF'
{
  "mcpServers": {
    "mcp-agent-mail": {
      "command": "python",
      "args": ["-m", "mcp_agent_mail"],
      "env": {
        "HTTP_BEARER_TOKEN": "test-token-123",
        "STORAGE_ROOT": "/tmp/python_storage"
      }
    },
    "other-tool": {
      "command": "node",
      "args": ["server.js"]
    }
  }
}
EOF

MCP_CONFIG_SHA_BEFORE="$(sha256_file "$MCP_CONFIG")"

ARTIFACT_V010="${WORK}/mcp-agent-mail-v0.1.0.tar.xz"
ARTIFACT_V011="${WORK}/mcp-agent-mail-v0.1.1.tar.xz"
build_mock_release_artifact "0.1.0" "$ARTIFACT_V010"
build_mock_release_artifact "0.1.1" "$ARTIFACT_V011"

# ===========================================================================
# Case 1: First install v0.1.0
# ===========================================================================
e2e_case_banner "First install establishes idempotency baseline"

run_installer "case_01_first_install" "0.1.0" "$ARTIFACT_V010"
e2e_assert_exit_code "first install exits 0" "0" "$LAST_INSTALL_RC"

VERSION_FIRST="$("$DEST/am" --version)"
e2e_assert_contains "installed am version is 0.1.0" "$VERSION_FIRST" "0.1.0"

PATH_LINE="export PATH=\"${DEST}:\$PATH\""
ZSH_PATH_COUNT_FIRST="$(count_literal_in_file "${TEST_HOME}/.zshrc" "$PATH_LINE")"
BASH_PATH_COUNT_FIRST="$(count_literal_in_file "${TEST_HOME}/.bashrc" "$PATH_LINE")"
e2e_assert_eq "zsh PATH line added once on first install" "1" "$ZSH_PATH_COUNT_FIRST"
e2e_assert_eq "bash PATH line added once on first install" "1" "$BASH_PATH_COUNT_FIRST"

ALIAS_DISABLE_COUNT_FIRST="$(count_literal_in_file "${TEST_HOME}/.zshrc" "Disabled by mcp-agent-mail Rust installer")"
e2e_assert_eq "baseline disabled alias marker remains single after first install" "1" "$ALIAS_DISABLE_COUNT_FIRST"

e2e_assert_file_exists "rust env exists" "$RUST_ENV"
TOKEN_FIRST="$(grep -E '^HTTP_BEARER_TOKEN=' "$RUST_ENV" | head -1 | cut -d= -f2-)"
e2e_assert_eq "bearer token baseline preserved" "test-token-123" "$TOKEN_FIRST"

DB_PATH="${STORAGE_ROOT}/storage.sqlite3"
e2e_assert_file_exists "storage DB exists" "$DB_PATH"

AM_SHA_FIRST="$(sha256_file "$DEST/am")"
DB_SHA_FIRST="$(sha256_file "$DB_PATH")"
ZSH_SHA_FIRST="$(sha256_file "${TEST_HOME}/.zshrc")"
BASH_SHA_FIRST="$(sha256_file "${TEST_HOME}/.bashrc")"
RUST_ENV_SHA_FIRST="$(sha256_file "$RUST_ENV")"
MCP_CONFIG_SHA_FIRST="$(sha256_file "$MCP_CONFIG")"

# ===========================================================================
# Case 2: Second install same version (idempotent)
# ===========================================================================
e2e_case_banner "Second install same version is idempotent"

run_installer "case_02_second_install_same_version" "0.1.0" "$ARTIFACT_V010"
e2e_assert_exit_code "second same-version install exits 0" "0" "$LAST_INSTALL_RC"
e2e_assert_contains "second install reports already installed" "$LAST_INSTALL_STDOUT" "already installed"

VERSION_SECOND="$("$DEST/am" --version)"
e2e_assert_contains "version unchanged after second install" "$VERSION_SECOND" "0.1.0"

AM_SHA_SECOND="$(sha256_file "$DEST/am")"
DB_SHA_SECOND="$(sha256_file "$DB_PATH")"
ZSH_SHA_SECOND="$(sha256_file "${TEST_HOME}/.zshrc")"
BASH_SHA_SECOND="$(sha256_file "${TEST_HOME}/.bashrc")"
RUST_ENV_SHA_SECOND="$(sha256_file "$RUST_ENV")"
MCP_CONFIG_SHA_SECOND="$(sha256_file "$MCP_CONFIG")"

e2e_assert_eq "binary checksum unchanged on same-version reinstall" "$AM_SHA_FIRST" "$AM_SHA_SECOND"
e2e_assert_eq "db checksum unchanged on same-version reinstall" "$DB_SHA_FIRST" "$DB_SHA_SECOND"
e2e_assert_eq "zshrc checksum unchanged on second install" "$ZSH_SHA_FIRST" "$ZSH_SHA_SECOND"
e2e_assert_eq "bashrc checksum unchanged on second install" "$BASH_SHA_FIRST" "$BASH_SHA_SECOND"
e2e_assert_eq "migrated env checksum unchanged on second install" "$RUST_ENV_SHA_FIRST" "$RUST_ENV_SHA_SECOND"
e2e_assert_eq "mcp config checksum unchanged on second install" "$MCP_CONFIG_SHA_FIRST" "$MCP_CONFIG_SHA_SECOND"

ZSH_PATH_COUNT_SECOND="$(count_literal_in_file "${TEST_HOME}/.zshrc" "$PATH_LINE")"
BASH_PATH_COUNT_SECOND="$(count_literal_in_file "${TEST_HOME}/.bashrc" "$PATH_LINE")"
ALIAS_DISABLE_COUNT_SECOND="$(count_literal_in_file "${TEST_HOME}/.zshrc" "Disabled by mcp-agent-mail Rust installer")"
e2e_assert_eq "zsh PATH line still single after second install" "1" "$ZSH_PATH_COUNT_SECOND"
e2e_assert_eq "bash PATH line still single after second install" "1" "$BASH_PATH_COUNT_SECOND"
e2e_assert_eq "alias displacement marker not duplicated" "1" "$ALIAS_DISABLE_COUNT_SECOND"

TOKEN_SECOND="$(grep -E '^HTTP_BEARER_TOKEN=' "$RUST_ENV" | head -1 | cut -d= -f2-)"
e2e_assert_eq "bearer token unchanged after second install" "$TOKEN_FIRST" "$TOKEN_SECOND"

set +e
DOCTOR_SECOND="$("$DEST/am" doctor 2>&1)"
DOCTOR_RC_SECOND=$?
set -e
e2e_assert_exit_code "doctor after second install exits 0" "0" "$DOCTOR_RC_SECOND"
e2e_assert_contains "doctor after second install all green" "$DOCTOR_SECOND" "all green"

set +e
git -C "${STORAGE_ROOT}" fsck --no-progress >/dev/null 2>&1
GIT_FSCK_RC_SECOND=$?
set -e
e2e_assert_exit_code "storage root git repo integrity preserved (second install)" "0" "$GIT_FSCK_RC_SECOND"

# ===========================================================================
# Case 3: Same-version reinstall repairs active Python alias shadow
# ===========================================================================
e2e_case_banner "Same-version reinstall repairs active Python am shadow"

printf "\nalias am='python -m mcp_agent_mail'\n" >> "${TEST_HOME}/.zshrc"
printf "\nalias am='python -m mcp_agent_mail'\n" >> "${TEST_HOME}/.bashrc"

ZSH_ALIAS_DISABLE_COUNT_BEFORE_REPAIR="$(count_literal_in_file "${TEST_HOME}/.zshrc" "Disabled by mcp-agent-mail Rust installer: alias am='python -m mcp_agent_mail'")"
BASH_ALIAS_DISABLE_COUNT_BEFORE_REPAIR="$(count_literal_in_file "${TEST_HOME}/.bashrc" "Disabled by mcp-agent-mail Rust installer: alias am='python -m mcp_agent_mail'")"

run_installer "case_03_same_version_repairs_python_shadow" "0.1.0" "$ARTIFACT_V010"
e2e_assert_exit_code "shadow-repair reinstall exits 0" "0" "$LAST_INSTALL_RC"
e2e_assert_not_contains "shadow-repair reinstall does not short-circuit as healthy" "$LAST_INSTALL_STDOUT" "ok mcp-agent-mail v0.1.0 is already installed"
e2e_assert_contains "shadow-repair reinstall explains repair requirement" "$LAST_INSTALL_STDOUT" "still needs repair"
e2e_assert_contains "shadow-repair reinstall continues into remediation" "$LAST_INSTALL_STDOUT" "Continuing with reinstall/remediation instead of exiting early."
e2e_assert_contains "shadow-repair reinstall disables python alias" "$LAST_INSTALL_STDOUT" "Python alias disabled in"
e2e_assert_contains "shadow-repair reinstall prints current-shell cleanup hint" "$LAST_INSTALL_STDOUT" "unalias am 2>/dev/null || true"

if grep -Eq '^[[:space:]]*(alias am=|alias am |function am[[:space:](]|am[[:space:]]*\(\))' "${TEST_HOME}/.zshrc"; then
    e2e_fail "shadow-repair reinstall removed active python alias from .zshrc"
else
    e2e_pass "shadow-repair reinstall removed active python alias from .zshrc"
fi

if grep -Eq '^[[:space:]]*(alias am=|alias am |function am[[:space:](]|am[[:space:]]*\(\))' "${TEST_HOME}/.bashrc"; then
    e2e_fail "shadow-repair reinstall removed active python alias from .bashrc"
else
    e2e_pass "shadow-repair reinstall removed active python alias from .bashrc"
fi

ZSH_ALIAS_DISABLE_COUNT_REPAIR="$(count_literal_in_file "${TEST_HOME}/.zshrc" "Disabled by mcp-agent-mail Rust installer: alias am='python -m mcp_agent_mail'")"
BASH_ALIAS_DISABLE_COUNT_REPAIR="$(count_literal_in_file "${TEST_HOME}/.bashrc" "Disabled by mcp-agent-mail Rust installer: alias am='python -m mcp_agent_mail'")"
e2e_assert_eq "shadow-repair reinstall adds one disabled zsh alias marker" "$((ZSH_ALIAS_DISABLE_COUNT_BEFORE_REPAIR + 1))" "$ZSH_ALIAS_DISABLE_COUNT_REPAIR"
e2e_assert_eq "shadow-repair reinstall adds one disabled bash alias marker" "$((BASH_ALIAS_DISABLE_COUNT_BEFORE_REPAIR + 1))" "$BASH_ALIAS_DISABLE_COUNT_REPAIR"

set +e
INTERACTIVE_RESOLUTION="$(
    HOME="$TEST_HOME" \
    SHELL="$TEST_SHELL" \
    PATH="$PATH_BASE" \
    "$TEST_SHELL" -i -c 'command -V am 2>/dev/null || echo NOT_FOUND' 2>/dev/null
)"
INTERACTIVE_RESOLUTION_RC=$?
set -e
e2e_assert_exit_code "interactive shell resolution probe exits 0 after repair" "0" "$INTERACTIVE_RESOLUTION_RC"
e2e_assert_contains "interactive shell resolves am to installed Rust binary after repair" "$INTERACTIVE_RESOLUTION" "$DEST/am"
e2e_assert_not_contains "interactive shell no longer resolves am via alias after repair" "$INTERACTIVE_RESOLUTION" "alias"

# ===========================================================================
# Case 4: Upgrade path v0.1.0 -> v0.1.1
# ===========================================================================
e2e_case_banner "Upgrade path installs new version safely"

run_installer "case_03_upgrade_install" "0.1.1" "$ARTIFACT_V011"
e2e_assert_exit_code "upgrade install exits 0" "0" "$LAST_INSTALL_RC"

VERSION_UPGRADE="$("$DEST/am" --version)"
e2e_assert_contains "version upgraded to 0.1.1" "$VERSION_UPGRADE" "0.1.1"

AM_SHA_UPGRADE="$(sha256_file "$DEST/am")"
e2e_assert_eq "binary checksum changes on upgrade" "different" "$([ "$AM_SHA_UPGRADE" = "$AM_SHA_SECOND" ] && echo same || echo different)"

ZSH_PATH_COUNT_UPGRADE="$(count_literal_in_file "${TEST_HOME}/.zshrc" "$PATH_LINE")"
BASH_PATH_COUNT_UPGRADE="$(count_literal_in_file "${TEST_HOME}/.bashrc" "$PATH_LINE")"
e2e_assert_eq "zsh PATH line still single after upgrade" "1" "$ZSH_PATH_COUNT_UPGRADE"
e2e_assert_eq "bash PATH line still single after upgrade" "1" "$BASH_PATH_COUNT_UPGRADE"

TOKEN_UPGRADE="$(grep -E '^HTTP_BEARER_TOKEN=' "$RUST_ENV" | head -1 | cut -d= -f2-)"
e2e_assert_eq "bearer token preserved through upgrade" "$TOKEN_FIRST" "$TOKEN_UPGRADE"

MCP_CONFIG_SHA_UPGRADE="$(sha256_file "$MCP_CONFIG")"
e2e_assert_eq "mcp config remains uncorrupted through upgrade" "$MCP_CONFIG_SHA_BEFORE" "$MCP_CONFIG_SHA_UPGRADE"

set +e
DOCTOR_UPGRADE="$("$DEST/am" doctor 2>&1)"
DOCTOR_RC_UPGRADE=$?
set -e
e2e_assert_exit_code "doctor after upgrade exits 0" "0" "$DOCTOR_RC_UPGRADE"
e2e_assert_contains "doctor after upgrade all green" "$DOCTOR_UPGRADE" "all green"

set +e
git -C "${STORAGE_ROOT}" fsck --no-progress >/dev/null 2>&1
GIT_FSCK_RC_UPGRADE=$?
set -e
e2e_assert_exit_code "storage root git repo integrity preserved (upgrade)" "0" "$GIT_FSCK_RC_UPGRADE"

e2e_summary
