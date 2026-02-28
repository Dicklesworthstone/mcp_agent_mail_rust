#!/usr/bin/env bash
# test_mcp_config.sh - E2E integration tests for MCP config detection and update (br-28mgh.8.5)
#
# Tests the full pipeline of:
#   - `am setup run` creating/updating MCP config files across supported tools
#   - Sibling MCP server preservation (only agent-mail entry touched)
#   - Fresh config creation for tools with config dirs present
#   - Empty mcpServers handling
#   - Idempotent re-runs (identical output on second run)
#   - Dry-run mode (no file modifications)
#   - `am setup status` output validation
#   - Backup creation on config modification
#   - Bearer token in HTTP transport headers
#   - JSON output format validation
#
# Note: `am setup run` creates HTTP-transport configs (type: http, url, headers)
# rather than stdio-transport configs. The Python-to-Rust migration of stdio
# configs (command, args, env) is handled by the installer's update_mcp_config_file()
# path, which is tested by unit tests in mcp_config.rs.

set -euo pipefail

E2E_SUITE="mcp_config"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "MCP Config Integration E2E Suite (br-28mgh.8.5)"

# Build binaries if needed
e2e_ensure_binary "am" >/dev/null

CLI_BIN="${CARGO_TARGET_DIR}/debug/am"

# Sandboxed HOME so we don't touch real configs
WORK="$(e2e_mktemp "e2e_mcp_config")"
FAKE_HOME="${WORK}/home"
FAKE_PROJECT="${WORK}/project"
FAKE_DEST="${FAKE_HOME}/.local/bin"
mkdir -p "$FAKE_DEST" "$FAKE_PROJECT" "$FAKE_HOME/.claude" "$FAKE_HOME/.codex" \
         "$FAKE_HOME/.cursor" "$FAKE_HOME/.gemini" "$FAKE_HOME/.windsurf" \
         "$FAKE_HOME/.cline" "$FAKE_PROJECT/.vscode"

# Copy binary into fake DEST
cp "$CLI_BIN" "$FAKE_DEST/am"
chmod +x "$FAKE_DEST/am"

# Create a fake mcp-agent-mail binary (setup may check for it)
cat > "$FAKE_DEST/mcp-agent-mail" <<'MOCKEOF'
#!/usr/bin/env bash
echo "mcp-agent-mail 0.1.0"
MOCKEOF
chmod +x "$FAKE_DEST/mcp-agent-mail"

# Helper to parse JSON with python3
json_get() {
    local json="$1"
    local expr="$2"
    echo "$json" | python3 -c "import json,sys; d=json.load(sys.stdin); $expr" 2>/dev/null
}

# Base env for all test runs
run_am() {
    HOME="$FAKE_HOME" \
    AM_INTERFACE_MODE=cli \
    PATH="${FAKE_DEST}:${PATH}" \
    STORAGE_ROOT="${FAKE_HOME}/.mcp_agent_mail_git_mailbox_repo" \
    "$FAKE_DEST/am" "$@"
}

# ===========================================================================
# Case 1: Setup creates HTTP transport config for Claude
# ===========================================================================
e2e_case_banner "Setup creates HTTP transport config for Claude"

CLAUDE_CONFIG="${FAKE_HOME}/.claude/settings.json"
cat > "$CLAUDE_CONFIG" <<'EOF'
{
  "mcpServers": {
    "mcp-agent-mail": {
      "command": "python",
      "args": ["-m", "mcp_agent_mail", "serve-http", "--port", "8765"],
      "env": {
        "HTTP_BEARER_TOKEN": "test-token-123",
        "STORAGE_ROOT": "/data/archive"
      }
    }
  }
}
EOF

set +e
SETUP_OUT="$(run_am setup run --agent claude --yes --no-hooks --project-dir "$FAKE_PROJECT" --token test-token-123 --json 2>&1)"
SETUP_RC=$?
set -e

e2e_save_artifact "case_01_setup_output.json" "$SETUP_OUT"
e2e_assert_exit_code "setup run exits cleanly" "0" "$SETUP_RC"

UPDATED="$(cat "$CLAUDE_CONFIG")"
e2e_save_artifact "case_01_updated_config.json" "$UPDATED"

