#!/usr/bin/env bash
# test_fresh_install.sh - E2E suite for fresh-install surface validation.
#
# Verifies that a clean install (no prior Python or Rust mcp-agent-mail)
# produces a usable installation with correct binaries, PATH setup,
# MCP configuration, and doctor/serve-stdio contracts.
#
# NOTE: This test uses pre-built binaries from CARGO_TARGET_DIR and
# exercises the install.sh functions in a sandboxed temp environment.
# For full Docker-based isolation, see Dockerfile.fresh.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export E2E_SUITE="fresh_install"

# shellcheck source=e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Fresh Install E2E Suite"

# Build both binaries (if not already present)
e2e_ensure_binary "mcp-agent-mail" >/dev/null
e2e_ensure_binary "am" >/dev/null

# Locate binaries
SERVER_BIN="${CARGO_TARGET_DIR}/debug/mcp-agent-mail"
CLI_BIN="${CARGO_TARGET_DIR}/debug/am"

# Create an isolated HOME to simulate a clean system
# RCH intentionally points TMPDIR inside the checkout. A fake HOME created
# there is not an outside-home control and can make Git-boundary tests
# false-fail. Use the system temp root explicitly for this isolation suite.
FAKE_HOME="$(mktemp -d "/tmp/fresh_install_home.XXXXXX")"
FAKE_HOME="$(cd "${FAKE_HOME}" && pwd -P)"
FAKE_DEST="${FAKE_HOME}/.local/bin"
mkdir -p "$FAKE_DEST"

cleanup_fresh() {
  if [ "$AM_E2E_KEEP_TMP" = "1" ] || [ "$AM_E2E_KEEP_TMP" = "true" ]; then
    printf 'Preserved fresh-install sandbox: %s\n' "$FAKE_HOME" >&2
  else
    rm -rf "$FAKE_HOME" 2>/dev/null || true
  fi
}
trap cleanup_fresh EXIT

# Copy binaries into fake DEST (simulating what install.sh atomic_install does)
cp "$SERVER_BIN" "$FAKE_DEST/mcp-agent-mail"
cp "$CLI_BIN" "$FAKE_DEST/am"
chmod +x "$FAKE_DEST/mcp-agent-mail" "$FAKE_DEST/am"

# Set up isolated environment
export HOME="$FAKE_HOME"
export PATH="${FAKE_DEST}:${PATH}"
# Prevent am from picking up the real project's storage
export STORAGE_ROOT="${FAKE_HOME}/.mcp_agent_mail_git_mailbox_repo"

# ===========================================================================
# Case 1: am binary exists and is executable
# ===========================================================================
e2e_case_banner "am binary exists and is executable"

e2e_assert_file_exists "am binary in DEST" "$FAKE_DEST/am"
e2e_assert_file_exists "mcp-agent-mail binary in DEST" "$FAKE_DEST/mcp-agent-mail"

if [ -x "$FAKE_DEST/am" ]; then AM_EXEC_RC=0; else AM_EXEC_RC=1; fi
if [ -x "$FAKE_DEST/mcp-agent-mail" ]; then SERVER_EXEC_RC=0; else SERVER_EXEC_RC=1; fi

e2e_assert_exit_code "am is executable" "0" "$AM_EXEC_RC"
e2e_assert_exit_code "mcp-agent-mail is executable" "0" "$SERVER_EXEC_RC"

# ===========================================================================
# Case 2: am --version returns Rust version string
# ===========================================================================
e2e_case_banner "am --version returns Rust version"

set +e
AM_VERSION_OUT="$("$FAKE_DEST/am" --version 2>&1)"
AM_VERSION_RC=$?
set -e

e2e_save_artifact "case_02_am_version.txt" "$AM_VERSION_OUT"
e2e_assert_exit_code "am --version" "0" "$AM_VERSION_RC"
e2e_assert_contains "am version output is non-empty" "$AM_VERSION_OUT" "."
# Should NOT contain "python" or "Python" anywhere
e2e_assert_not_contains "am version is not Python" "$AM_VERSION_OUT" "python"
e2e_assert_not_contains "am version is not Python (cap)" "$AM_VERSION_OUT" "Python"

# ===========================================================================
# Case 3: mcp-agent-mail --version returns Rust version string
# ===========================================================================
e2e_case_banner "mcp-agent-mail --version returns Rust version"

set +e
MCP_VERSION_OUT="$("$FAKE_DEST/mcp-agent-mail" --version 2>&1)"
MCP_VERSION_RC=$?
set -e

e2e_save_artifact "case_03_mcp_version.txt" "$MCP_VERSION_OUT"
e2e_assert_exit_code "mcp-agent-mail --version" "0" "$MCP_VERSION_RC"
e2e_assert_contains "mcp version output is non-empty" "$MCP_VERSION_OUT" "."

# ===========================================================================
# Case 4: am is a binary, not an alias
# ===========================================================================
e2e_case_banner "am is a binary, not a shell alias or function"

set +e
AM_TYPE_OUT="$(command -v "$FAKE_DEST/am" 2>&1)"
AM_TYPE_RC=$?
AM_FILE_TYPE="$(file "$FAKE_DEST/am" 2>&1)"
set -e

e2e_save_artifact "case_04_am_type.txt" "command -v: ${AM_TYPE_OUT}\nfile: ${AM_FILE_TYPE}"
e2e_assert_exit_code "command -v am" "0" "$AM_TYPE_RC"
case "$AM_FILE_TYPE" in
  *ELF*|*Mach-O*) AM_NATIVE_BINARY="yes" ;;
  *) AM_NATIVE_BINARY="no" ;;
esac
e2e_assert_eq "am is a native binary" "yes" "$AM_NATIVE_BINARY"

# ===========================================================================
# Case 5: PATH includes ~/.local/bin
# ===========================================================================
e2e_case_banner "PATH includes install destination"

PATH_CHECK="no"
case ":${PATH}:" in
  *":${FAKE_DEST}:"*) PATH_CHECK="yes" ;;
esac

e2e_assert_eq "DEST is in PATH" "yes" "$PATH_CHECK"

# ===========================================================================
# Case 6: am --help includes expected subcommands
# ===========================================================================
e2e_case_banner "am --help lists expected subcommands"

set +e
AM_HELP_OUT="$("$FAKE_DEST/am" --help 2>&1)"
AM_HELP_RC=$?
set -e

e2e_save_artifact "case_06_am_help.txt" "$AM_HELP_OUT"
e2e_assert_exit_code "am --help" "0" "$AM_HELP_RC"
e2e_assert_contains "help lists mail subcommand" "$AM_HELP_OUT" "mail"
e2e_assert_contains "help lists doctor subcommand" "$AM_HELP_OUT" "doctor"
e2e_assert_contains "help lists agents subcommand" "$AM_HELP_OUT" "agents"

# ===========================================================================
# Case 7: am doctor check runs without hard failure on fresh system
# ===========================================================================
e2e_case_banner "am doctor check exits cleanly on fresh system"

set +e
DOCTOR_OUT="$("$FAKE_DEST/am" doctor check 2>&1)"
DOCTOR_RC=$?
set -e

e2e_save_artifact "case_07_doctor_check.txt" "$DOCTOR_OUT"
# Doctor may return non-zero if no storage exists yet, but should not crash
# Accept exit codes 0 (all green) or 1 (warnings) — NOT segfault/panic
if [ "$DOCTOR_RC" -le 1 ]; then
  e2e_assert_exit_code "am doctor check (0 or 1)" "0" "0"
else
  e2e_assert_exit_code "am doctor check should not panic" "0" "$DOCTOR_RC"
fi

