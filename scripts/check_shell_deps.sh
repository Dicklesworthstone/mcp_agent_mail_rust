#!/usr/bin/env bash
# check_shell_deps.sh — Anti-regression guard for external process dependencies.
#
# Scans Rust source for Command::new / std::process::Command usage and
# validates each occurrence against an explicit allowlist. Fails if
# unapproved shell-outs are found.
#
# Usage:
#   bash scripts/check_shell_deps.sh          # Full scan
#   bash scripts/check_shell_deps.sh --json   # Machine-readable output
#
# Exit codes:
#   0 = no violations found
#   1 = unapproved shell-outs detected
#   2 = usage error

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

JSON_MODE=0
if [ "${1:-}" = "--json" ]; then
    JSON_MODE=1
fi

# ─── Allowlist ────────────────────────────────────────────────────────
#
# Each entry is a relative path from project root. Files listed here are
# approved to use std::process::Command. Adding a new file requires
# explicit review and a comment explaining the justification.
#
# Categories:
#   RUNTIME   — needed when running the server/CLI in production
#   CLI_OPS   — operator CLI commands that invoke external tools
#   TEST      — test infrastructure (spawns binaries under test)
#
# To approve a new file: add it below with a comment, and update the
# "approved_count" assertion at the bottom of this script.
ALLOWLIST=(
    # ── RUNTIME: core process dependencies ──────────────────────────
    # Git identity discovery (rev-parse, branch, config)
    "crates/mcp-agent-mail-core/src/identity.rs"
    # Optional TOON encoder (configurable via TOON_BIN env var)
    "crates/mcp-agent-mail-core/src/toon.rs"
    # Flake triage utility (runs cargo test)
    "crates/mcp-agent-mail-core/src/flake_triage.rs"
    # Git archive operations (commit coalescing)
    "crates/mcp-agent-mail-storage/src/lib.rs"
    # Pre-commit guard hook (git diff, reservation enforcement)
    "crates/mcp-agent-mail-guard/src/lib.rs"
    # Server cleanup (process management)
    "crates/mcp-agent-mail-server/src/cleanup.rs"
    # Mail UI static file serving (asset management)
    "crates/mcp-agent-mail-server/src/mail_ui.rs"

    # ── CLI_OPS: operator CLI tool commands ─────────────────────────
    # Deployment shell executor (user-specified deploy commands)
    "crates/mcp-agent-mail-share/src/executor.rs"
    # Agent detection (coding_agent_session_search)
    "crates/mcp-agent-mail-share/src/detection.rs"
    # Crypto operations (key generation, signing)
    "crates/mcp-agent-mail-share/src/crypto.rs"
    # Deployment hosting probes (curl-like checks)
    "crates/mcp-agent-mail-share/src/probe.rs"
    # Deployment hosting setup
    "crates/mcp-agent-mail-share/src/hosting.rs"
    # CLI main dispatch (runs subprocesses for some commands)
    "crates/mcp-agent-mail-cli/src/lib.rs"
    # E2E test runner (spawns server binary)
    "crates/mcp-agent-mail-cli/src/e2e_runner.rs"
    # E2E artifact management
    "crates/mcp-agent-mail-cli/src/e2e_artifacts.rs"
    # Benchmark runner
    "crates/mcp-agent-mail-cli/src/bench.rs"
    # CI gate runner
    "crates/mcp-agent-mail-cli/src/ci.rs"
    # Golden file management
    "crates/mcp-agent-mail-cli/src/golden.rs"
    # MCP resource implementations (build info)
    "crates/mcp-agent-mail-tools/src/resources.rs"

    # ── TEST: test infrastructure ───────────────────────────────────
    # Conformance test harness
    "crates/mcp-agent-mail-conformance/src/main.rs"
    "crates/mcp-agent-mail-conformance/tests/conformance.rs"
    "crates/mcp-agent-mail-conformance/tests/contact_enforcement_outage.rs"
    # CLI integration tests
    "crates/mcp-agent-mail-cli/tests/mode_matrix_harness.rs"
    "crates/mcp-agent-mail-cli/tests/perf_security_regressions.rs"
    "crates/mcp-agent-mail-cli/tests/golden_integration.rs"
    "crates/mcp-agent-mail-cli/tests/flake_triage_integration.rs"
    "crates/mcp-agent-mail-cli/tests/ci_integration.rs"
    "crates/mcp-agent-mail-cli/tests/integration_runs.rs"
    "crates/mcp-agent-mail-cli/tests/cli_json_snapshots.rs"
    "crates/mcp-agent-mail-cli/tests/semantic_conformance.rs"
    "crates/mcp-agent-mail-cli/tests/help_snapshots.rs"
    "crates/mcp-agent-mail-cli/tests/share_verify_decrypt.rs"
)

# ─── Denylist patterns ────────────────────────────────────────────────
#
# These patterns should NEVER appear in non-test Rust source code.
# They indicate Python/shell dependencies creeping back in.
# Test files (*/tests/*.rs, conformance/) are excluded from denylist
# checks since they may need to invoke external tools for testing.
DENYLIST_PATTERNS=(
    # Python interpreter invocations
    'Command::new\("python'
    'Command::new\("python3'
    'Command::new\("pip'
    'Command::new\("pip3'
    # Node.js
    'Command::new\("node'
    'Command::new\("npm'
    'Command::new\("npx'
    # Ruby
    'Command::new\("ruby'
    'Command::new\("gem'
)