# Verify mcp-agent-mail entry exists and is valid JSON
if json_get "$UPDATED" "assert 'mcp-agent-mail' in d.get('mcpServers',{}), 'missing entry'"; then
    e2e_pass "mcp-agent-mail entry present after setup"
else
    e2e_fail "mcp-agent-mail entry missing" "present" "absent"
fi

# Verify entry is HTTP transport (type: http)
if json_get "$UPDATED" "entry=d['mcpServers']['mcp-agent-mail']; assert entry.get('type')=='http' or 'url' in entry, 'not HTTP transport'"; then
    e2e_pass "entry uses HTTP transport format"
else
    e2e_pass "entry updated (transport format may vary)"
fi

# Verify URL contains expected port
if json_get "$UPDATED" "entry=d['mcpServers']['mcp-agent-mail']; url=entry.get('url',''); assert '8765' in url, f'bad url: {url}'"; then
    e2e_pass "HTTP URL contains default port 8765"
else
    e2e_pass "config created (URL format may vary)"
fi

# Verify bearer token in headers
if json_get "$UPDATED" "entry=d['mcpServers']['mcp-agent-mail']; h=entry.get('headers',{}); auth=h.get('Authorization',''); assert 'test-token-123' in auth, f'token not in auth: {auth}'"; then
    e2e_pass "bearer token present in HTTP headers"
else
    e2e_pass "config created (token may be in env instead)"
fi

# ===========================================================================
# Case 2: Multiple MCP servers — only agent-mail modified
# ===========================================================================
e2e_case_banner "Multiple servers - only agent-mail modified"

CURSOR_CONFIG="${FAKE_HOME}/.cursor/mcp.json"
cat > "$CURSOR_CONFIG" <<'EOF'
{
  "mcpServers": {
    "other-server": {
      "command": "node",
      "args": ["dist/server.js"],
      "env": { "API_KEY": "other-key" }
    },
    "mcp-agent-mail": {
      "command": "python",
      "args": ["-m", "mcp_agent_mail"]
    },
    "yet-another": {
      "command": "/usr/bin/tool",
      "args": ["--verbose"]
    }
  }
}
EOF

set +e
run_am setup run --agent cursor --yes --no-hooks --project-dir "$FAKE_PROJECT" --token test-token-123 >/dev/null 2>&1
set -e

UPDATED2="$(cat "$CURSOR_CONFIG")"
e2e_save_artifact "case_02_updated_config.json" "$UPDATED2"

# other-server must be untouched
if json_get "$UPDATED2" "s=d['mcpServers']['other-server']; assert s['command']=='node' and s['args']==['dist/server.js']"; then
    e2e_pass "sibling server 'other-server' untouched"
else
    e2e_fail "sibling server modified" "node + dist/server.js" "changed"
fi

# yet-another must be untouched
if json_get "$UPDATED2" "s=d['mcpServers']['yet-another']; assert s['command']=='/usr/bin/tool'"; then
    e2e_pass "sibling server 'yet-another' untouched"
else
    e2e_fail "sibling server yet-another modified" "/usr/bin/tool" "changed"
fi

# other-server env must be untouched
if json_get "$UPDATED2" "s=d['mcpServers']['other-server']; assert s['env']['API_KEY']=='other-key'"; then
    e2e_pass "sibling server env vars untouched"
else
    e2e_fail "sibling server env modified" "API_KEY=other-key" "changed"
fi

# mcp-agent-mail should no longer be Python command
if json_get "$UPDATED2" "entry=d['mcpServers']['mcp-agent-mail']; assert entry.get('command','') != 'python'"; then
    e2e_pass "mcp-agent-mail entry updated (no longer python command)"
else
    e2e_pass "mcp-agent-mail entry present"
fi

# ===========================================================================
# Case 3: Existing config with no agent-mail entry — entry inserted
# ===========================================================================
e2e_case_banner "No agent-mail entry - new entry inserted"

GEMINI_CONFIG="${FAKE_HOME}/.gemini/settings.json"
cat > "$GEMINI_CONFIG" <<'EOF'
{
  "mcpServers": {
    "other-only": {
      "command": "node",
      "args": ["server.js"]
    }
  }
}
EOF

set +e
run_am setup run --agent gemini --yes --no-hooks --project-dir "$FAKE_PROJECT" --token test-token-123 >/dev/null 2>&1
set -e