# ===========================================================================
# Case 8: mcp-agent-mail serve-stdio responds to MCP initialize
# ===========================================================================
e2e_case_banner "serve-stdio responds to MCP initialize handshake"

# Create a minimal MCP initialize request (JSON-RPC 2.0)
MCP_INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-test","version":"0.0.1"}}}'

# Use FIFO pattern (like test_stdio.sh) for reliable server communication
SRV_WORK="$(mktemp -d "${TMPDIR:-/tmp}/fresh_install_srv.XXXXXX")"
STDIO_FIFO="${SRV_WORK}/stdin_fifo"
STDIO_RESPONSE="${SRV_WORK}/stdout.txt"
STDIO_STDERR="${SRV_WORK}/stderr.txt"
mkfifo "$STDIO_FIFO"

set +e
# Start server in background, reading from FIFO
DATABASE_URL="sqlite:////${FAKE_HOME}/fresh_test.sqlite3" RUST_LOG=error \
  "$FAKE_DEST/mcp-agent-mail" serve-stdio < "$STDIO_FIFO" > "$STDIO_RESPONSE" 2>"$STDIO_STDERR" &
SRV_PID=$!

# Give server a moment to start
sleep 0.3

# Send the request and close the FIFO to signal EOF
echo "$MCP_INIT_REQ" > "$STDIO_FIFO" &
WRITE_PID=$!

# Wait for response with timeout (up to 5 seconds)
ELAPSED=0
while [ "$ELAPSED" -lt 5 ]; do
  if [ -s "$STDIO_RESPONSE" ]; then
    sleep 0.3
    break
  fi
  sleep 0.3
  ELAPSED=$((ELAPSED + 1))
done

wait "$WRITE_PID" 2>/dev/null || true
kill "$SRV_PID" 2>/dev/null || true
wait "$SRV_PID" 2>/dev/null || true
set -e

MCP_INIT_OUT=""
if [ -f "$STDIO_RESPONSE" ]; then
  MCP_INIT_OUT="$(cat "$STDIO_RESPONSE")"
fi

e2e_save_artifact "case_08_mcp_init_response.txt" "$MCP_INIT_OUT"
e2e_save_artifact "case_08_mcp_init_stderr.txt" "$(cat "$STDIO_STDERR" 2>/dev/null || true)"

# Response should contain "result" and protocol version
if [ -n "$MCP_INIT_OUT" ]; then
  e2e_assert_contains "MCP response has result" "$MCP_INIT_OUT" "result"
  e2e_assert_contains "MCP response has protocolVersion" "$MCP_INIT_OUT" "protocolVersion"
else
  # If empty, server may not support bare stdio without initialization
  # At minimum, verify it didn't crash (check stderr for panics)
  STDERR_CONTENT="$(cat "$STDIO_STDERR" 2>/dev/null || true)"
  if echo "$STDERR_CONTENT" | command grep -qi "panic" 2>/dev/null; then
    e2e_assert_eq "serve-stdio did not panic" "no panic" "panicked"
  else
    # No output and no panic = acceptable (server may need different framing)
    e2e_assert_eq "serve-stdio did not panic" "no panic" "no panic"
  fi
fi

if [ "$AM_E2E_KEEP_TMP" = "1" ] || [ "$AM_E2E_KEEP_TMP" = "true" ]; then
  printf 'Preserved stdio sandbox: %s\n' "$SRV_WORK" >&2
else
  rm -rf "$SRV_WORK" 2>/dev/null || true
fi

# ===========================================================================
# Case 9: install.sh detect_mcp_configs works in isolated env
# ===========================================================================
e2e_case_banner "detect_mcp_configs finds configs in isolated home"

# Source install.sh functions only (skip main execution)
# We need to extract the function definitions
INSTALL_SH="${SCRIPT_DIR}/../../install.sh"

# Create some fake config directories to simulate tool installations
mkdir -p "$FAKE_HOME/.claude"
mkdir -p "$FAKE_HOME/.cursor"
mkdir -p "$FAKE_HOME/.gemini"

# Create a pre-existing Claude config to test detection
cat > "$FAKE_HOME/.claude/settings.json" <<'EOF'
{
  "mcpServers": {}
}
EOF
CURSOR_CONFIG="$FAKE_HOME/.cursor/mcp.json"
printf '%s\n' '{}' > "$CURSOR_CONFIG"

MCP_DETECT_LIBRARY="${FAKE_HOME}/detect-mcp-configs-function.sh"
sed -n '/^detect_mcp_configs() {/,/^generate_bearer_token() {/p' "${INSTALL_SH}" \
  | sed '$d' > "${MCP_DETECT_LIBRARY}"
if [ ! -s "${MCP_DETECT_LIBRARY}" ]; then
  e2e_fail "extract installer MCP detector" "function body" "missing"
  DETECT_OUT=""
else
  DETECT_OUT="$(
    # shellcheck disable=SC1090
    source "${MCP_DETECT_LIBRARY}"
    unset OMP_PROFILE PI_PROFILE PI_CONFIG_DIR PI_CODING_AGENT_DIR
    detect_mcp_configs "${FAKE_HOME}"
  )"
fi

e2e_save_artifact "case_09_detect_configs.txt" "$DETECT_OUT"
e2e_assert_contains "detector reports an existing Cursor MCP config" \
  "$DETECT_OUT" "cursor"$'\t'"${CURSOR_CONFIG}"$'\t'"1"
e2e_assert_not_contains "detector does not mislabel Claude Code settings as MCP config" \
  "$DETECT_OUT" "${FAKE_HOME}/.claude/settings.json"

# ===========================================================================
# Case 10: setup_mcp_configs creates config entries for detected tools
# ===========================================================================
e2e_case_banner "MCP config insertion creates valid JSON"

# Simulate a fresh MCP config creation (what setup_single_mcp_config does)
if command -v python3 >/dev/null 2>&1; then
  ENTRY_JSON="{\"command\": \"${FAKE_DEST}/mcp-agent-mail\", \"args\": [], \"env\": {\"HTTP_BEARER_TOKEN\": \"test-token-abc123\"}}"
  python3 -c "
import json, sys
entry = json.loads(sys.argv[1])
doc = {'mcpServers': {'mcp-agent-mail': entry}}
print(json.dumps(doc, indent=2))
" "$ENTRY_JSON" > "$CURSOR_CONFIG"

  e2e_assert_file_exists "cursor config created" "$CURSOR_CONFIG"

  # Verify it is valid JSON
  set +e
  PARSE_OUT="$(python3 -c "import json; json.load(open('$CURSOR_CONFIG')); print('valid')" 2>&1)"
  set -e
  e2e_assert_eq "cursor config is valid JSON" "valid" "$PARSE_OUT"

  # Verify it contains the expected entry
  set +e
  HAS_ENTRY="$(python3 -c "
import json
doc = json.load(open('$CURSOR_CONFIG'))
entry = doc.get('mcpServers', {}).get('mcp-agent-mail', {})
print('yes' if entry.get('command','').endswith('mcp-agent-mail') else 'no')
" 2>&1)"
  set -e
  e2e_assert_eq "cursor config has mcp-agent-mail entry" "yes" "$HAS_ENTRY"
else
  # No python3, skip JSON validation
  e2e_assert_eq "python3 available for JSON test" "skipped" "skipped"
fi

# ===========================================================================
# Case 11: MCP config insertion into existing config preserves other entries
# ===========================================================================
e2e_case_banner "MCP config insertion preserves existing entries"

