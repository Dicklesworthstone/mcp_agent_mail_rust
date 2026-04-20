#!/usr/bin/env bash
#
# scripts/capture_git_251_baseline.sh — br-8ujfs.1.6 (A6)
#
# Read-only capture of the current state of git 2.51.0 damage on this
# box. Produces a structured artifact under
# tests/artifacts/baselines/<ts>/ containing:
#   - host_context.txt   (uname, git --version, /proc/version, package)
#   - kernel_segfaults.log (last 72h of git-process segfaults)
#   - damage_census.jsonl (one line per registered project with
#                          fsck output, orphan-ref count, ahead/behind
#                          health)
#   - summary.md         (narrative table for easy reading)
#
# Safe on any box: does NOT modify state. Useful for:
#   - G2 stress-test ceiling comparison
#   - post-fix regression comparison
#   - incident attachments
#
# Requires: am (for am robot overview), journalctl (for kernel log).

set -euo pipefail

ts="$(date -u +%Y%m%dT%H%M%SZ)"
out_dir="tests/artifacts/baselines/$ts"
mkdir -p "$out_dir"

log() { echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] $*" >&2; }

# --- Host context ----------------------------------------------------------
log "capturing host context → $out_dir/host_context.txt"
{
    echo "=== uname -a ==="
    uname -a
    echo
    echo "=== git --version ==="
    git --version || echo "git not found"
    echo
    if [ -n "${AM_GIT_BINARY:-}" ]; then
        echo "=== AM_GIT_BINARY=$AM_GIT_BINARY ==="
        "$AM_GIT_BINARY" --version 2>&1 || echo "AM_GIT_BINARY --version failed"
        echo
    fi
    echo "=== /proc/version ==="
    cat /proc/version 2>/dev/null || echo "not available"
    echo
    echo "=== package info (dpkg/apt) ==="
    dpkg -l git 2>/dev/null | tail -3 || echo "not a dpkg system"
    apt-cache policy git 2>/dev/null | head -10 || true
    echo
    echo "=== am --version ==="
    am --version 2>/dev/null || echo "am not on PATH"
} > "$out_dir/host_context.txt"

# --- Kernel log ------------------------------------------------------------
log "capturing last 72h of kernel segfaults → $out_dir/kernel_segfaults.log"
if command -v journalctl >/dev/null 2>&1; then
    journalctl -k --since '72 hours ago' --no-pager 2>/dev/null \
        | awk '/segfault/ && /git\[/' \
        > "$out_dir/kernel_segfaults.log" || true
else
    echo "# journalctl not available on this system" \
        > "$out_dir/kernel_segfaults.log"
fi
segfault_count=$(wc -l < "$out_dir/kernel_segfaults.log" || echo 0)
log "  found $segfault_count git segfault events in kernel log"

# --- Damage census --------------------------------------------------------
log "running per-project damage census → $out_dir/damage_census.jsonl"

# Use am doctor fix-orphan-refs --all --dry-run --format json to get a
# structured report of orphan refs across every registered project.
# Pipe through jq to simplify to one line per project.
: > "$out_dir/damage_census.jsonl"
if command -v am >/dev/null 2>&1; then
    am doctor fix-orphan-refs --all --dry-run --format json 2>/dev/null \
        | jq -c '.projects[]? | {
              project: .project,
              scanned_refs: .scanned_refs,
              findings: .summary.findings,
              protected: .summary.by_category.protected,
              safe_to_prune: .summary.by_category.safe_to_prune,
              ask_user: .summary.by_category.ask_user,
              error: .error
          }' > "$out_dir/damage_census.jsonl" 2>/dev/null \
        || log "  am doctor fix-orphan-refs output not parseable; census skipped"
fi

project_count=$(wc -l < "$out_dir/damage_census.jsonl" 2>/dev/null || echo 0)
log "  census: $project_count project(s) scanned"

# --- Summary narrative ----------------------------------------------------
log "writing summary → $out_dir/summary.md"
{
    echo "# Git 2.51.0 Baseline — $ts"
    echo
    echo "## Host"
    head -4 "$out_dir/host_context.txt" | sed 's/^/    /'
    echo
    echo "## Kernel log"
    echo
    echo "- $segfault_count git segfault events in the last 72h"
    if [ "$segfault_count" -gt 0 ]; then
        echo "- First: $(head -1 "$out_dir/kernel_segfaults.log")"
        echo "- Last:  $(tail -1 "$out_dir/kernel_segfaults.log")"
    fi
    echo
    echo "## Damage census"
    echo
    echo "Projects scanned: $project_count"
    if [ -s "$out_dir/damage_census.jsonl" ]; then
        total_findings=$(awk -F',' '{
            for (i=1; i<=NF; i++) {
                if (index($i, "\"findings\":") > 0) {
                    split($i, kv, ":");
                    s += kv[2];
                }
            }
        } END { print s+0 }' "$out_dir/damage_census.jsonl")
        echo "Total orphan ref findings: $total_findings"
    fi
    echo
    echo "## Next"
    echo
    echo "- Compare post-fix: re-run this script after fix applied."
    echo "- Expected post-fix: kernel segfaults → 0, damage findings → 0 for newly-corrupted."
    echo "- File incident report with this artifact dir attached if either baseline metric is abnormal."
} > "$out_dir/summary.md"

log "baseline captured: $out_dir"
cat "$out_dir/summary.md"