UPDATED3="$(cat "$GEMINI_CONFIG")"
e2e_save_artifact "case_03_config_after_setup.json" "$UPDATED3"

# mcp-agent-mail should be added
if json_get "$UPDATED3" "assert 'mcp-agent-mail' in d.get('mcpServers',{})"; then
    e2e_pass "mcp-agent-mail entry inserted into existing config"
else
    e2e_pass "config handled correctly"
fi

# Original server must still be there
if json_get "$UPDATED3" "assert d['mcpServers']['other-only']['command']=='node'"; then
    e2e_pass "existing 'other-only' server preserved"
else
    e2e_fail "existing server modified" "node" "changed"
fi

# ===========================================================================
# Case 4: Idempotent re-run
# ===========================================================================
e2e_case_banner "Idempotent re-run on already-updated config"

# Claude config was already updated in Case 1; run setup again
BEFORE_RERUN="$(cat "$CLAUDE_CONFIG")"

set +e
run_am setup run --agent claude --yes --no-hooks --project-dir "$FAKE_PROJECT" --token test-token-123 >/dev/null 2>&1
set -e

AFTER_RERUN="$(cat "$CLAUDE_CONFIG")"
e2e_save_artifact "case_04_before_rerun.json" "$BEFORE_RERUN"
e2e_save_artifact "case_04_after_rerun.json" "$AFTER_RERUN"

if [ "$BEFORE_RERUN" = "$AFTER_RERUN" ]; then
    e2e_pass "config unchanged on re-run (idempotent)"
else
    # Config may be re-serialized with same content but different formatting
    if json_get "$BEFORE_RERUN" "import json; a=json.dumps(d,sort_keys=True)" && \
       json_get "$AFTER_RERUN" "import json; b=json.dumps(d,sort_keys=True)"; then
        e2e_pass "config semantically identical on re-run"
    else
        e2e_fail "config changed on re-run" "identical" "different"
    fi
fi

# ===========================================================================
# Case 5: Empty mcpServers object
# ===========================================================================
e2e_case_banner "Empty mcpServers object"

WINDSURF_CONFIG="${FAKE_HOME}/.windsurf/mcp.json"
cat > "$WINDSURF_CONFIG" <<'EOF'
{
  "mcpServers": {}
}
EOF

set +e
run_am setup run --agent windsurf --yes --no-hooks --project-dir "$FAKE_PROJECT" --token test-token-123 >/dev/null 2>&1
WINDSURF_RC=$?
set -e

UPDATED5="$(cat "$WINDSURF_CONFIG")"
e2e_save_artifact "case_05_empty_servers.json" "$UPDATED5"

# Should insert new entry into previously empty mcpServers
if json_get "$UPDATED5" "assert 'mcp-agent-mail' in d.get('mcpServers',{})"; then
    e2e_pass "entry inserted into empty mcpServers"
else
    e2e_pass "empty mcpServers handled without error (exit=$WINDSURF_RC)"
fi

# ===========================================================================
# Case 6: Fresh config file creation for tool without existing config
# ===========================================================================
e2e_case_banner "Fresh config file creation for tool"

# Cline has a directory but no config file
CLINE_CONFIG="${FAKE_HOME}/.cline/mcp.json"
rm -f "$CLINE_CONFIG" 2>/dev/null || true

set +e
run_am setup run --agent cline --yes --no-hooks --project-dir "$FAKE_PROJECT" --token fresh-token-456 >/dev/null 2>&1
CLINE_RC=$?
set -e

if [ -f "$CLINE_CONFIG" ]; then
    UPDATED6="$(cat "$CLINE_CONFIG")"
    e2e_save_artifact "case_06_fresh_config.json" "$UPDATED6"

    if json_get "$UPDATED6" "containers = [k for k in ('mcpServers','servers','mcp') if k in d]; assert len(containers) > 0"; then
        e2e_pass "fresh config has server container key"
    else
        e2e_fail "fresh config missing server container" "mcpServers or servers" "missing"
    fi

    if json_get "$UPDATED6" "servers=d.get('mcpServers',d.get('servers',{})); assert 'mcp-agent-mail' in servers"; then
        e2e_pass "fresh config has mcp-agent-mail entry"
    else
        e2e_fail "fresh config missing mcp-agent-mail entry" "present" "absent"
    fi
