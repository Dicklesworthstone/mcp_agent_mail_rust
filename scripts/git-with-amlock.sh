#!/usr/bin/env bash
#
# scripts/git-with-amlock.sh — br-8ujfs.2.5 (B5)
#
# Opt-in wrapper that lets external tools (NTM, CI scripts, user
# shell aliases) coordinate with mcp-agent-mail's internal per-repo
# `flock` sentinel on `<repo>/.git/am.git-serialize.lock`. This
# prevents the user's own `git commit` from racing against the
# server's in-process git shell-outs (which are the primary
# reproducer for the git 2.51.0 .git/index race).
#
# Usage (alias style):
#   export GIT_BIN="$(command -v git-with-amlock || command -v git)"
#   alias gitam='"$GIT_BIN"'
#   gitam status
#   gitam commit -m 'synced'
#
# Usage (explicit):
#   /path/to/git-with-amlock status
#
# Options:
#   - AM_GIT_BINARY: if set, use that git binary instead of the
#     system default. Same knob the server uses.
#   - AM_GIT_FLOCK_TIMEOUT_SECS: override the 60s flock wait cap.
#
# Exit codes:
#   0..128 — pass-through from git itself
#   75     — EX_TEMPFAIL; flock wait exceeded timeout (another git
#            process appears stuck). Run `am doctor check` to triage.
#
# Safety: if we're NOT inside a git repo (no `.git` directory up-tree),
# we degrade to `exec git "$@"` without any flock. Same applies if
# we can't create the sentinel (read-only .git, permission denied).

set -euo pipefail

# --------------------------------------------------------------------------
# Resolve which git binary to call. Honors AM_GIT_BINARY (bead A5).
# --------------------------------------------------------------------------
git_bin="${AM_GIT_BINARY:-git}"
if ! command -v "$git_bin" >/dev/null 2>&1; then
    echo "am-git-wrapper: cannot find git at '$git_bin' (check AM_GIT_BINARY)" >&2
    exit 127
fi

# --------------------------------------------------------------------------
# Find the repo admin directory (.git dir). `rev-parse` handles normal
# repos, bare repos, linked worktrees, and honors GIT_DIR env.
# --------------------------------------------------------------------------
if ! repo_admin_dir="$("$git_bin" rev-parse --git-dir 2>/dev/null)"; then
    # Not inside a repo (or rev-parse failed). Pass through.
    exec "$git_bin" "$@"
fi

# Normalize the admin dir to an absolute path.
case "$repo_admin_dir" in
    /*) ;;
    *) repo_admin_dir="$PWD/$repo_admin_dir" ;;
esac

sentinel="$repo_admin_dir/am.git-serialize.lock"

# --------------------------------------------------------------------------
# Create the sentinel (idempotent; 0644). If we can't create it,
# degrade gracefully — coordination is opt-in; unavailability isn't
# a blocker.
# --------------------------------------------------------------------------
if ! mkdir -p "$(dirname -- "$sentinel")" 2>/dev/null; then
    exec "$git_bin" "$@"
fi
if ! : > "$sentinel" 2>/dev/null && [ ! -e "$sentinel" ]; then
    # Cannot create the sentinel (read-only .git); degrade.
    exec "$git_bin" "$@"
fi

# --------------------------------------------------------------------------
# flock with a bounded wait. 60s default matches the server-side cap
# in mcp_agent_mail_core::git_lock::DEFAULT_FLOCK_TIMEOUT_SECS.
# --------------------------------------------------------------------------
timeout="${AM_GIT_FLOCK_TIMEOUT_SECS:-60}"
if ! command -v flock >/dev/null 2>&1; then
    # No flock utility (e.g., macOS default). Degrade to pass-through.
    # Users on macOS can install `util-linux` via Homebrew for flock.
    exec "$git_bin" "$@"
fi

if ! flock -x -w "$timeout" "$sentinel" "$git_bin" "$@"; then
    rc=$?
    if [ "$rc" -eq 1 ]; then
        # flock returned 1 on timeout (vs. command exit status).
        echo "am-git-wrapper: flock timeout on $sentinel after ${timeout}s — another git process appears stuck. Run 'am doctor check'." >&2
        exit 75
    fi
    exit "$rc"
fi