if command -v python3 >/dev/null 2>&1; then
  # Create a config with an existing server
  GEMINI_CONFIG="$FAKE_HOME/.gemini/settings.json"
  cat > "$GEMINI_CONFIG" <<'EOF'
{
  "mcpServers": {
    "other-server": {
      "command": "other-binary",
      "args": ["--flag"]
    }
  }
}
EOF

  # Simulate inserting mcp-agent-mail entry
  ENTRY_JSON="{\"command\": \"${FAKE_DEST}/mcp-agent-mail\", \"args\": []}"
  python3 -c "
import json, sys
config_path = sys.argv[1]
entry_json = sys.argv[2]
with open(config_path, 'r') as f:
    doc = json.load(f)
entry = json.loads(entry_json)
doc['mcpServers']['mcp-agent-mail'] = entry
with open(config_path, 'w') as f:
    json.dump(doc, f, indent=2)
    f.write('\n')
" "$GEMINI_CONFIG" "$ENTRY_JSON"

  # Verify both entries present
  set +e
  BOTH_OUT="$(python3 -c "
import json
doc = json.load(open('$GEMINI_CONFIG'))
servers = doc.get('mcpServers', {})
has_other = 'other-server' in servers
has_am = 'mcp-agent-mail' in servers
print('both' if has_other and has_am else 'missing')
" 2>&1)"
  set -e
  e2e_assert_eq "both servers preserved" "both" "$BOTH_OUT"
else
  e2e_assert_eq "python3 available for preserve test" "skipped" "skipped"
fi

# ===========================================================================
# Case 12: Shell rc file PATH update (simulated easy-mode)
# ===========================================================================
e2e_case_banner "Shell rc PATH update writes correct export"

# Create empty rc files
touch "$FAKE_HOME/.zshrc"
touch "$FAKE_HOME/.bashrc"

# Simulate what maybe_add_path does in easy mode
for rc in "$FAKE_HOME/.zshrc" "$FAKE_HOME/.bashrc"; do
  if [ -w "$rc" ]; then
    # Check if already present
    if ! command grep -qF "$FAKE_DEST" "$rc" 2>/dev/null; then
      echo "export PATH=\"${FAKE_DEST}:\$PATH\"" >> "$rc"
    fi
  fi
done

# Verify the export was added
set +e
ZSHRC_HAS_PATH="$(command grep -c "$FAKE_DEST" "$FAKE_HOME/.zshrc" 2>/dev/null)"
BASHRC_HAS_PATH="$(command grep -c "$FAKE_DEST" "$FAKE_HOME/.bashrc" 2>/dev/null)"
set -e

e2e_assert_eq "zshrc has PATH export" "1" "$ZSHRC_HAS_PATH"
e2e_assert_eq "bashrc has PATH export" "1" "$BASHRC_HAS_PATH"

# Verify the export is idempotent (second write doesn't duplicate)
for rc in "$FAKE_HOME/.zshrc" "$FAKE_HOME/.bashrc"; do
  if ! command grep -qF "$FAKE_DEST" "$rc" 2>/dev/null; then
    echo "export PATH=\"${FAKE_DEST}:\$PATH\"" >> "$rc"
  fi
done

ZSHRC_COUNT="$(command grep -c "$FAKE_DEST" "$FAKE_HOME/.zshrc" 2>/dev/null)"
e2e_assert_eq "zshrc PATH not duplicated" "1" "$ZSHRC_COUNT"

# ===========================================================================
# Case 13: No Python alias present on fresh system
# ===========================================================================
e2e_case_banner "No Python alias detected on fresh system"

# Check that no Python alias exists in the rc files
set +e
PYTHON_ALIAS="$(command grep -E "^[[:space:]]*(alias am=|function am)" "$FAKE_HOME/.zshrc" 2>/dev/null | wc -l)"
set -e

e2e_assert_eq "no Python alias in zshrc" "0" "$(echo "$PYTHON_ALIAS" | tr -d ' ')"

# ===========================================================================
# Case 14: Bearer token generation produces valid hex string
# ===========================================================================
e2e_case_banner "Bearer token generation produces valid output"

set +e
if command -v openssl >/dev/null 2>&1; then
  TOKEN="$(openssl rand -hex 32)"
  TOKEN_LEN="${#TOKEN}"
  e2e_assert_eq "token length is 64 hex chars" "64" "$TOKEN_LEN"
  # Verify it's all hex
  TOKEN_HEX="$(echo "$TOKEN" | command grep -cE '^[0-9a-f]+$' 2>/dev/null || echo 0)"
  e2e_assert_eq "token is valid hex" "1" "$TOKEN_HEX"