# Files/directories excluded from denylist checks (test infrastructure).
DENYLIST_EXCLUDE_DIRS=(
    "crates/mcp-agent-mail-cli/tests/"
    "crates/mcp-agent-mail-conformance/tests/"
    "crates/mcp-agent-mail-conformance/src/"
)

# ─── Scan ─────────────────────────────────────────────────────────────

VIOLATIONS=()
DENYLIST_HITS=()

# Build allowlist lookup (associative array)
declare -A ALLOWED
for f in "${ALLOWLIST[@]}"; do
    ALLOWED["$f"]=1
done

# Find all .rs files with Command::new or std::process::Command
while IFS= read -r file; do
    # Make path relative to project root
    rel_path="${file#"$PROJECT_ROOT"/}"

    if [ -z "${ALLOWED[$rel_path]+x}" ]; then
        VIOLATIONS+=("$rel_path")
    fi
done < <(grep -rl --include='*.rs' -E 'Command::new|std::process::Command' "$PROJECT_ROOT/crates/" 2>/dev/null || true)

# Check denylist patterns across non-test Rust source
for pattern in "${DENYLIST_PATTERNS[@]}"; do
    while IFS= read -r match; do
        if [ -n "$match" ]; then
            # Check if match is in an excluded directory
            excluded=0
            for excl in "${DENYLIST_EXCLUDE_DIRS[@]}"; do
                if [[ "$match" == *"$excl"* ]]; then
                    excluded=1
                    break
                fi
            done
            if [ "$excluded" -eq 0 ]; then
                DENYLIST_HITS+=("$match")
            fi
        fi
    done < <(grep -rn --include='*.rs' -E "$pattern" "$PROJECT_ROOT/crates/" 2>/dev/null || true)
done

# ─── Report ───────────────────────────────────────────────────────────

VIOLATION_COUNT=${#VIOLATIONS[@]}
DENYLIST_COUNT=${#DENYLIST_HITS[@]}
TOTAL_ISSUES=$((VIOLATION_COUNT + DENYLIST_COUNT))

if [ "$JSON_MODE" -eq 1 ]; then
    # JSON output for CI integration
    violations_json="[]"
    if [ "$VIOLATION_COUNT" -gt 0 ]; then
        violations_json=$(printf '%s\n' "${VIOLATIONS[@]}" | python3 -c "
import json, sys
print(json.dumps([line.strip() for line in sys.stdin if line.strip()]))
" 2>/dev/null || printf '%s\n' "${VIOLATIONS[@]}" | while IFS= read -r v; do echo "\"$v\""; done | paste -sd',' | sed 's/^/[/;s/$/]/')
    fi

    denylist_json="[]"
    if [ "$DENYLIST_COUNT" -gt 0 ]; then
        denylist_json=$(printf '%s\n' "${DENYLIST_HITS[@]}" | python3 -c "
import json, sys
print(json.dumps([line.strip() for line in sys.stdin if line.strip()]))
" 2>/dev/null || printf '%s\n' "${DENYLIST_HITS[@]}" | while IFS= read -r v; do echo "\"$v\""; done | paste -sd',' | sed 's/^/[/;s/$/]/')
    fi

    cat <<EOF
{
  "check": "shell_deps_anti_regression",
  "status": $([ "$TOTAL_ISSUES" -eq 0 ] && echo '"pass"' || echo '"fail"'),
  "approved_files": ${#ALLOWLIST[@]},
  "violations": $violations_json,
  "violation_count": $VIOLATION_COUNT,
  "denylist_hits": $denylist_json,
  "denylist_hit_count": $DENYLIST_COUNT
}
EOF
else
    # Human-readable output
    echo "Shell/Python Dependency Anti-Regression Check"
    echo "=============================================="
    echo ""
    echo "Approved files with process spawning: ${#ALLOWLIST[@]}"
    echo ""

    if [ "$VIOLATION_COUNT" -gt 0 ]; then
        echo "VIOLATIONS: $VIOLATION_COUNT unapproved file(s) use Command::new:"
        for v in "${VIOLATIONS[@]}"; do
            echo "  - $v"
        done
        echo ""
        echo "To fix: Either remove the process spawning, or add the file to"
        echo "the allowlist in scripts/check_shell_deps.sh with a justification comment."
        echo ""
    fi

    if [ "$DENYLIST_COUNT" -gt 0 ]; then
        echo "DENYLIST VIOLATIONS: $DENYLIST_COUNT forbidden dependency pattern(s):"
        for h in "${DENYLIST_HITS[@]}"; do
            echo "  $h"
        done
        echo ""
        echo "Python, Node.js, and Ruby dependencies are NOT allowed in Rust source."
        echo "Use native Rust implementations instead."
        echo ""
    fi

    if [ "$TOTAL_ISSUES" -eq 0 ]; then
        echo "PASS: No unapproved shell/Python dependencies found."
    else
        echo "FAIL: $TOTAL_ISSUES issue(s) detected."
    fi
fi

exit "$( [ "$TOTAL_ISSUES" -eq 0 ] && echo 0 || echo 1 )"
