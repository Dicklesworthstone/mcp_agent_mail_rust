#!/usr/bin/env bash
#
# scripts/lint_git_shellouts.sh — br-8ujfs.4.2 (D2)
#
# Fail the build if any NEW production-path `Command::new("git")`
# shell-out appears outside the allow-listed exceptions.
#
# Sites that may shell out directly (no run_git_locked / GitCmd):
#   - crates/mcp-agent-mail-guard/src/*          (pre-commit hook process
#     runs inside user's git process; same pid; cannot deadlock; guard
#     gets SIGSEGV-retry via bead E5 instead)
#   - crates/mcp-agent-mail-conformance/tests/** (Python-parity fixtures
#     deliberately use raw git subprocess)
#   - crates/**/tests/**                         (test-only fixtures)
#   - crates/**/benches/**                       (benchmark fixtures)
#   - crates/mcp-agent-mail-core/src/git_cmd.rs  (the helper ITSELF —
#     this is where the single allowed Command::new sits)
#   - crates/mcp-agent-mail-core/src/git_binary.rs (the --version probe)
#   - crates/mcp-agent-mail-server/src/cleanup.rs line 554 onwards —
#     streaming `git ls-files` with kill-on-match; routed through the
#     resolved binary path so AM_GIT_BINARY works. Documented exception.
#   - crates/mcp-agent-mail-test-helpers/src/parity.rs — parity harness
#     CLI invocation.
#
# Anything else is a regression.
#
# Exit codes:
#   0 — clean
#   1 — regression detected (lists offenders)
#   2 — tooling failure (ripgrep missing, repo not found, etc.)

set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v rg >/dev/null 2>&1; then
    echo "scripts/lint_git_shellouts.sh: ripgrep (rg) not found on PATH" >&2
    exit 2
fi

# Find all Command::new("git") hits in production code, excluding
# intentional sites.
#
# In-source #[cfg(test)] modules: we filter those out by detecting the
# `mod tests {` or `mod <x>_tests {` boundary in each file and only
# considering lines BEFORE it. Sites after that boundary are
# necessarily test code.
#
# Per-site suppression: lines preceded by a comment containing
# "shellout-allowed:" (anywhere on the prior 3 lines) are skipped.
raw_hits=$(
    rg -n 'Command::new\("git"\)' crates/ \
        --glob '!**/tests/**' \
        --glob '!**/benches/**' \
        --glob '!crates/mcp-agent-mail-guard/**' \
        --glob '!crates/mcp-agent-mail-conformance/**' \
        --glob '!crates/mcp-agent-mail-core/src/git_cmd.rs' \
        --glob '!crates/mcp-agent-mail-core/src/git_binary.rs' \
        --glob '!crates/mcp-agent-mail-core/src/identity.rs' \
        --glob '!crates/mcp-agent-mail-test-helpers/**' \
        --glob '!crates/mcp-agent-mail-server/src/cleanup.rs' \
        2>/dev/null || true
)

# Filter out hits that are inside #[cfg(test)] modules OR preceded by
# a "shellout-allowed:" comment within 3 lines above.
#
# Boundary heuristic: a test-only scope is declared by `mod tests {`
# (or `mod <name>_tests {`). A bare `#[cfg(test)]` preceding a single
# item (fn / struct) does NOT flip the whole file into test scope —
# production code often declares single test helpers intermixed with
# production. We detect ONLY the `mod ...tests {` boundary, and treat
# everything after it as test scope.
hits=$(
    awk -v raw="$raw_hits" 'BEGIN {
        n = split(raw, lines, "\n")
        for (i = 1; i <= n; i++) {
            if (lines[i] == "") continue
            # Split file:line:content
            colon1 = index(lines[i], ":")
            if (colon1 == 0) continue
            file = substr(lines[i], 1, colon1 - 1)
            rest = substr(lines[i], colon1 + 1)
            colon2 = index(rest, ":")
            if (colon2 == 0) continue
            line_num = substr(rest, 1, colon2 - 1) + 0

            # Read file once, cache the FIRST `mod *tests {` boundary.
            # Only `mod tests {` / `mod <name>_tests {` counts —
            # bare `#[cfg(test)]` on a single fn/struct does not flip
            # the entire file below it into test scope.
            if (!(file in test_boundary)) {
                test_boundary[file] = 1e12
                fline = 0
                while ((getline ln < file) > 0) {
                    fline++
                    if (ln ~ /^[[:space:]]*mod tests[[:space:]]*\{/ \
                        || ln ~ /^[[:space:]]*mod [a-z_]+_tests[[:space:]]*\{/ \
                        || ln ~ /^[[:space:]]*pub mod tests[[:space:]]*\{/) {
                        if (fline < test_boundary[file]) test_boundary[file] = fline
                    }
                }
                close(file)
            }
            if (line_num >= test_boundary[file]) continue

            # Read 3 lines above for allow-list suppression comment.
            suppressed = 0
            start = line_num - 3
            if (start < 1) start = 1
            fline = 0
            while ((getline ln < file) > 0) {
                fline++
                if (fline >= start && fline < line_num) {
                    if (ln ~ /shellout-allowed:/) { suppressed = 1; break }
                }
                if (fline >= line_num) break
            }
            close(file)
            if (!suppressed) print lines[i]
        }
    }'
)

if [ -n "$hits" ]; then
    echo "BLOCK: $(wc -l <<<"$hits") new unwrapped git shell-out(s) detected." >&2
    echo >&2
    echo "Replace with:" >&2
    echo "    mcp_agent_mail_core::git_cmd::GitCmd::new(repo).args([...]).run()?" >&2
    echo >&2
    echo "See docs/GIT_SHELLOUT_AUDIT.md for the allow-list and migration recipe." >&2
    echo >&2
    echo "Offenders:" >&2
    echo "$hits" >&2
    exit 1
fi

# Also count the grandfathered sites and verify they haven't drifted
# upward — drift without audit doc update is a signal.
count_file="docs/GIT_SHELLOUT_AUDIT_COUNT.txt"
if [ -f "$count_file" ]; then
    expected=$(awk -F= '/^TOTAL_OCCURRENCES=/ {print $2}' "$count_file")
    actual=$(rg -c 'Command::new\("git"\)' crates/ 2>/dev/null \
        | awk -F: '{s+=$2} END {print s+0}')
    if [ "$actual" -gt "$expected" ]; then
        echo "WARN: shell-out count rose from $expected (audit baseline) to $actual." >&2
        echo "      Update docs/GIT_SHELLOUT_AUDIT.md + GIT_SHELLOUT_AUDIT_COUNT.txt or remove the new shell-outs." >&2
        # Warn-not-fail on count drift if the regex allow-list above
        # already accepted every site. The allow-list is the strict gate.
    fi
fi

echo "scripts/lint_git_shellouts.sh: OK — no new unwrapped git shell-outs in production code."