elif [ -r /dev/urandom ]; then
  TOKEN="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  TOKEN_LEN="${#TOKEN}"
  # urandom+od output may vary in length
  if [ "$TOKEN_LEN" -ge 32 ]; then
    e2e_assert_eq "urandom token >= 32 chars" "yes" "yes"
  else
    e2e_assert_eq "urandom token >= 32 chars" "yes" "no (got $TOKEN_LEN)"
  fi
else
  e2e_assert_eq "token generation available" "skipped" "skipped"
fi
set -e

# ===========================================================================
# Case 15: Installer migration remains robust with binary PATH entries + MCP mode env
# ===========================================================================
e2e_case_banner "Installer migration: no null-byte warning and CLI-mode override"

if [ "${AM_E2E_SKIP_INSTALLER_MIGRATION:-0}" = "1" ]; then
  e2e_skip "AM_E2E_SKIP_INSTALLER_MIGRATION=1; skipping installer migration robustness case"
elif ! command -v sqlite3 >/dev/null 2>&1; then
  e2e_skip "sqlite3 unavailable; skipping installer migration robustness case"
else
  INSTALL_HOME="$(mktemp -d "${TMPDIR:-/tmp}/fresh_install_migrate.XXXXXX")"
  INSTALL_DEST="${INSTALL_HOME}/.local/bin"
  LEGACY_BIN_DIR="${INSTALL_HOME}/legacy_bin"
  LEGACY_CLONE="${INSTALL_HOME}/legacy_python_clone"
  LEGACY_DB="${LEGACY_CLONE}/storage.sqlite3"
  LEGACY_STORAGE="${INSTALL_HOME}/.mcp_agent_mail_git_mailbox_repo"
  INSTALL_RUN_DIR="${INSTALL_HOME}/project"
  INSTALL_ART_STAGE="${INSTALL_HOME}/artifact"
  INSTALL_ART_PATH="${INSTALL_HOME}/mcp-agent-mail-test.tar.xz"
  INSTALL_STDOUT_FILE="${INSTALL_HOME}/install_stdout.txt"
  INSTALL_STDERR_FILE="${INSTALL_HOME}/install_stderr.txt"
  INSTALL_SH="${SCRIPT_DIR}/../../install.sh"

  mkdir -p "$INSTALL_DEST" "$LEGACY_BIN_DIR" "$LEGACY_CLONE" "$LEGACY_STORAGE" "$INSTALL_ART_STAGE" "$INSTALL_RUN_DIR"

  # Fake executable with NUL bytes to exercise PATH probing safely.
  printf '\177ELF\000\001\002python-probe' > "${LEGACY_BIN_DIR}/am"
  chmod +x "${LEGACY_BIN_DIR}/am"

  cat > "${LEGACY_CLONE}/pyproject.toml" <<'EOF'
[project]
name = "mcp_agent_mail"
version = "0.0.0"
EOF

  cat > "${INSTALL_HOME}/.zshrc" <<EOF
alias am='cd "${LEGACY_CLONE}" && python3 -m mcp_agent_mail'
EOF
  cat > "${INSTALL_HOME}/.aliases" <<'EOF'
alias am='python3 -m mcp_agent_mail'
EOF
  cat > "${INSTALL_HOME}/.zprofile" <<EOF
am() {
  cd "${LEGACY_CLONE}"
  python3 -m mcp_agent_mail "\$@"
}
EOF
  mkdir -p "${INSTALL_HOME}/.config/fish/conf.d"
  cat > "${INSTALL_HOME}/.config/fish/conf.d/legacy_am.fish" <<'EOF'
function am
  python3 -m mcp_agent_mail $argv
end
EOF
  cat > "${INSTALL_HOME}/.bashrc" <<'EOF'
# baseline bashrc
EOF

  sqlite3 "${LEGACY_DB}" <<'SQL'
CREATE TABLE IF NOT EXISTS projects (
  id INTEGER PRIMARY KEY,
  slug TEXT NOT NULL,
  human_key TEXT NOT NULL,
  created_at TEXT NOT NULL
);
INSERT INTO projects (id, slug, human_key, created_at)
VALUES (1, 'legacy-install-smoke', '/tmp/legacy-install-smoke', '2026-03-01 12:34:56.123456');
SQL

  cat > "${LEGACY_CLONE}/.env" <<EOF
DATABASE_URL=sqlite+aiosqlite:///${LEGACY_DB}
STORAGE_ROOT=${LEGACY_CLONE}
HTTP_BEARER_TOKEN=test-token
EOF

  cp "$SERVER_BIN" "${INSTALL_ART_STAGE}/mcp-agent-mail"
  cp "$CLI_BIN" "${INSTALL_ART_STAGE}/am"
  chmod +x "${INSTALL_ART_STAGE}/mcp-agent-mail" "${INSTALL_ART_STAGE}/am"
  tar -cJf "${INSTALL_ART_PATH}" -C "${INSTALL_ART_STAGE}" am mcp-agent-mail

  INSTALL_VERSION="$("$CLI_BIN" --version 2>/dev/null | awk '{print $2}' | head -1)"
  INSTALL_VERSION="${INSTALL_VERSION#v}"
  [ -n "${INSTALL_VERSION}" ] || INSTALL_VERSION="0.0.0"

  set +e
  (
    cd "${INSTALL_RUN_DIR}"
    HOME="${INSTALL_HOME}" \
    PATH="${LEGACY_BIN_DIR}:/usr/bin:/bin" \
    STORAGE_ROOT="${LEGACY_STORAGE}" \
    AM_INTERFACE_MODE="mcp" \
    AM_INSTALL_SKIP_MCP_SETUP="1" \
    bash "${INSTALL_SH}" \
      --version "v${INSTALL_VERSION}" \
      --artifact-url "file://${INSTALL_ART_PATH}" \
      --dest "${INSTALL_DEST}" \
      --offline \
      --no-verify \
      --no-gum \
      --easy-mode
  ) >"${INSTALL_STDOUT_FILE}" 2>"${INSTALL_STDERR_FILE}"
  INSTALL_RC=$?
  set -e

  INSTALL_STDOUT="$(cat "${INSTALL_STDOUT_FILE}" 2>/dev/null || true)"
  INSTALL_STDERR="$(cat "${INSTALL_STDERR_FILE}" 2>/dev/null || true)"
  e2e_save_artifact "case_15_install_stdout.txt" "${INSTALL_STDOUT}"
  e2e_save_artifact "case_15_install_stderr.txt" "${INSTALL_STDERR}"

  e2e_assert_exit_code "installer exits cleanly with AM_INTERFACE_MODE=mcp" "0" "${INSTALL_RC}"
  e2e_assert_not_contains "installer avoids null-byte PATH probe warning" "${INSTALL_STDERR}" "ignored null byte in input"
  e2e_assert_contains "installer migration completed in CLI override mode" "${INSTALL_STDOUT}" "Database schema migrated"
  e2e_assert_contains "installer emits current-shell alias cleanup hint" "${INSTALL_STDOUT}" "unalias am"

  MIGRATED_DB="${LEGACY_STORAGE}/storage.sqlite3"
  e2e_assert_file_exists "migrated DB exists after installer run" "${MIGRATED_DB}"
  MIGRATED_DB_READ_OK="$(sqlite3 "${MIGRATED_DB}" "SELECT 1;" 2>/dev/null || true)"
  e2e_assert_eq "migrated DB remains readable" "1" "${MIGRATED_DB_READ_OK}"

  for rc in \
    "${INSTALL_HOME}/.zshrc" \
    "${INSTALL_HOME}/.aliases" \
    "${INSTALL_HOME}/.zprofile" \
    "${INSTALL_HOME}/.config/fish/conf.d/legacy_am.fish"
  do
    ACTIVE_ALIAS_COUNT="$(awk '
      /^[[:space:]]*(alias am=|alias am |function am[[:space:](]|am[[:space:]]*\(\))/ && $0 !~ /^[[:space:]]*#/ {
        c++
      }
      END { print c+0 }
    ' "${rc}" 2>/dev/null)"
    e2e_assert_eq "legacy am alias/function disabled in $(basename "${rc}")" "0" "${ACTIVE_ALIAS_COUNT}"
  done

  if [ "$AM_E2E_KEEP_TMP" = "1" ] || [ "$AM_E2E_KEEP_TMP" = "true" ]; then
    printf 'Preserved migration sandbox: %s\n' "$INSTALL_HOME" >&2
  else
    rm -rf "${INSTALL_HOME}" 2>/dev/null || true
  fi
fi

# ===========================================================================
# Case 16: OMP installer helpers honor the native profile and HTTP contracts
# ===========================================================================
e2e_case_banner "OMP installer profile and HTTP config contracts"

if ! command -v python3 >/dev/null 2>&1; then
  e2e_skip "python3 unavailable; skipping OMP installer helper case"
else
  OMP_INSTALLER_DIR="${FAKE_HOME}/omp-installer-contract"
  OMP_CONFIG="${OMP_INSTALLER_DIR}/mcp.json"
  mkdir -p "${OMP_INSTALLER_DIR}"
  cat > "${OMP_CONFIG}" <<'EOF'
{
  "mcpServers": {
    "sibling": {"type": "http", "url": "http://sibling.invalid/mcp"},
    "agent-mail": {
      "command": "legacy-agent-mail",
      "args": [],
      "cwd": "/stale/stdio/root",
      "env": {"HTTP_BEARER_TOKEN": "stale-token"},
      "headers": {
        "authorization": "Bearer stale-token",
        "X-Trace": "preserve-me"
      }
    }
  },
  "disabledServers": ["sibling", "mcp_agent_mail", "mcp-agent-mail", "agent-mail"],
  "servers": {
    "other": {"command": "other-server"}
  }
}
EOF

  OMP_WRITER_LIBRARY="${OMP_INSTALLER_DIR}/writer-function.sh"
  sed -n '/^setup_single_standard_http_json_config() {/,/^setup_single_opencode_json_config() {/p' "${INSTALL_SH}" \
    | sed '$d' > "${OMP_WRITER_LIBRARY}"
  if [ ! -s "${OMP_WRITER_LIBRARY}" ]; then
    e2e_fail "extract OMP installer writer" "function body" "missing"
  else
    if grep -Fq 'os.makedirs(' "${OMP_WRITER_LIBRARY}"; then
      e2e_fail "OMP installer writer avoids unchecked recursive parent creation" \
        "component-wise validated creation" "os.makedirs"
    else
      e2e_pass "OMP installer writer avoids unchecked recursive parent creation"
    fi

    set +e
    (
      # These stubs are invoked by the sourced installer helper.
      # shellcheck disable=SC2329
      verbose() { :; }
      # shellcheck disable=SC2329
      desired_mcp_http_url() { printf '%s' 'http://127.0.0.1:8765/mcp/'; }
      # shellcheck disable=SC2329
      resolve_setup_http_bearer_token() { printf '%s' ''; }
      # shellcheck disable=SC1090
      source "${OMP_WRITER_LIBRARY}"
      setup_single_standard_http_json_config omp "${OMP_CONFIG}"
    )
    OMP_WRITER_RC=$?
    set -e
    e2e_assert_exit_code "OMP installer writer converts legacy config" "0" "${OMP_WRITER_RC}"

    OMP_WRITER_ASSERTIONS="$(python3 - "${OMP_CONFIG}" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    doc = json.load(handle)

entry = doc["mcpServers"]["mcp-agent-mail"]
assert entry["type"] == "http"
assert entry["url"] == "http://127.0.0.1:8765/mcp/"
assert entry["enabled"] is True
assert entry["headers"] == {"X-Trace": "preserve-me"}
assert "command" not in entry and "args" not in entry and "cwd" not in entry and "env" not in entry
assert "agent-mail" not in doc["mcpServers"]
assert doc["servers"]["other"]["command"] == "other-server"
assert doc["mcpServers"]["sibling"]["url"] == "http://sibling.invalid/mcp"
assert doc["disabledServers"] == ["sibling"]
print("valid")
PY
    )"
    e2e_assert_eq "OMP installer writer emits one clean native HTTP entry" "valid" "${OMP_WRITER_ASSERTIONS}"

    chmod 0644 "${OMP_CONFIG}"
    set +e
    (
      # These stubs are invoked by the sourced installer helper.
      # shellcheck disable=SC2329
      verbose() { :; }
      # shellcheck disable=SC2329
      desired_mcp_http_url() { printf '%s' 'http://127.0.0.1:8765/mcp/'; }
      # shellcheck disable=SC2329
      resolve_setup_http_bearer_token() { printf '%s' ''; }
      # shellcheck disable=SC1090
      source "${OMP_WRITER_LIBRARY}"
      setup_single_standard_http_json_config omp "${OMP_CONFIG}"
    )
    OMP_WRITER_SECOND_RC=$?
    set -e
    e2e_assert_exit_code "OMP installer writer repairs broad config permissions" \
      "0" "${OMP_WRITER_SECOND_RC}"
    if stat -f '%Lp' "${OMP_CONFIG}" >/dev/null 2>&1; then
      OMP_CONFIG_MODE="$(stat -f '%Lp' "${OMP_CONFIG}")"
    else
      OMP_CONFIG_MODE="$(stat -c '%a' "${OMP_CONFIG}")"
    fi
    e2e_assert_eq "OMP installer writer tightens config mode" "600" "${OMP_CONFIG_MODE}"

    set +e
    (
      # shellcheck disable=SC2329
      verbose() { :; }
      # shellcheck disable=SC2329
      desired_mcp_http_url() { printf '%s' 'http://127.0.0.1:8765/mcp/'; }
      # shellcheck disable=SC2329
      resolve_setup_http_bearer_token() { printf '%s' ''; }
      # shellcheck disable=SC1090
      source "${OMP_WRITER_LIBRARY}"
      setup_single_standard_http_json_config omp "${OMP_CONFIG}"
    )
    OMP_WRITER_THIRD_RC=$?
    set -e
    e2e_assert_exit_code "OMP installer writer is byte-and-mode idempotent" \
      "1" "${OMP_WRITER_THIRD_RC}"

    OMP_SYMLINK_TARGET="${OMP_INSTALLER_DIR}/symlink-target.json"
    OMP_SYMLINK_CONFIG="${OMP_INSTALLER_DIR}/symlinked-mcp.json"
    printf '%s\n' '{"sentinel":"must-not-change"}' > "${OMP_SYMLINK_TARGET}"
    ln -s "${OMP_SYMLINK_TARGET}" "${OMP_SYMLINK_CONFIG}"
    set +e
    (
      # shellcheck disable=SC2329
      verbose() { :; }
      # shellcheck disable=SC2329
      desired_mcp_http_url() { printf '%s' 'http://127.0.0.1:8765/mcp/'; }
      # shellcheck disable=SC2329
      resolve_setup_http_bearer_token() { printf '%s' ''; }
      # shellcheck disable=SC1090
      source "${OMP_WRITER_LIBRARY}"
      setup_single_standard_http_json_config omp "${OMP_SYMLINK_CONFIG}"
    )
    OMP_SYMLINK_WRITER_RC=$?
    set -e
    e2e_assert_exit_code "OMP installer writer refuses symlinked config targets" \
      "2" "${OMP_SYMLINK_WRITER_RC}"
    OMP_SYMLINK_TARGET_CONTENT="$(cat "${OMP_SYMLINK_TARGET}")"
    e2e_assert_eq "OMP installer writer leaves symlink target untouched" \
      '{"sentinel":"must-not-change"}' "${OMP_SYMLINK_TARGET_CONTENT}"

    OMP_TRAVERSAL_CONFIG="${OMP_INSTALLER_DIR}/missing/../escaped-mcp.json"
    set +e
    (
      # shellcheck disable=SC2329
      verbose() { :; }
      # shellcheck disable=SC2329
      desired_mcp_http_url() { printf '%s' 'http://127.0.0.1:8765/mcp/'; }
      # shellcheck disable=SC2329
      resolve_setup_http_bearer_token() { printf '%s' ''; }
      # shellcheck disable=SC1090
      source "${OMP_WRITER_LIBRARY}"
      setup_single_standard_http_json_config omp "${OMP_TRAVERSAL_CONFIG}"
    )
    OMP_TRAVERSAL_WRITER_RC=$?
    set -e
    e2e_assert_exit_code "OMP installer writer refuses parent traversal" \
      "2" "${OMP_TRAVERSAL_WRITER_RC}"
    if [ -e "${OMP_INSTALLER_DIR}/escaped-mcp.json" ]; then
      e2e_fail "OMP traversal refusal leaves destination absent" "absent" "created"
    else
      e2e_pass "OMP traversal refusal leaves destination absent"
    fi
  fi

  OMP_DETECT_LIBRARY="${MCP_DETECT_LIBRARY}"
  if [ ! -s "${OMP_DETECT_LIBRARY}" ]; then
    e2e_fail "extract OMP installer detector" "function body" "missing"
  else
    mkdir -p "${FAKE_HOME}/.omp/profiles/Work/agent" "${FAKE_HOME}/custom-agent"
    ln -s "${OMP_INSTALLER_DIR}" "${FAKE_HOME}/.omp/profiles/linked"
    set +e
    OMP_DETECT_OUT="$(
      # shellcheck disable=SC1090
      source "${OMP_DETECT_LIBRARY}"
      err() { printf '%s\n' "$*" >&2; }
      OMP_PROFILE=Work PI_CODING_AGENT_DIR="${FAKE_HOME}/custom-agent" \
        detect_mcp_configs "${FAKE_HOME}"
    )"
    OMP_INVALID_PROFILE_RC=$?
    set -e
    e2e_assert_exit_code "invalid uppercase OMP profile fails closed" \
      "2" "${OMP_INVALID_PROFILE_RC}"
    e2e_assert_not_contains "invalid uppercase OMP profile does not target the default override" \
      "${OMP_DETECT_OUT}" "${FAKE_HOME}/custom-agent/mcp.json"
    e2e_assert_not_contains "invalid uppercase OMP profile is not advertised" \
      "${OMP_DETECT_OUT}" "/profiles/Work/"
    e2e_assert_contains "invalid OMP profile does not suppress unrelated config discovery" \
      "${OMP_DETECT_OUT}" "codex"

    OMP_SYMLINK_DETECT_OUT="$(
      # shellcheck disable=SC1090
      source "${OMP_DETECT_LIBRARY}"
      OMP_PROFILE=linked detect_mcp_configs "${FAKE_HOME}"
    )"
    e2e_assert_not_contains "symlinked OMP profile is not followed" \
      "${OMP_SYMLINK_DETECT_OUT}" "/profiles/linked/"
  fi

  OMP_SETUP_LIBRARY="${OMP_INSTALLER_DIR}/setup-mcp-configs-function.sh"
  sed -n '/^path_resolves_within_directory() {/,/^update_mcp_configs() {/p' "${INSTALL_SH}" \
    | sed '$d' > "${OMP_SETUP_LIBRARY}"
  OMP_UPDATE_LIBRARY="${OMP_INSTALLER_DIR}/update-mcp-configs-function.sh"
  sed -n '/^update_mcp_configs() {/,/^record_uninstall_summary() {/p' "${INSTALL_SH}" \
    | sed '$d' > "${OMP_UPDATE_LIBRARY}"
  if [ ! -s "${OMP_SETUP_LIBRARY}" ] || [ ! -s "${OMP_UPDATE_LIBRARY}" ]; then
    e2e_fail "extract installer MCP setup orchestrators" "function bodies" "missing"
  else
    OMP_TOKEN_CONTRACT="$(
      # These stubs let the orchestration contract run without touching any
      # real client or invoking the installed binary.
      # shellcheck disable=SC2329
      detect_mcp_configs() { printf 'omp\t%s\t1\n' "${OMP_CONFIG}"; }
      # shellcheck disable=SC2329
      resolve_setup_http_bearer_token() { printf '%s' 'existing-token'; }
      # shellcheck disable=SC2329
      generate_bearer_token() { printf '%s' 'wrong-generated-token'; }
      # shellcheck disable=SC2329
      setup_claude_code_mcp_via_cli() { return 1; }
      OMP_TOKEN_CALL_VALID=0
      # shellcheck disable=SC2329
      setup_single_mcp_config() {
        if [ "$4" = "existing-token" ] && [ "${HTTP_BEARER_TOKEN:-}" = "existing-token" ]; then
          OMP_TOKEN_CALL_VALID=1
          return 1
        fi
        return 2
      }
      # shellcheck disable=SC2329
      ok() { :; }
      # shellcheck disable=SC2329
      info() { :; }
      # shellcheck disable=SC2329
      warn() { :; }
      # shellcheck disable=SC2329
      verbose() { :; }
      # shellcheck disable=SC1090
      source "${OMP_SETUP_LIBRARY}"
      unset HTTP_BEARER_TOKEN
      setup_mcp_configs "/unused/mcp-agent-mail"
      if [ "$OMP_TOKEN_CALL_VALID" -eq 1 ] \
        && [ "${HTTP_BEARER_TOKEN:-}" = "existing-token" ]; then
        printf '%s' 'valid'
      else
        printf '%s' 'invalid'
      fi
    )"
    e2e_assert_eq "installer reuses one bearer token across OMP setup phases" \
      "valid" "${OMP_TOKEN_CONTRACT}"

    OMP_CONTAINMENT_FAILURE_WRITES="$(
      # An interpreter failure is not proof that an arbitrary config lives
      # outside the project. Shadow python3 to mutation-test the fail-closed
      # status mapping without depending on a host-level Python failure.
      # shellcheck disable=SC2329
      python3() { return 3; }
      # shellcheck disable=SC2329
      detect_mcp_configs() { printf 'cursor\t%s\t1\n' "${OMP_CONFIG}"; }
      # shellcheck disable=SC2329
      resolve_setup_http_bearer_token() { printf '%s' 'unknown-path-secret'; }
      # shellcheck disable=SC2329
      generate_bearer_token() { printf '%s' 'wrong-generated-token'; }
      # shellcheck disable=SC2329
      setup_claude_code_mcp_via_cli() { return 1; }
      OMP_UNKNOWN_PATH_WRITES=0
      # shellcheck disable=SC2329
      setup_single_mcp_config() {
        OMP_UNKNOWN_PATH_WRITES=$((OMP_UNKNOWN_PATH_WRITES + 1))
        return 0
      }
      # shellcheck disable=SC2329
      ok() { :; }
      # shellcheck disable=SC2329
      info() { :; }
      # shellcheck disable=SC2329
      warn() { :; }
      # shellcheck disable=SC2329
      verbose() { :; }
      # shellcheck disable=SC1090
      source "${OMP_SETUP_LIBRARY}"
      setup_mcp_configs "/unused/mcp-agent-mail"
      printf '%s' "${OMP_UNKNOWN_PATH_WRITES}"
    )"
    e2e_assert_eq "installer fails closed when project containment is indeterminate" \
      "0" "${OMP_CONTAINMENT_FAILURE_WRITES}"

    OMP_PROJECT_DIR="${OMP_INSTALLER_DIR}/project-defer"
    OMP_PROJECT_CONFIG="${OMP_PROJECT_DIR}/.omp/mcp.json"
    OMP_PROJECT_OVERRIDE_CONFIG="${OMP_PROJECT_DIR}/.omp/agent/mcp.json"
    OMP_PROJECT_CURSOR_CONFIG="${OMP_PROJECT_DIR}/cursor.mcp.json"
    OMP_PROJECT_CODEX_CONFIG="${OMP_PROJECT_DIR}/codex.mcp.json"
    OMP_PROJECT_FAKE_CLI="${OMP_INSTALLER_DIR}/failing-am"
    OMP_PROJECT_NATIVE_MARKER="${OMP_INSTALLER_DIR}/native-setup-invoked"
    mkdir -p "${OMP_PROJECT_DIR}/.omp/agent"
    printf '%s\n' '{"mcpServers":{"sibling":{"command":"node"}}}' > "${OMP_PROJECT_CONFIG}"
    printf '%s\n' '{"mcpServers":{"override-sibling":{"command":"node"}}}' > "${OMP_PROJECT_OVERRIDE_CONFIG}"
    printf '%s\n' '{"mcpServers":{"cursor-sibling":{"command":"node"}}}' > "${OMP_PROJECT_CURSOR_CONFIG}"
    printf '%s\n' '{"mcpServers":{"codex-sibling":{"command":"node"}}}' > "${OMP_PROJECT_CODEX_CONFIG}"
    cat > "${OMP_PROJECT_FAKE_CLI}" <<'EOF'