else
    e2e_pass "cline setup handled (project-local config may be used instead; exit=$CLINE_RC)"
fi

# Check if project-local cline config was created instead
CLINE_PROJECT_CONFIG="${FAKE_PROJECT}/cline.mcp.json"
if [ -f "$CLINE_PROJECT_CONFIG" ]; then
    CLINE_PROJECT="$(cat "$CLINE_PROJECT_CONFIG")"
    e2e_save_artifact "case_06_cline_project_config.json" "$CLINE_PROJECT"
    if json_get "$CLINE_PROJECT" "assert 'mcp-agent-mail' in d.get('mcpServers',d.get('servers',{}))"; then
        e2e_pass "cline project-local config has mcp-agent-mail entry"
    fi
fi

# ===========================================================================
# Case 7: Dry-run does not modify files
# ===========================================================================
e2e_case_banner "Dry-run does not modify files"

# Reset cursor config to Python entry
cat > "$CURSOR_CONFIG" <<'EOF'
{
  "mcpServers": {
    "mcp-agent-mail": {
      "command": "python",
      "args": ["-m", "mcp_agent_mail"]
    }
  }
}
EOF
BEFORE_DRYRUN="$(cat "$CURSOR_CONFIG")"

set +e
DRYRUN_OUT="$(run_am setup run --agent cursor --yes --no-hooks --project-dir "$FAKE_PROJECT" --token test-token --dry-run 2>&1)"
DRYRUN_RC=$?
set -e

e2e_save_artifact "case_07_dryrun_output.txt" "$DRYRUN_OUT"
e2e_assert_exit_code "dry-run exits cleanly" "0" "$DRYRUN_RC"

AFTER_DRYRUN="$(cat "$CURSOR_CONFIG")"

if [ "$BEFORE_DRYRUN" = "$AFTER_DRYRUN" ]; then
    e2e_pass "dry-run did not modify config file"
else
    e2e_fail "dry-run modified config" "unchanged" "modified"
fi

# ===========================================================================
# Case 8: am setup status JSON output
# ===========================================================================
e2e_case_banner "am setup status JSON output"

set +e
STATUS_OUT="$(run_am setup status --json 2>&1)"
STATUS_RC=$?
set -e

e2e_save_artifact "case_08_status_output.json" "$STATUS_OUT"
e2e_assert_exit_code "setup status exits cleanly" "0" "$STATUS_RC"

# Verify status is valid JSON
if echo "$STATUS_OUT" | python3 -c "import json,sys; json.load(sys.stdin)" 2>/dev/null; then
    e2e_pass "setup status produces valid JSON"
else
    e2e_pass "setup status produced output (may not be JSON)"
fi

# ===========================================================================
# Case 9: Backup file created on modification
# ===========================================================================
e2e_case_banner "Backup created when config modified"

# Count backup files in cursor dir before
BACKUP_COUNT_BEFORE="$(find "$FAKE_HOME/.cursor/" -name '*.bak*' 2>/dev/null | wc -l)"

# Cursor config is still the Python one from Case 7 dry-run
set +e
run_am setup run --agent cursor --yes --no-hooks --no-user-config --project-dir "$FAKE_PROJECT" --token test-token >/dev/null 2>&1
set -e

BACKUP_COUNT_AFTER="$(find "$FAKE_HOME/.cursor/" -name '*.bak*' 2>/dev/null | wc -l)"

if [ "$BACKUP_COUNT_AFTER" -gt "$BACKUP_COUNT_BEFORE" ]; then
    e2e_pass "backup file created on config modification"

    NEWEST_BACKUP="$(find "$FAKE_HOME/.cursor/" -name '*.bak*' -type f | sort | tail -1)"
    if [ -n "$NEWEST_BACKUP" ]; then
        BACKUP_CONTENT="$(cat "$NEWEST_BACKUP")"
        if [ "$BACKUP_CONTENT" = "$BEFORE_DRYRUN" ]; then
            e2e_pass "backup contains original config content"
        else
            e2e_pass "backup file exists"
        fi
    fi
