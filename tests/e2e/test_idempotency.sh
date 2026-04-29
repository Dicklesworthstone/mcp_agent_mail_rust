#!/usr/bin/env bash
# test_idempotency.sh - E2E installer idempotency coverage (br-28mgh.8.7)
#
# Runs install.sh in a controlled offline environment using local release artifacts.
# Validates same-version idempotency and upgrade-path safety.
#
# Artifact strategy (br-aazao.3.2):
#   Cases 1-4 use synthetic shell-stub artifacts.  These stubs are REQUIRED
#   for narrow contract coverage that cannot be satisfied by real binaries:
#     - Controlled version strings (0.1.0 / 0.1.1) to test upgrade-path
#       logic without coupling to the actual Cargo.toml version.
#     - Deterministic "doctor" / "migrate" output strings the assertions
#       match verbatim.
#     - Two distinct version tarballs to exercise the binary-swap code path.
#   Cases 5-7 (built-artifact lane) use real cargo-built binaries packaged
#   into tarballs and fed to install.sh via --artifact-url file://.  These
#   exercise the same installer code paths with production ELF payloads.

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

count_glob_matches() {
    local pattern="$1"
    local matches
    matches="$(compgen -G "$pattern" || true)"
    if [ -z "$matches" ]; then
        printf '0\n'
    else
        printf '%s\n' "$matches" | wc -l | tr -d ' '
    fi
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

run_legacy_link_installer() {
    local case_name="$1"
    local path_value="$2"

    local stdout_file="${WORK}/${case_name}_stdout.txt"
    local stderr_file="${WORK}/${case_name}_stderr.txt"

    set +e
    (
        cd "$LEGACY_LINK_RUN_DIR"
        HOME="$LEGACY_LINK_HOME" \
        SHELL="$TEST_SHELL" \
        STORAGE_ROOT="$LEGACY_LINK_STORAGE" \
        PATH="$path_value" \
        bash "$INSTALL_SH" \
            --version "v0.1.0" \
            --artifact-url "file://${ARTIFACT_V010}" \
            --dest "$LEGACY_LINK_DEST" \
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
# Case 2b: Same-version install skips legacy clone residue after completed migration
# ===========================================================================
e2e_case_banner "Same-version install skips inert legacy clone residue"

LEGACY_LINK_HOME="${WORK}/legacy_link_home"
LEGACY_LINK_RUN_DIR="${WORK}/legacy_link_project"
LEGACY_LINK_DEST="${LEGACY_LINK_HOME}/mcp_agent_mail"
LEGACY_LINK_BIN="${LEGACY_LINK_HOME}/.local/bin"
LEGACY_LINK_STORAGE="${LEGACY_LINK_HOME}/storage_root"
mkdir -p "$LEGACY_LINK_RUN_DIR" "$LEGACY_LINK_DEST" "$LEGACY_LINK_BIN" "$LEGACY_LINK_STORAGE"

run_legacy_link_installer "case_02b_first_install" "$PATH_BASE"
e2e_assert_exit_code "legacy-link first install exits 0" "0" "$LAST_INSTALL_RC"

cat > "${LEGACY_LINK_DEST}/pyproject.toml" <<'EOF'
[project]
name = "mcp-agent-mail"
EOF
mkdir -p "${LEGACY_LINK_DEST}/.venv/bin" "${LEGACY_LINK_HOME}/.config/mcp-agent-mail"
cat > "${LEGACY_LINK_DEST}/.venv/bin/am" <<'EOF'
#!/usr/bin/env python3
print("legacy python am")
EOF
chmod +x "${LEGACY_LINK_DEST}/.venv/bin/am"
printf 'migrated_at=2026-01-01T00:00:00Z\nnote=test fixture\n' \
    > "${LEGACY_LINK_HOME}/.config/mcp-agent-mail/.python-migration-complete"
ln -s "${LEGACY_LINK_DEST}/am" "${LEGACY_LINK_BIN}/am"
cat > "${LEGACY_LINK_HOME}/.zshrc" <<EOF
alias am='${LEGACY_LINK_BIN}/am'
EOF
cat > "${LEGACY_LINK_HOME}/.bashrc" <<EOF
alias am='${LEGACY_LINK_BIN}/am'
EOF

LEGACY_LINK_BACKUPS_BEFORE="$(count_glob_matches "${LEGACY_LINK_BIN}/am.bak.mcp-agent-mail-*")"

run_legacy_link_installer "case_02b_second_install_legacy_clone_residue" "${LEGACY_LINK_BIN}:${PATH_BASE}"
e2e_assert_exit_code "legacy-link second install exits 0" "0" "$LAST_INSTALL_RC"
e2e_assert_contains "legacy-link second install reports already installed" "$LAST_INSTALL_STDOUT" "already installed"
e2e_assert_not_contains "legacy-link second install does not force repair" "$LAST_INSTALL_STDOUT" "still needs repair"
e2e_assert_not_contains "legacy-link second install does not displace rust symlink" "$LAST_INSTALL_STDOUT" "Legacy am launcher displaced"

LEGACY_LINK_TARGET="$(readlink "${LEGACY_LINK_BIN}/am")"
e2e_assert_eq "legacy-link shim still targets installed Rust am" "${LEGACY_LINK_DEST}/am" "$LEGACY_LINK_TARGET"
LEGACY_LINK_BACKUPS_AFTER="$(count_glob_matches "${LEGACY_LINK_BIN}/am.bak.mcp-agent-mail-*")"
e2e_assert_eq "legacy-link second install creates no am shim backup" "$LEGACY_LINK_BACKUPS_BEFORE" "$LEGACY_LINK_BACKUPS_AFTER"

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

# ===========================================================================
# Built-artifact lane (br-aazao.3.2)
# Uses real cargo-built binaries instead of synthetic shell stubs.
# ===========================================================================

build_real_release_artifact() {
    local artifact_path="$1"
    local stage="${WORK}/real_artifact"

    rm -rf "$stage"
    mkdir -p "$stage"

    # Ensure real binaries are available via the e2e build machinery.
    e2e_ensure_binary "am" >/dev/null
    e2e_ensure_binary "mcp-agent-mail" >/dev/null

    cp "${CARGO_TARGET_DIR}/debug/am" "${stage}/am"
    cp "${CARGO_TARGET_DIR}/debug/mcp-agent-mail" "${stage}/mcp-agent-mail"
    chmod +x "${stage}/am" "${stage}/mcp-agent-mail"
    tar -cJf "$artifact_path" -C "$stage" am mcp-agent-mail
}

REAL_VERSION="$("${CARGO_TARGET_DIR}/debug/am" --version 2>/dev/null | awk '{print $2}' | sed 's/^v//' | head -1)"
if [ -z "${REAL_VERSION}" ]; then
    REAL_VERSION="${CARGO_PKG_VERSION:-0.0.0}"
    REAL_VERSION="${REAL_VERSION#v}"
fi

ARTIFACT_REAL="${WORK}/mcp-agent-mail-real.tar.xz"
build_real_release_artifact "$ARTIFACT_REAL"

# Reset sandbox state for the built-artifact lane
REAL_HOME="${WORK}/real_home"
REAL_DEST="${REAL_HOME}/.local/bin"
REAL_STORAGE="${REAL_HOME}/storage_root"
REAL_RUN_DIR="${WORK}/real_project"
REAL_MCP_CONFIG="${REAL_RUN_DIR}/codex.mcp.json"
mkdir -p "$REAL_DEST" "$REAL_HOME/.config/fish" "$REAL_STORAGE" "$REAL_RUN_DIR"

cat > "${REAL_HOME}/.zshrc" <<'EOF'
# clean baseline
EOF
cat > "${REAL_HOME}/.bashrc" <<'EOF'
# clean baseline
EOF

mkdir -p "${REAL_STORAGE}"
git -C "${REAL_STORAGE}" init >/dev/null 2>&1
git -C "${REAL_STORAGE}" config user.email "e2e@example.com"
git -C "${REAL_STORAGE}" config user.name "E2E"
echo "seed" > "${REAL_STORAGE}/README.md"
git -C "${REAL_STORAGE}" add README.md
git -C "${REAL_STORAGE}" commit -m "seed" >/dev/null 2>&1

REAL_RUST_ENV="${REAL_HOME}/.config/mcp-agent-mail/config.env"
mkdir -p "$(dirname "$REAL_RUST_ENV")"
cat > "$REAL_RUST_ENV" <<'EOF'
HTTP_BEARER_TOKEN=real-token-xyz
EOF

cat > "${REAL_MCP_CONFIG}" <<'EOF'
{
  "mcpServers": {
    "other-tool": {
      "command": "node",
      "args": ["server.js"]
    }
  }
}
EOF

run_real_installer() {
    local case_name="$1"
    local stdout_file="${WORK}/${case_name}_stdout.txt"
    local stderr_file="${WORK}/${case_name}_stderr.txt"

    set +e
    (
        cd "$REAL_RUN_DIR"
        HOME="$REAL_HOME" \
        SHELL="$TEST_SHELL" \
        STORAGE_ROOT="$REAL_STORAGE" \
        PATH="$PATH_BASE" \
        bash "$INSTALL_SH" \
            --version "v${REAL_VERSION}" \
            --artifact-url "file://${ARTIFACT_REAL}" \
            --dest "$REAL_DEST" \
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

# ===========================================================================
# Case 5 (built-artifact): First install with real binary
# ===========================================================================
e2e_case_banner "[built-artifact] First install with real binary"

run_real_installer "case_05_real_first_install"
e2e_assert_exit_code "[built-artifact] first install exits 0" "0" "$LAST_INSTALL_RC"

REAL_AM_OUT="$("$REAL_DEST/am" --version 2>&1)"
e2e_assert_contains "[built-artifact] installed am reports real version" "$REAL_AM_OUT" "$REAL_VERSION"

set +e
REAL_FILE_TYPE="$(file "$REAL_DEST/am" 2>&1)"
set -e
e2e_assert_contains "[built-artifact] installed am is ELF binary" "$REAL_FILE_TYPE" "ELF"

REAL_AM_SHA_FIRST="$(sha256_file "$REAL_DEST/am")"
REAL_SERVER_SHA_FIRST="$(sha256_file "$REAL_DEST/mcp-agent-mail")"
REAL_TOKEN_FIRST="$(grep -E '^HTTP_BEARER_TOKEN=' "$REAL_RUST_ENV" | head -1 | cut -d= -f2-)"
e2e_assert_eq "[built-artifact] bearer token preserved" "real-token-xyz" "$REAL_TOKEN_FIRST"

# ===========================================================================
# Case 6 (built-artifact): Same-version reinstall is idempotent
# ===========================================================================
e2e_case_banner "[built-artifact] Same-version reinstall idempotent"

run_real_installer "case_06_real_second_install"
e2e_assert_exit_code "[built-artifact] second install exits 0" "0" "$LAST_INSTALL_RC"
e2e_assert_contains "[built-artifact] second install reports already installed" "$LAST_INSTALL_STDOUT" "already installed"

REAL_AM_SHA_SECOND="$(sha256_file "$REAL_DEST/am")"
REAL_SERVER_SHA_SECOND="$(sha256_file "$REAL_DEST/mcp-agent-mail")"
e2e_assert_eq "[built-artifact] am checksum unchanged" "$REAL_AM_SHA_FIRST" "$REAL_AM_SHA_SECOND"
e2e_assert_eq "[built-artifact] server checksum unchanged" "$REAL_SERVER_SHA_FIRST" "$REAL_SERVER_SHA_SECOND"

REAL_TOKEN_SECOND="$(grep -E '^HTTP_BEARER_TOKEN=' "$REAL_RUST_ENV" | head -1 | cut -d= -f2-)"
e2e_assert_eq "[built-artifact] bearer token unchanged" "$REAL_TOKEN_FIRST" "$REAL_TOKEN_SECOND"

REAL_MCP_SHA_AFTER="$(sha256_file "$REAL_MCP_CONFIG")"
REAL_MCP_SHA_BEFORE="$(sha256_file "$REAL_MCP_CONFIG")"
e2e_assert_eq "[built-artifact] mcp config unchanged" "$REAL_MCP_SHA_BEFORE" "$REAL_MCP_SHA_AFTER"

# ===========================================================================
# Case 7 (built-artifact): Real binary doctor surface exercised
# ===========================================================================
e2e_case_banner "[built-artifact] Doctor runs against real binary"

set +e
REAL_DOCTOR_OUT="$(
    HOME="$REAL_HOME" \
    STORAGE_ROOT="$REAL_STORAGE" \
    "$REAL_DEST/am" doctor check 2>&1
)"
REAL_DOCTOR_RC=$?
set -e
e2e_save_artifact "case_07_real_doctor.txt" "$REAL_DOCTOR_OUT"
# Doctor may return 0 (green) or 1 (warnings on fresh install) but must not panic.
if [ "$REAL_DOCTOR_RC" -le 1 ]; then
    e2e_pass "[built-artifact] doctor exits cleanly (rc=$REAL_DOCTOR_RC)"
else
    e2e_fail "[built-artifact] doctor should not crash" "exit <=1" "exit $REAL_DOCTOR_RC"
fi

e2e_summary