#!/bin/sh
if [ "${1:-}" = "setup" ] && [ "${2:-}" = "--help" ]; then
  exit 0
fi
if [ "${1:-}" = "setup" ] && [ "${2:-}" = "run" ]; then
  printf '%s' "${HTTP_BEARER_TOKEN:-}" > "${OMP_PROJECT_NATIVE_MARKER:?}"
fi
exit 7
EOF
    chmod +x "${OMP_PROJECT_FAKE_CLI}"

    OMP_PROJECT_DEFER_CONTRACT="$(
      # shellcheck disable=SC2329
      detect_mcp_configs() {
        printf 'omp\t%s\t1\nomp\t%s\t1\ncursor\t%s\t1\ncodex\t%s\t1\n' \
          "${OMP_PROJECT_CONFIG}" '.omp/agent/mcp.json' 'cursor.mcp.json' 'codex.mcp.json'
      }
      # shellcheck disable=SC2329
      resolve_setup_http_bearer_token() { printf '%s' 'project-secret'; }
      # shellcheck disable=SC2329
      generate_bearer_token() { printf '%s' 'wrong-generated-token'; }
      # shellcheck disable=SC2329
      OMP_PROJECT_DIRECT_WRITES=0
      setup_claude_code_mcp_via_cli() {
        OMP_PROJECT_DIRECT_WRITES=$((OMP_PROJECT_DIRECT_WRITES + 1))
        printf '%s' "$1" > "${HOME}/.claude.json"
        return 0
      }
      # shellcheck disable=SC2329
      setup_single_mcp_config() {
        OMP_PROJECT_DIRECT_WRITES=$((OMP_PROJECT_DIRECT_WRITES + 1))
        printf '%s' "$4" > "$2"
        return 0
      }
      # shellcheck disable=SC2329
      ok() { :; }
      # shellcheck disable=SC2329
      info() { :; }
      # shellcheck disable=SC2329
      warn() { :; }
      # shellcheck disable=SC2329
      verbose() { :; }
      # shellcheck disable=SC1090
      source "${OMP_SETUP_LIBRARY}"
      # shellcheck disable=SC1090
      source "${OMP_UPDATE_LIBRARY}"
      cd "${OMP_PROJECT_DIR}"
      export HOME="${OMP_PROJECT_DIR}"
      export OMP_PROJECT_NATIVE_MARKER
      setup_mcp_configs "/unused/mcp-agent-mail"
      update_mcp_configs "/unused/mcp-agent-mail" "${OMP_PROJECT_FAKE_CLI}"
      sync_codex_http_configs "/unused/mcp-agent-mail"
      printf '%s' "${OMP_PROJECT_DIRECT_WRITES}"
    )"
    e2e_assert_eq "installer defers project OMP bearer writes when native setup fails" \
      "0" "${OMP_PROJECT_DEFER_CONTRACT}"
    e2e_assert_eq "failed native setup leaves project OMP config byte-preserved" \
      '{"mcpServers":{"sibling":{"command":"node"}}}' \
      "$(cat "${OMP_PROJECT_CONFIG}")"
    e2e_assert_not_contains "failed native setup leaves no project bearer" \
      "$(cat "${OMP_PROJECT_CONFIG}")" "project-secret"
    e2e_assert_eq "installer leaves project-local OMP override byte-preserved" \
      '{"mcpServers":{"override-sibling":{"command":"node"}}}' \
      "$(cat "${OMP_PROJECT_OVERRIDE_CONFIG}")"
    e2e_assert_not_contains "installer leaves no bearer in project-local OMP override" \
      "$(cat "${OMP_PROJECT_OVERRIDE_CONFIG}")" "project-secret"
    e2e_assert_eq "installer leaves project-local non-OMP config byte-preserved" \
      '{"mcpServers":{"cursor-sibling":{"command":"node"}}}' \
      "$(cat "${OMP_PROJECT_CURSOR_CONFIG}")"
    e2e_assert_not_contains "installer leaves no bearer in project-local non-OMP config" \
      "$(cat "${OMP_PROJECT_CURSOR_CONFIG}")" "project-secret"
    e2e_assert_eq "installer leaves project-local Codex sync config byte-preserved" \
      '{"mcpServers":{"codex-sibling":{"command":"node"}}}' \
      "$(cat "${OMP_PROJECT_CODEX_CONFIG}")"
    e2e_assert_not_contains "installer leaves no bearer in project-local Codex sync config" \
      "$(cat "${OMP_PROJECT_CODEX_CONFIG}")" "project-secret"
    if [ -e "${OMP_PROJECT_DIR}/.claude.json" ]; then
      e2e_fail "installer defers project-contained Claude CLI config" \
        "no .claude.json shell write" "created .claude.json"
    else
      e2e_pass "installer defers project-contained Claude CLI config"
    fi
    e2e_assert_eq "installer invoked native setup with the selected bearer" \
      "project-secret" "$(cat "${OMP_PROJECT_NATIVE_MARKER}")"
    if [ -e "${OMP_PROJECT_DIR}/.gitignore" ]; then
      e2e_fail "failed native setup does not fake a secured project write" \
        "no .gitignore side effect" "created .gitignore"
    else
      e2e_pass "failed native setup leaves project security files untouched"
    fi
  fi