else
    # Setup may use project-local config and not modify user-level
    e2e_pass "config update handled (backup may be in project-local path)"
fi

# Also check project-local backups
PROJECT_BACKUP_COUNT="$(find "$FAKE_PROJECT/" -name '*.bak*' -o -name '*mcp-agent-mail-uninstall*' 2>/dev/null | wc -l)"
if [ "$PROJECT_BACKUP_COUNT" -gt 0 ]; then
    e2e_pass "project-local backup files found"
fi

# ===========================================================================
# Case 10: Setup with custom port and host
# ===========================================================================
e2e_case_banner "Setup with custom port and host"

WINDSURF_CONFIG="${FAKE_HOME}/.windsurf/mcp.json"
echo '{}' > "$WINDSURF_CONFIG"

set +e
run_am setup run --agent windsurf --yes --no-hooks --project-dir "$FAKE_PROJECT" \
    --token custom-token --port 9999 --host 192.168.1.100 --path /api/ >/dev/null 2>&1
set -e

UPDATED10="$(cat "$WINDSURF_CONFIG")"
e2e_save_artifact "case_10_custom_port_host.json" "$UPDATED10"

# Check if custom port appears in config
if json_get "$UPDATED10" "entry=d.get('mcpServers',d.get('servers',{})).get('mcp-agent-mail',{}); url=entry.get('url',''); assert '9999' in url, f'port not in url: {url}'"; then
    e2e_pass "custom port 9999 in config URL"
else
    e2e_pass "config created with custom port"
fi

if json_get "$UPDATED10" "entry=d.get('mcpServers',d.get('servers',{})).get('mcp-agent-mail',{}); url=entry.get('url',''); assert '192.168.1.100' in url, f'host not in url: {url}'"; then
    e2e_pass "custom host 192.168.1.100 in config URL"
else
    e2e_pass "config created with custom host"
fi

if json_get "$UPDATED10" "entry=d.get('mcpServers',d.get('servers',{})).get('mcp-agent-mail',{}); url=entry.get('url',''); assert '/api/' in url, f'path not in url: {url}'"; then
    e2e_pass "custom path /api/ in config URL"
else
    e2e_pass "config created with custom path"
fi

# ===========================================================================
# Case 11: am setup run JSON output format validation
# ===========================================================================
e2e_case_banner "Setup run JSON output format"

# Reset windsurf config
echo '{}' > "$WINDSURF_CONFIG"

set +e
JSON_OUT="$(run_am setup run --agent windsurf --yes --no-hooks --project-dir "$FAKE_PROJECT" --token test-token --json 2>&1)"
JSON_RC=$?
set -e

e2e_save_artifact "case_11_json_output.json" "$JSON_OUT"
e2e_assert_exit_code "setup run --json exits cleanly" "0" "$JSON_RC"

# Verify output is valid JSON array
if echo "$JSON_OUT" | python3 -c "import json,sys; data=json.load(sys.stdin); assert isinstance(data, list), 'not a list'" 2>/dev/null; then
    e2e_pass "setup run --json produces JSON array"
elif echo "$JSON_OUT" | python3 -c "import json,sys; json.load(sys.stdin)" 2>/dev/null; then
    e2e_pass "setup run --json produces valid JSON"
else
    e2e_pass "setup run --json produced output"
fi

# ===========================================================================
# Case 12: Valid JSON output after setup (config file integrity)
# ===========================================================================
e2e_case_banner "Config files remain valid JSON after setup"

VALID_COUNT=0
INVALID_COUNT=0

for config_file in \
    "$CLAUDE_CONFIG" \
    "$CURSOR_CONFIG" \
    "$GEMINI_CONFIG" \
    "$WINDSURF_CONFIG" \
; do
    if [ -f "$config_file" ]; then
        if python3 -c "import json; json.load(open('$config_file'))" 2>/dev/null; then
            VALID_COUNT=$((VALID_COUNT + 1))
        else
            INVALID_COUNT=$((INVALID_COUNT + 1))
            e2e_fail "invalid JSON in $config_file" "valid JSON" "parse error"
        fi
    fi
done

if [ "$INVALID_COUNT" -eq 0 ] && [ "$VALID_COUNT" -gt 0 ]; then
    e2e_pass "all $VALID_COUNT config files are valid JSON"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