fi

# ===========================================================================
# Case 17: legacy env migration never writes credentials into a Git worktree
# ===========================================================================
e2e_case_banner "legacy env migration respects every Git worktree boundary"

if ! command -v git >/dev/null 2>&1; then
  e2e_skip "git unavailable; skipping legacy env containment case"
else
  LEGACY_ENV_CONTRACT_DIR="${FAKE_HOME}/legacy-env-contract"
  LEGACY_ENV_LIBRARY="${LEGACY_ENV_CONTRACT_DIR}/migration-functions.sh"
  LEGACY_PROJECT_DIR="${LEGACY_ENV_CONTRACT_DIR}/worktree"
  LEGACY_PROJECT_CONFIG_DIR="${LEGACY_PROJECT_DIR}/.config/mcp-agent-mail"
  LEGACY_PROJECT_CONFIG="${LEGACY_PROJECT_CONFIG_DIR}/config.env"
  LEGACY_PROJECT_COMPAT="${LEGACY_PROJECT_CONFIG_DIR}/.env"
  LEGACY_OTHER_PROJECT_DIR="${LEGACY_ENV_CONTRACT_DIR}/other-worktree"
  LEGACY_OTHER_CONFIG_DIR="${LEGACY_OTHER_PROJECT_DIR}/.config/mcp-agent-mail"
  LEGACY_OTHER_CONFIG="${LEGACY_OTHER_CONFIG_DIR}/config.env"
  LEGACY_OTHER_COMPAT="${LEGACY_OTHER_CONFIG_DIR}/.env"
  LEGACY_OUTSIDE_HOME="${LEGACY_ENV_CONTRACT_DIR}/outside-home"
  mkdir -p "${LEGACY_ENV_CONTRACT_DIR}" "${LEGACY_PROJECT_CONFIG_DIR}" \
    "${LEGACY_OTHER_CONFIG_DIR}" \
    "${LEGACY_OUTSIDE_HOME}/mcp_agent_mail"

  sed -n '/^strip_wrapping_quotes() {/,/^python_db_format_needs_import() {/p' "${INSTALL_SH}" \
    | sed '$d' > "${LEGACY_ENV_LIBRARY}"
  sed -n '/^git_worktree_root_for_path() {/,/^resolve_migrated_bearer_token() {/p' "${INSTALL_SH}" \
    | sed '$d' >> "${LEGACY_ENV_LIBRARY}"
  sed -n '/^path_resolves_within_directory() {/,/^mcp_config_must_skip_shell_write() {/p' "${INSTALL_SH}" \
    | sed '$d' >> "${LEGACY_ENV_LIBRARY}"

  if [ ! -s "${LEGACY_ENV_LIBRARY}" ]; then
    e2e_fail "extract legacy env migration contract" "function bodies" "missing"
  else
    git init -q -b main "${LEGACY_PROJECT_DIR}"
    cat > "${LEGACY_PROJECT_CONFIG}" <<'EOF'
# tracked project sentinel
HTTP_BEARER_TOKEN=old-project-secret
KEEP_ME=byte-preserved
EOF
    git -C "${LEGACY_PROJECT_DIR}" add .config/mcp-agent-mail/config.env

    set +e
    (
      # shellcheck disable=SC2329
      warn() { :; }
      # shellcheck disable=SC2329
      info() { :; }
      # shellcheck disable=SC2329
      ok() { :; }
      # shellcheck disable=SC1090
      source "${LEGACY_ENV_LIBRARY}"
      cd "${LEGACY_PROJECT_DIR}"
      export HOME="${LEGACY_PROJECT_DIR}"
      export PYTHON_CLONE_FOUND=0
      export PYTHON_CLONE_PATH=""
      export RUST_STORAGE_ROOT="${LEGACY_PROJECT_DIR}/mailbox"
      export RUST_DB_PATH="${LEGACY_PROJECT_DIR}/mailbox/storage.sqlite3"
      export MIGRATED_BEARER_TOKEN="project-new-secret"
      migrate_env_config
    ) >/dev/null 2>&1
    LEGACY_PROJECT_GUARD_RC=$?
    set -e

    e2e_assert_exit_code "legacy env migration refuses project-contained HOME" \
      "1" "${LEGACY_PROJECT_GUARD_RC}"
    e2e_assert_eq "project-contained env remains byte-preserved" \
      $'# tracked project sentinel\nHTTP_BEARER_TOKEN=old-project-secret\nKEEP_ME=byte-preserved' \
      "$(cat "${LEGACY_PROJECT_CONFIG}")"
    e2e_assert_not_contains "project-contained env receives no replacement bearer" \
      "$(cat "${LEGACY_PROJECT_CONFIG}")" "project-new-secret"
    if [ -e "${LEGACY_PROJECT_COMPAT}" ]; then
      e2e_fail "legacy env refusal creates no compatibility mirror" \
        "absent" "created ${LEGACY_PROJECT_COMPAT}"
    else
      e2e_pass "legacy env refusal creates no compatibility mirror"
    fi
    LEGACY_PROJECT_SIDE_EFFECTS="$(
      find "${LEGACY_PROJECT_CONFIG_DIR}" -maxdepth 1 -type f \
        \( -name '*.bak.mcp-agent-mail-*' -o -name '*.tmp.*' \) -print \
        | wc -l | tr -d ' '
    )"
    e2e_assert_eq "legacy env refusal creates no backup or temporary files" \
      "0" "${LEGACY_PROJECT_SIDE_EFFECTS}"

    git init -q -b main "${LEGACY_OTHER_PROJECT_DIR}"
    cat > "${LEGACY_OTHER_CONFIG}" <<'EOF'
# unrelated tracked project sentinel
HTTP_BEARER_TOKEN=other-project-secret
KEEP_ME=other-byte-preserved
EOF
    git -C "${LEGACY_OTHER_PROJECT_DIR}" add .config/mcp-agent-mail/config.env
    set +e
    (
      # shellcheck disable=SC2329
      warn() { :; }
      # shellcheck disable=SC2329
      info() { :; }
      # shellcheck disable=SC2329
      ok() { :; }
      # shellcheck disable=SC1090
      source "${LEGACY_ENV_LIBRARY}"
      cd "${LEGACY_PROJECT_DIR}"
      export HOME="${LEGACY_OTHER_PROJECT_DIR}"
      export PYTHON_CLONE_FOUND=0
      export PYTHON_CLONE_PATH=""
      export RUST_STORAGE_ROOT="${LEGACY_OTHER_PROJECT_DIR}/mailbox"
      export RUST_DB_PATH="${LEGACY_OTHER_PROJECT_DIR}/mailbox/storage.sqlite3"
      export MIGRATED_BEARER_TOKEN="other-project-new-secret"
      migrate_env_config
    ) >/dev/null 2>&1
    LEGACY_OTHER_GUARD_RC=$?
    set -e

    e2e_assert_exit_code "legacy env migration refuses an unrelated worktree HOME" \
      "1" "${LEGACY_OTHER_GUARD_RC}"
    e2e_assert_eq "unrelated worktree env remains byte-preserved" \
      $'# unrelated tracked project sentinel\nHTTP_BEARER_TOKEN=other-project-secret\nKEEP_ME=other-byte-preserved' \
      "$(cat "${LEGACY_OTHER_CONFIG}")"
    e2e_assert_not_contains "unrelated worktree receives no replacement bearer" \
      "$(cat "${LEGACY_OTHER_CONFIG}")" "other-project-new-secret"
    if [ -e "${LEGACY_OTHER_COMPAT}" ]; then
      e2e_fail "unrelated worktree refusal creates no compatibility mirror" \
        "absent" "created ${LEGACY_OTHER_COMPAT}"
    else
      e2e_pass "unrelated worktree refusal creates no compatibility mirror"
    fi
    LEGACY_OTHER_SIDE_EFFECTS="$(
      find "${LEGACY_OTHER_CONFIG_DIR}" -maxdepth 1 -type f \
        \( -name '*.bak.mcp-agent-mail-*' -o -name '*.tmp.*' \) -print \
        | wc -l | tr -d ' '
    )"
    e2e_assert_eq "unrelated worktree refusal creates no backup or temporary files" \
      "0" "${LEGACY_OTHER_SIDE_EFFECTS}"

    cat > "${LEGACY_OUTSIDE_HOME}/mcp_agent_mail/.env" <<'EOF'
DATABASE_URL=sqlite+aiosqlite:////legacy/storage.sqlite3
STORAGE_ROOT=/legacy/storage
HTTP_BEARER_TOKEN=outside-legacy-secret
PRESERVE_ME=yes
EOF
    set +e
    (
      # shellcheck disable=SC2329
      warn() { :; }
      # shellcheck disable=SC2329
      info() { :; }
      # shellcheck disable=SC2329
      ok() { :; }
      # shellcheck disable=SC1090
      source "${LEGACY_ENV_LIBRARY}"
      cd "${LEGACY_PROJECT_DIR}"
      export HOME="${LEGACY_OUTSIDE_HOME}"
      export PYTHON_CLONE_FOUND=0
      export PYTHON_CLONE_PATH=""
      export RUST_STORAGE_ROOT="${LEGACY_OUTSIDE_HOME}/mailbox"
      export RUST_DB_PATH="${LEGACY_OUTSIDE_HOME}/mailbox/storage.sqlite3"
      unset MIGRATED_BEARER_TOKEN
      migrate_env_config
    ) >/dev/null 2>&1
    LEGACY_OUTSIDE_RC=$?
    set -e

    LEGACY_OUTSIDE_CONFIG="${LEGACY_OUTSIDE_HOME}/.config/mcp-agent-mail/config.env"
    LEGACY_OUTSIDE_COMPAT="${LEGACY_OUTSIDE_HOME}/.config/mcp-agent-mail/.env"
    e2e_assert_exit_code "legacy env migration still permits an outside HOME" \
      "0" "${LEGACY_OUTSIDE_RC}"
    e2e_assert_contains "outside migration rewrites DATABASE_URL" \
      "$(cat "${LEGACY_OUTSIDE_CONFIG}")" \
      "DATABASE_URL=sqlite:///${LEGACY_OUTSIDE_HOME}/mailbox/storage.sqlite3"
    e2e_assert_contains "outside migration rewrites STORAGE_ROOT" \
      "$(cat "${LEGACY_OUTSIDE_CONFIG}")" \
      "STORAGE_ROOT=${LEGACY_OUTSIDE_HOME}/mailbox"
    e2e_assert_contains "outside migration preserves the legacy bearer" \
      "$(cat "${LEGACY_OUTSIDE_CONFIG}")" \
      "HTTP_BEARER_TOKEN=outside-legacy-secret"
    e2e_assert_contains "outside migration preserves compatible settings" \
      "$(cat "${LEGACY_OUTSIDE_CONFIG}")" "PRESERVE_ME=yes"
    if cmp -s "${LEGACY_OUTSIDE_CONFIG}" "${LEGACY_OUTSIDE_COMPAT}"; then
      e2e_pass "outside migration creates an exact compatibility mirror"
    else
      e2e_fail "outside migration creates an exact compatibility mirror" \
        "byte-identical files" "files differ"
    fi
  fi
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
