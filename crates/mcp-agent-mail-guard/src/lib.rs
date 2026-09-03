#![forbid(unsafe_code)]

use globset::GlobSetBuilder;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Repository metadata that is shared by every agent and must not be guarded.
///
/// Beads writes task state under `.beads/`; blocking those commits when another
/// agent happens to reserve the directory makes normal swarm coordination
/// impossible. This is intentionally a narrow, built-in exemption rather than
/// a broad guard bypass.
const DEFAULT_EXEMPT_PATH_PREFIXES: &[&str] = &[".beads"];

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("not implemented")]
    NotImplemented,
    #[error("invalid repository path: {path}")]
    InvalidRepo { path: String },
    #[error("invalid reservation pattern '{pattern}': {error}")]
    InvalidReservationPattern { pattern: String, error: String },
    #[error("missing AGENT_NAME env var")]
    MissingAgentName,
    #[error(
        "refusing to install a per-project guard into the machine-wide hooks directory \
         '{hooks_path}' configured by {origin} core.hooksPath: every repository on this \
         machine would run this project's reservation guard. Either set a repo-local hooks \
         path first (`git config --local core.hooksPath <dir>`), or explicitly allow the \
         shared directory with AGENT_MAIL_GUARD_ALLOW_GLOBAL_HOOKSPATH=1."
    )]
    GlobalHooksPath { hooks_path: String, origin: String },
    #[error("git error: {0}")]
    Git(#[from] git2::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type GuardResult<T> = Result<T, GuardError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardMode {
    Block,
    Warn,
}

impl GuardMode {
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var("AGENT_MAIL_GUARD_MODE")
            .unwrap_or_else(|_| "block".to_string())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "warn" => Self::Warn,
            _ => Self::Block,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GuardStatus {
    pub worktrees_enabled: bool,
    pub guard_mode: GuardMode,
    pub hooks_dir: String,
    pub pre_commit_present: bool,
    pub pre_push_present: bool,
}

#[derive(Debug, Clone)]
pub struct GuardConflict {
    pub path: String,
    pub pattern: String,
    pub holder: String,
    pub expires_ts: String,
}

/// A parsed file reservation from the archive JSON files.
#[derive(Debug, Clone)]
pub struct FileReservationRecord {
    pub path_pattern: String,
    pub agent_name: String,
    pub exclusive: bool,
    pub expires_ts: String,
    pub released_ts: Option<String>,

    // Cached for optimization
    pub normalized_pattern: String,
    pub has_glob: bool,
}

/// Result from a full guard check run.
#[derive(Debug)]
pub struct GuardCheckResult {
    pub conflicts: Vec<GuardConflict>,
    pub mode: GuardMode,
    pub bypassed: bool,
    pub gated: bool,
}

fn home_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HOME")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }

    // Windows fallbacks (best-effort; tests run on Linux, but keep portable).
    if let Some(p) = std::env::var_os("USERPROFILE")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }

    let drive = std::env::var_os("HOMEDRIVE");
    let path = std::env::var_os("HOMEPATH");
    match (drive, path) {
        (Some(d), Some(p)) if !d.is_empty() && !p.is_empty() => Some(PathBuf::from(d).join(p)),
        _ => None,
    }
}

fn expand_user(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn resolve_common_git_dir(repo: &git2::Repository) -> GuardResult<PathBuf> {
    // For worktrees, repo.path() points at .git/worktrees/<name>/.
    // The commondir file contains a relative path back to the common .git directory.
    let gitdir = repo.path();
    let commondir_path = gitdir.join("commondir");
    if commondir_path.is_file() {
        let rel = std::fs::read_to_string(commondir_path)?;
        let rel = rel.trim();
        if rel.is_empty() {
            return Ok(gitdir.to_path_buf());
        }
        let candidate = gitdir.join(rel);
        // canonicalize is nice-to-have; keep best-effort to avoid surprising errors.
        return Ok(candidate.canonicalize().unwrap_or(candidate));
    }

    Ok(gitdir.to_path_buf())
}

/// Resolve the git hooks directory for a repository, honoring `core.hooksPath`.
///
/// This is intentionally compatible with legacy semantics:
/// - Absolute `core.hooksPath` wins.
/// - Relative `core.hooksPath` is resolved against repo workdir (toplevel).
/// - Otherwise, use the common git dir's `hooks/` (handles worktrees).
pub fn resolve_hooks_dir(repo_path: &Path) -> GuardResult<PathBuf> {
    if !repo_path.exists() {
        return Err(GuardError::InvalidRepo {
            path: repo_path.display().to_string(),
        });
    }

    let repo = git2::Repository::discover(repo_path)?;
    if repo.is_bare() || repo.workdir().is_none() {
        return Err(GuardError::InvalidRepo {
            path: repo_path.display().to_string(),
        });
    }

    let config = repo.config()?;
    if let Ok(raw) = config.get_string("core.hooksPath") {
        let raw = raw.trim();
        if !raw.is_empty() {
            let expanded = expand_user(raw);
            if expanded.is_absolute() {
                return Ok(expanded);
            }

            let root = repo.workdir().unwrap_or(repo_path).to_path_buf();
            return Ok(root.join(expanded));
        }
    }

    let common_git_dir = resolve_common_git_dir(&repo)?;
    Ok(common_git_dir.join("hooks"))
}

fn env_allows_global_hookspath() -> bool {
    std::env::var("AGENT_MAIL_GUARD_ALLOW_GLOBAL_HOOKSPATH").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "t" | "yes" | "y"
        )
    })
}

/// Whether an install into a `core.hooksPath` configured at `level` must be
/// refused. Repo-scoped levels (local/worktree/app) are always honored;
/// machine-wide levels (global/system/XDG) are refused unless explicitly
/// allowed (GH#223).
const fn hookspath_level_refuses_install(level: git2::ConfigLevel, allow_global: bool) -> bool {
    let repo_scoped = matches!(
        level,
        git2::ConfigLevel::Local | git2::ConfigLevel::Worktree | git2::ConfigLevel::App
    );
    !repo_scoped && !allow_global
}

const fn config_level_label(level: git2::ConfigLevel) -> &'static str {
    match level {
        git2::ConfigLevel::System => "system",
        git2::ConfigLevel::XDG => "XDG",
        git2::ConfigLevel::Global => "global",
        git2::ConfigLevel::Local => "local",
        git2::ConfigLevel::Worktree => "worktree",
        git2::ConfigLevel::App => "app",
        _ => "inherited",
    }
}

/// Resolve the hooks directory to *install into*.
///
/// GH#223: unlike [`resolve_hooks_dir`] (which mirrors git's runtime lookup
/// and is the right resolver for `uninstall`/`status`, so existing installs
/// can always be found), installation must not silently honor a
/// `core.hooksPath` inherited from global/system git config. Such a directory
/// is used by *every repository on the machine*, so installing the per-project
/// guard chain-runner there displaces the user's global hooks and turns a
/// project-scoped reservation guard into a machine-wide gate.
///
/// A repo-scoped `core.hooksPath` (local/worktree config, e.g. husky-style
/// setups) is honored as before. A global/system value is refused with an
/// actionable error unless `AGENT_MAIL_GUARD_ALLOW_GLOBAL_HOOKSPATH=1` is set.
pub fn resolve_install_hooks_dir(repo_path: &Path) -> GuardResult<PathBuf> {
    if !repo_path.exists() {
        return Err(GuardError::InvalidRepo {
            path: repo_path.display().to_string(),
        });
    }

    let repo = git2::Repository::discover(repo_path)?;
    if repo.is_bare() || repo.workdir().is_none() {
        return Err(GuardError::InvalidRepo {
            path: repo_path.display().to_string(),
        });
    }

    let config = repo.config()?;
    if let Ok(entry) = config.get_entry("core.hookspath") {
        let raw = entry.value().unwrap_or("").trim().to_string();
        if !raw.is_empty() {
            let level = entry.level();
            if hookspath_level_refuses_install(level, env_allows_global_hookspath()) {
                return Err(GuardError::GlobalHooksPath {
                    hooks_path: raw,
                    origin: config_level_label(level).to_string(),
                });
            }
            drop(entry);
            let expanded = expand_user(&raw);
            if expanded.is_absolute() {
                return Ok(expanded);
            }
            let root = repo.workdir().unwrap_or(repo_path).to_path_buf();
            return Ok(root.join(expanded));
        }
    }

    let common_git_dir = resolve_common_git_dir(&repo)?;
    Ok(common_git_dir.join("hooks"))
}

const PLUGIN_FILE_NAME: &str = "50-agent-mail.py";

/// Write a git-hook artifact atomically and symlink-safely.
///
/// Hardens `install_guard` against two real hazards:
/// 1. **Symlink-planted destination.** A blind `std::fs::write` follows a
///    symlink, so a pre-planted `50-agent-mail.py -> ~/.bashrc` (or any hook
///    path) would be written *through* the link — an arbitrary-file clobber,
///    and the subsequent path-based `chmod` would force the exec bit on the
///    target. We remove any symlink at the destination first (never write
///    through it) and apply the mode to the freshly-created fd (fchmod), never
///    via a path that a symlink could redirect.
/// 2. **Torn / fail-open hook.** `std::fs::write` truncates in place and is not
///    atomic; a crash or disk-full mid-write leaves a partially written hook
///    that can parse as an effective no-op and silently bypass the reservation
///    guard on every later commit. We write to a temp file in the same
///    directory and atomically rename over the destination.
fn write_guard_file_atomic(path: &Path, contents: &str, executable: bool) -> GuardResult<()> {
    use std::io::Write as _;

    let parent = path.parent().ok_or_else(|| {
        GuardError::Io(std::io::Error::other("hook path has no parent directory"))
    })?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| GuardError::Io(std::io::Error::other("hook path has no file name")))?;

    // Never write through a symlink planted at the destination.
    if let Ok(meta) = path.symlink_metadata()
        && meta.file_type().is_symlink()
    {
        std::fs::remove_file(path)?;
    }

    let tmp_path = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    // `create_new` refuses to follow a symlink at the temp path.
    let mut tmp = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)?;

    let staged = (|| -> std::io::Result<()> {
        tmp.write_all(contents.as_bytes())?;
        #[cfg(unix)]
        if executable {
            use std::os::unix::fs::PermissionsExt;
            // fchmod on the fd we just created — symlink-safe by construction.
            tmp.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        }
        tmp.sync_all()?;
        Ok(())
    })();
    #[cfg(not(unix))]
    let _ = executable;

    drop(tmp);

    if let Err(e) = staged.and_then(|()| std::fs::rename(&tmp_path, path)) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(GuardError::Io(e));
    }
    Ok(())
}

fn is_legacy_single_file_guard(contents: &str) -> bool {
    // Legacy (pre-chain-runner) guard installs used a single hook file.
    // Keep this detection permissive and sentinel-based.
    contents.contains("mcp-agent-mail guard hook")
        || contents.contains("AGENT_NAME environment variable is required.")
}

fn render_chain_runner_script(hook_name: &str) -> String {
    // Run hooks.d/<hook>/* in lexical order and preserve the first failure
    // without skipping later guards. Pre-push children all receive the exact
    // same ref-update bytes.
    let mut lines: Vec<String> = vec![
        "#!/usr/bin/env python3".to_string(),
        format!("# mcp-agent-mail chain-runner ({hook_name})"),
        "import os".to_string(),
        "import sys".to_string(),
        "import stat".to_string(),
        "import subprocess".to_string(),
        "from pathlib import Path".to_string(),
        String::new(),
        "HOOK_DIR = Path(__file__).parent".to_string(),
        format!("RUN_DIR = HOOK_DIR / 'hooks.d' / '{hook_name}'"),
        format!("ORIG = HOOK_DIR / '{hook_name}.orig'"),
        String::new(),
        "def _is_exec(p: Path) -> bool:".to_string(),
        "    try:".to_string(),
        "        st = p.stat()".to_string(),
        "        return bool(st.st_mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH))"
            .to_string(),
        "    except Exception:".to_string(),
        "        return False".to_string(),
        String::new(),
        "def _list_execs() -> list[Path]:".to_string(),
        "    if not RUN_DIR.exists() or not RUN_DIR.is_dir():".to_string(),
        "        return []".to_string(),
        "    items = sorted([p for p in RUN_DIR.iterdir() if p.is_file()], key=lambda p: p.name)"
            .to_string(),
        "    # On POSIX, honor exec bit; on Windows, include all files (we'll dispatch .py via python)."
            .to_string(),
        "    if os.name == 'posix':".to_string(),
        "        try:".to_string(),
        "            items = [p for p in items if _is_exec(p)]".to_string(),
        "        except Exception:".to_string(),
        "            pass".to_string(),
        "    return items".to_string(),
        String::new(),
        "def _run_child(path: Path, * , stdin_bytes=None):".to_string(),
        "    # On Windows, prefer 'python' for .py plugins to avoid PATHEXT reliance.".to_string(),
        "    try:".to_string(),
        "        if os.name != 'posix' and path.suffix.lower() == '.py':".to_string(),
        "            return subprocess.run([sys.executable, str(path)], input=stdin_bytes, check=False).returncode"
            .to_string(),
        "        return subprocess.run([str(path)], input=stdin_bytes, check=False).returncode"
            .to_string(),
        "    except OSError as exc:".to_string(),
        "        print(f'mcp-agent-mail chain-runner: could not execute {path.name}: {exc}', file=sys.stderr)"
            .to_string(),
        "        return 126".to_string(),
        String::new(),
        "def _remember_failure(path: Path, rc: int, first_failure: int) -> int:".to_string(),
        "    if rc == 0:".to_string(),
        "        return first_failure".to_string(),
        "    print(f'mcp-agent-mail chain-runner: {path.name} exited with status {rc}', file=sys.stderr)"
            .to_string(),
        "    return first_failure or rc".to_string(),
        String::new(),
    ];

    if hook_name == "pre-push" {
        lines.extend([
            "# Read STDIN once (Git passes ref tuples); forward to children".to_string(),
            "stdin_bytes = sys.stdin.buffer.read()".to_string(),
            "first_failure = 0".to_string(),
            "for exe in _list_execs():".to_string(),
            "    rc = _run_child(exe, stdin_bytes=stdin_bytes)".to_string(),
            "    first_failure = _remember_failure(exe, rc, first_failure)".to_string(),
            String::new(),
            "if ORIG.exists():".to_string(),
            "    rc = _run_child(ORIG, stdin_bytes=stdin_bytes)".to_string(),
            "    first_failure = _remember_failure(ORIG, rc, first_failure)".to_string(),
            "sys.exit(first_failure)".to_string(),
        ]);
    } else {
        lines.extend([
            "first_failure = 0".to_string(),
            "for exe in _list_execs():".to_string(),
            "    rc = _run_child(exe)".to_string(),
            "    first_failure = _remember_failure(exe, rc, first_failure)".to_string(),
            String::new(),
            "if ORIG.exists():".to_string(),
            "    rc = _run_child(ORIG)".to_string(),
            "    first_failure = _remember_failure(ORIG, rc, first_failure)".to_string(),
            "sys.exit(first_failure)".to_string(),
        ]);
    }

    format!("{}\n", lines.join("\n"))
}

// One template per hook is more readable than a dozen concatenated fragments.
#[allow(clippy::too_many_lines)]
fn render_guard_plugin_script(project: &str, hook_name: &str) -> String {
    // Real guard plugin: checks active file reservations against staged changes (pre-commit)
    // or pushed commits (pre-push).
    let project_json = serde_json::to_string(project).unwrap_or_else(|_| "\"\"".to_string());
    let hook_name_json = serde_json::to_string(hook_name).unwrap_or_else(|_| "\"\"".to_string());
    let template = r##"#!/usr/bin/env python3
# mcp-agent-mail guard plugin (__HOOK_NAME_TEXT__)
# project: __PROJECT_TEXT__
# Auto-generated by mcp-agent-mail install_guard

import datetime
import hashlib
import json
import os
import re
import subprocess
import sys

PROJECT = __PROJECT_JSON__
HOOK_NAME = __HOOK_NAME_JSON__
AGENT_NAME = os.environ.get("AGENT_NAME", "").strip()
GUARD_MODE = os.environ.get("AGENT_MAIL_GUARD_MODE", "block")

def fail_closed(message):
    """Fail-closed exit for guard infrastructure failures (GH#224).

    Every fail-closed path must (a) honor AGENT_MAIL_GUARD_MODE=warn, which
    the guard's own block message advertises, and (b) name the
    AGENT_MAIL_BYPASS escape hatch, so an operator is never stuck behind an
    exit 2 with no advertised way out.
    """
    if GUARD_MODE == "warn":
        print(
            "WARNING: " + message + " (AGENT_MAIL_GUARD_MODE=warn: allowing)",
            file=sys.stderr,
        )
        sys.exit(0)
    print("ERROR: " + message, file=sys.stderr)
    print(
        "Set AGENT_MAIL_GUARD_MODE=warn to continue with a warning, or "
        "AGENT_MAIL_BYPASS=1 to skip this guard entirely.",
        file=sys.stderr,
    )
    sys.exit(2)

# br-8ujfs.5.5 (E5) — Python-side SIGSEGV retry for git 2.51.0 index race.
# Matches the Rust retry policy (3 retries, 100/400/1600ms jittered).
# Only retries on segfault-shaped exits (139 = 128+11, 135 = 128+7,
# negative -11/-7 on POSIX); all other nonzero returncodes propagate.
import random as _random
import time as _time

def _run_git_with_retry(args, **kwargs):
    """subprocess.run wrapper that retries on SIGSEGV (git 2.51.0).

    `args` is the full argv list (typically ["git", ...]).
    Honors `AM_GIT_BINARY` if set, replacing argv[0]=="git".
    kwargs are forwarded to subprocess.run, but note that `check=True`
    is handled by this wrapper — we capture check_requested, force
    check=False internally so the retry loop can inspect returncode
    on segfault-shaped exits, and re-raise CalledProcessError ourselves
    ONLY on the final non-retryable failure. Without this, callers
    that pass check=True would see the first segfault as an exception
    and bypass the retry entirely.
    """
    env_bin = os.environ.get("AM_GIT_BINARY")
    if env_bin and args and args[0] == "git":
        args = [env_bin] + list(args[1:])

    check_requested = kwargs.pop("check", False)

    delays = (0.1, 0.4, 1.6)
    max_retries = 3
    last_result = None
    for attempt in range(max_retries + 1):
        last_result = subprocess.run(args, check=False, **kwargs)
        rc = last_result.returncode
        # segfault-like exits:
        #   -11 / -7 on POSIX (signal), 139 / 135 via shell wrapping,
        #   0xC0000005 on Windows.
        is_segfault = rc in (-11, -7, 139, 135, 0xC0000005)
        if not is_segfault:
            # Non-segfault outcome (success OR other error).
            # If caller requested check=True and we saw a nonzero
            # non-segfault exit, emulate subprocess.run's behavior
            # by raising CalledProcessError.
            if check_requested and rc != 0:
                raise subprocess.CalledProcessError(
                    rc,
                    args,
                    output=getattr(last_result, "stdout", None),
                    stderr=getattr(last_result, "stderr", None),
                )
            return last_result
        if attempt == max_retries:
            sys.stderr.write(
                "guard: git exited with signal %d %d times (known-bad 2.51.0). "
                "Set AM_GIT_BINARY or upgrade git.\n" % (rc, attempt + 1)
            )
            if check_requested:
                raise subprocess.CalledProcessError(
                    rc,
                    args,
                    output=getattr(last_result, "stdout", None),
                    stderr=getattr(last_result, "stderr", None),
                )
            return last_result
        jitter = _random.uniform(0.75, 1.25)
        sleep_s = delays[attempt] * jitter
        sys.stderr.write(
            "guard: git segfault (rc=%d, attempt %d/%d); retrying in %.2fs\n"
            % (rc, attempt + 1, max_retries + 1, sleep_s)
        )
        _time.sleep(sleep_s)
    return last_result


def get_staged_files():
    """Get list of staged files from git (for pre-commit)."""
    try:
        result = _run_git_with_retry(
            ["git", "diff", "--cached", "--name-status", "-M", "-z", "--diff-filter=ACMRDTU"],
            capture_output=True, check=True,
        )
        data = result.stdout or b""
        if not data:
            return []
        parts = data.split(b"\0")
        files = []
        i = 0
        while i < len(parts):
            if not parts[i]:
                break
            status = parts[i].decode("utf-8", "ignore")
            i += 1
            if status.startswith(("R", "C")):
                # Rename/Copy: next two entries are old and new path.
                if i + 1 >= len(parts):
                    break
                oldp = parts[i].decode("utf-8", "ignore")
                newp = parts[i + 1].decode("utf-8", "ignore")
                i += 2
                if oldp:
                    files.append(oldp)
                if newp:
                    files.append(newp)
            else:
                # Normal entry: next is the path.
                if i >= len(parts):
                    break
                p = parts[i].decode("utf-8", "ignore")
                i += 1
                if p:
                    files.append(p)
        # De-duplicate while preserving order.
        seen = set()
        out = []
        for f in files:
            if f not in seen:
                seen.add(f)
                out.append(f)
        return out
    except subprocess.CalledProcessError as exc:
        fail_closed("mcp-agent-mail: guard failed to inspect staged files: " + str(exc))
    except Exception as exc:
        fail_closed("mcp-agent-mail: guard failed to inspect staged files: " + str(exc))

def get_push_files():
    """Get list of files modified in the push (for pre-push)."""
    files = set()
    try:
        # Read stdin for ref updates (local_ref local_sha remote_ref remote_sha)
        # sys.stdin.read() works because chain-runner pipes input as text/bytes depending on OS,
        # but in Python 3 sys.stdin is a text wrapper. The chain-runner sends raw bytes,
        # but standard python environment usually handles this.
        # Safe fallback is sys.stdin.read().
        stdin_data = sys.stdin.read()
        if not stdin_data:
            return []

        for line in stdin_data.splitlines():
            parts = line.split()
            if len(parts) < 4:
                continue
            local_sha = parts[1]
            remote_sha = parts[3]

            # Skip deletes
            if set(local_sha) == {'0'}:
                continue

            if set(remote_sha) == {'0'}:
                rev_list_args = ["git", "rev-list", "--topo-order", local_sha, "--not", "--remotes"]
            else:
                rev_list_args = ["git", "rev-list", "--topo-order", f"{remote_sha}..{local_sha}"]

            # Get commits in range
            res = _run_git_with_retry(
                rev_list_args,
                capture_output=True, text=True
            )
            if res.returncode != 0:
                detail = (res.stderr or "").strip()
                if not detail:
                    detail = f"git rev-list exited with status {res.returncode}"
                fail_closed(
                    "mcp-agent-mail: guard failed to enumerate pushed commits: " + detail
                )

            commits = [c.strip() for c in res.stdout.splitlines() if c.strip()]

            for sha in commits:
                diff_res = _run_git_with_retry(
                    # `--cc` (not `-m`): on a merge commit `-m` explodes the diff
                    # into one section PER PARENT, flagging every file merely
                    # carried in from origin as a "pushed change" (false positive,
                    # issue #238). `--cc` reports only the files the merge itself
                    # changed relative to ALL parents, preserving the fail-closed
                    # check for real conflict-resolution edits while dropping
                    # carried files. On a regular (single-parent) commit `--cc`
                    # and `-m` produce identical --name-status output (no FN).
                    ["git", "diff-tree", "--root", "-r", "--no-commit-id", "--name-status",
                     "-M", "--no-ext-diff", "--diff-filter=ACMRDTU", "-z", "--cc", sha],
                    capture_output=True
                )
                if diff_res.returncode != 0:
                    detail = diff_res.stderr.decode("utf-8", "ignore").strip()
                    if not detail:
                        detail = f"git diff-tree exited with status {diff_res.returncode}"
                    fail_closed(
                        "mcp-agent-mail: guard failed to inspect pushed commit paths: " + detail
                    )
                data = diff_res.stdout
                parts = data.split(b'\0')
                i = 0
                while i < len(parts):
                    status = parts[i].decode('utf-8', 'ignore').strip()
                    if not status:
                        i += 1
                        continue
                    i += 1
                    if status.startswith(('R', 'C')):
                        if i + 1 < len(parts):
                            oldp = parts[i].decode('utf-8', 'ignore')
                            newp = parts[i+1].decode('utf-8', 'ignore')
                            if oldp: files.add(oldp)
                            if newp: files.add(newp)
                            i += 2
                    else:
                        if i < len(parts):
                            p = parts[i].decode('utf-8', 'ignore')
                            if p: files.add(p)
                            i += 1
    except SystemExit:
        raise
    except Exception as exc:
        fail_closed("mcp-agent-mail: guard failed to inspect push files: " + str(exc))
    return sorted(list(files))

def is_real_directory(path):
    try:
        return os.path.isdir(path) and not os.path.islink(path)
    except OSError:
        return False

def is_real_file(path):
    try:
        return os.path.isfile(path) and not os.path.islink(path)
    except OSError:
        return False

def slugify(value):
    out = []
    prev_dash = False
    for ch in value.strip().lower():
        # Match Rust's is_ascii_alphanumeric() behavior
        if (ord(ch) >= 97 and ord(ch) <= 122) or (ord(ch) >= 48 and ord(ch) <= 57):
            out.append(ch)
            prev_dash = False
        elif not prev_dash:
            out.append("-")
            prev_dash = True
    slug = "".join(out).strip("-")
    return slug or "project"

def default_storage_root():
    for key in ("STORAGE_ROOT", "AGENT_MAIL_STORAGE_ROOT"):
        value = os.environ.get(key, "").strip()
        if value:
            return os.path.expanduser(value)
    
    # Match Rust core's default_storage_root_path logic.
    # Only honor the legacy path if it actually *contains* an archive —
    # an empty-directory stub at ~/.mcp_agent_mail_git_mailbox_repo (left
    # over from a prior install, or created accidentally) used to win
    # over the real XDG archive and make every commit fail with
    # "guard could not locate archive for project '...'" (#95).
    legacy = os.path.expanduser("~/.mcp_agent_mail_git_mailbox_repo")
    if is_real_directory(os.path.join(legacy, "projects")):
        return legacy
        
    # XDG Data path fallback
    xdg_data = os.environ.get("XDG_DATA_HOME")
    if xdg_data:
        root = os.path.join(xdg_data, "mcp-agent-mail", "git_mailbox_repo")
    else:
        root = os.path.expanduser("~/.local/share/mcp-agent-mail/git_mailbox_repo")
    return root

def get_repo_root():
    try:
        result = _run_git_with_retry(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            check=True,
        )
        value = (result.stdout or "").strip()
        return value or None
    except Exception:
        return None

def canonical_text(value):
    if not value:
        return ""
    try:
        return os.path.realpath(value)
    except OSError:
        return os.path.abspath(value)

def sanitize_pane_id(pane_id):
    """Match core's pane-identity filename normalization."""
    stripped = (pane_id or "").strip()
    if stripped.startswith("%"):
        stripped = stripped[1:]
    out = []
    for ch in stripped:
        if ch.isascii() and (ch.isalnum() or ch in ("-", "_")):
            out.append(ch)
        elif ch == ":":
            out.append("-")
        else:
            out.append("_")
    return "".join(out) or "unknown"

def read_identity_file(path):
    """Read a regular, non-symlink identity file if it carries a name."""
    try:
        parent = os.path.dirname(os.path.abspath(path))
        while parent and parent != os.path.dirname(parent):
            if os.path.islink(parent):
                return None
            parent = os.path.dirname(parent)
        if not os.path.isfile(path) or os.path.islink(path):
            return None
        with open(path, "r", encoding="utf-8") as handle:
            name = handle.read().strip()
        return name or None
    except OSError:
        return None

def current_pane_ids():
    """Return the caller's composite tmux pane id, then its bare fallback."""
    pane = os.environ.get("TMUX_PANE", "").strip()
    if not pane:
        return []
    pane_ids = []
    try:
        result = subprocess.run(
            ["tmux", "display-message", "-t", pane, "-p", "#{session_name}:#{window_index}:#{pane_index}"],
            capture_output=True,
            text=True,
            check=False,
        )
        composite = (result.stdout or "").strip()
        if result.returncode == 0 and composite and ":" in composite:
            pane_ids.append(composite)
    except OSError:
        pass
    if pane not in pane_ids:
        pane_ids.append(pane)
    return pane_ids

def resolve_identity_agent_name():
    """Resolve this hook's identity from the canonical pane-identity files."""
    pane_ids = current_pane_ids()
    if not pane_ids:
        return ""

    project_key = canonical_text(get_repo_root() or PROJECT)
    project_hash = hashlib.sha1(project_key.encode("utf-8")).hexdigest()[:12]
    config_base = os.environ.get("XDG_CONFIG_HOME", "").strip()
    if not config_base:
        config_base = os.path.join(os.path.expanduser("~"), ".config")

    for pane_id in pane_ids:
        sanitized = sanitize_pane_id(pane_id)
        candidates = [
            os.path.join(config_base, "agent-mail", "identity", project_hash, sanitized),
            os.path.join(os.path.expanduser("~"), ".claude", "agent-mail", f"identity.{sanitized}"),
            os.path.join("/tmp", f"agent-mail-name.{project_hash}.{sanitized}"),
        ]
        for candidate in candidates:
            if name := read_identity_file(candidate):
                return name
    return ""

def looks_like_project_slug(value):
    value = (value or "").strip()
    return bool(value) and not os.path.isabs(value) and "/" not in value and "\\" not in value

def project_metadata_matches(
    metadata,
    project_value,
    project_slug,
    repo_root,
    repo_slug,
    canonical_project,
    canonical_repo,
):
    if not isinstance(metadata, dict):
        return False

    slug = str(metadata.get("slug", "")).strip()
    project_value_is_slug = looks_like_project_slug(project_value)
    repo_root_is_slug = looks_like_project_slug(repo_root)
    if slug:
        if project_value_is_slug and slug in {project_value, project_slug}:
            return True
        if repo_root_is_slug and slug in {repo_root, repo_slug}:
            return True

    human_key = str(metadata.get("human_key", "")).strip()
    if human_key and human_key in {project_value, repo_root}:
        return True

    canonical_human_key = canonical_text(human_key)
    return bool(canonical_human_key and canonical_human_key in {canonical_project, canonical_repo})

def probe_real_directory(path):
    """(is_dir, error) -- inspection errors are distinct from a missing path.

    `os.path.isdir` coerces permission errors into False, which upstream made
    indistinguishable from "no matching project" and therefore fail-OPEN
    (GH#228). Absent paths return (False, None); paths that cannot even be
    inspected return (False, "<detail>") so the caller can route the errored
    outcome through fail_closed.
    """
    try:
        if os.path.islink(path):
            return False, None
        os.stat(path)
    except (FileNotFoundError, NotADirectoryError):
        return False, None
    except OSError as exc:
        return False, "%s: %s" % (path, exc)
    try:
        return os.path.isdir(path), None
    except OSError as exc:
        return False, "%s: %s" % (path, exc)

def resolve_archive_root():
    """Locate this project's archive root.

    Returns (archive_root, suspicious_entry, errors):
    - (path, None, [...]) when a matching archive is found;
    - (None, name, [...]) when an archive whose directory name collides with
      this project's slug exists but provably belongs to a different project
      (slug collision -- the guard must stay fail-closed, GH#224 note);
    - (None, None, []) when nothing in the storage root relates to this repo
      at all (nothing to guard -- callers may fail open);
    - (None, *, [error, ...]) when resolution hit inspection errors
      (permissions, IO) that make "no match" unprovable -- callers must route
      this through fail_closed rather than allowing (GH#228).
    """
    errors = []
    repo_root = get_repo_root()
    if repo_root:
        local_reservations, err = probe_real_directory(
            os.path.join(repo_root, "file_reservations")
        )
        if local_reservations:
            return repo_root, None, errors
        if err:
            errors.append(err)

    storage_root = default_storage_root()
    projects_dir = os.path.join(storage_root, "projects")
    projects_dir_ok, err = probe_real_directory(projects_dir)
    if err:
        errors.append(err)
        return None, None, errors
    if not projects_dir_ok:
        return None, None, errors

    project_value = PROJECT.strip()
    project_slug = slugify(project_value) if project_value else ""
    repo_slug = slugify(repo_root) if repo_root else ""
    project_value_is_slug = looks_like_project_slug(project_value)
    repo_root_is_slug = looks_like_project_slug(repo_root)
    explicit_names = []
    for name in (
        project_value if project_value_is_slug else "",
        project_slug if project_value_is_slug else "",
        repo_root if repo_root_is_slug else "",
        repo_slug if repo_root_is_slug else "",
    ):
        if (
            name
            and not os.path.isabs(name)
            and "/" not in name
            and "\\" not in name
            and name not in explicit_names
        ):
            explicit_names.append(name)

    for name in explicit_names:
        candidate = os.path.join(projects_dir, name)
        reservations_ok, err = probe_real_directory(
            os.path.join(candidate, "file_reservations")
        )
        if reservations_ok:
            return candidate, None, errors
        if err:
            errors.append(err)

    canonical_project = canonical_text(project_value)
    canonical_repo = canonical_text(repo_root)
    try:
        entries = sorted(os.scandir(projects_dir), key=lambda entry: entry.name)
    except OSError as exc:
        errors.append("%s: %s" % (projects_dir, exc))
        return None, None, errors

    slug_candidates = set(explicit_names)
    if project_slug:
        slug_candidates.add(project_slug)
    if repo_slug:
        slug_candidates.add(repo_slug)

    suspicious = None
    for entry in entries:
        try:
            if not entry.is_dir(follow_symlinks=False):
                continue
        except OSError as exc:
            errors.append("%s: %s" % (entry.path, exc))
            continue

        candidate = entry.path
        reservations_ok, err = probe_real_directory(
            os.path.join(candidate, "file_reservations")
        )
        if err:
            errors.append(err)
            continue
        if not reservations_ok:
            continue
        if entry.name in explicit_names:
            return candidate, None, errors

        metadata_path = os.path.join(candidate, "project.json")
        if not is_real_file(metadata_path):
            if suspicious is None and entry.name in slug_candidates:
                suspicious = entry.name
            continue
        try:
            with open(metadata_path, "r", encoding="utf-8") as handle:
                metadata = json.load(handle)
        except OSError as exc:
            # Cannot rule this candidate out as ours: unreadable metadata is
            # an errored outcome, not proof of no-match (GH#228).
            errors.append("%s: %s" % (metadata_path, exc))
            continue
        except Exception:
            if suspicious is None and entry.name in slug_candidates:
                suspicious = entry.name
            continue
        if project_metadata_matches(
            metadata,
            project_value,
            project_slug,
            repo_root or "",
            repo_slug,
            canonical_project,
            canonical_repo,
        ):
            return candidate, None, errors
        if suspicious is None and entry.name in slug_candidates:
            suspicious = entry.name

    return None, suspicious, errors

def released_ts_marks_released(value):
    if value is None:
        return False
    if isinstance(value, (int, float)):
        return value > 0
    if isinstance(value, str):
        trimmed = value.strip()
        lowered = trimmed.lower()
        if lowered in ("", "0", "null", "none"):
            return False
        if all(ch.isdigit() or ch in ".+-" for ch in trimmed):
            try:
                return float(trimmed) > 0
            except ValueError:
                return False
        return True
    return False

def is_expired(value, now):
    if value is None:
        return True
    if isinstance(value, (int, float)):
        return value <= now.timestamp() * 1_000_000
    if isinstance(value, str):
        trimmed = value.strip()
        if not trimmed:
            return True
        if all(ch.isdigit() or ch in ".+-" for ch in trimmed):
            try:
                return float(trimmed) <= now.timestamp() * 1_000_000
            except ValueError:
                return False
        try:
            dt = datetime.datetime.fromisoformat(trimmed.replace("Z", "+00:00"))
            if dt.tzinfo is None:
                dt = dt.replace(tzinfo=datetime.timezone.utc)
            return dt <= now
        except Exception:
            return False
    return False

def get_active_reservations():
    """Read active file reservations directly from the archive."""
    archive_root, suspicious, errors = resolve_archive_root()
    if not archive_root:
        if errors:
            # Resolution ERRORED -- "no matching project" is unprovable
            # (permissions, IO). Coercing this into the no-match allow path
            # turned a chmod-000 storage root into a silent bypass of an
            # active exclusive reservation (GH#228). fail_closed honors
            # AGENT_MAIL_GUARD_MODE=warn and advertises AGENT_MAIL_BYPASS=1.
            fail_closed(
                "mcp-agent-mail: guard could not resolve the archive for project "
                "%r; resolution errors: %s" % (PROJECT, "; ".join(errors[:5]))
            )
        if suspicious:
            # An archive directory named like this project's slug exists but
            # provably belongs to a different project (slug collision).
            # A conflicting slug proves the archive is unavailable for this
            # repository, but it does not prove that a reservation applies.
            # Do not strand ordinary commits behind stale/missing mailbox
            # state; make the degraded protection explicit instead.
            print(
                f"WARNING: mcp-agent-mail: guard could not locate archive for project "
                f"{PROJECT!r} (archive {suspicious!r} exists but belongs to a "
                "different project -- slug collision); allowing",
                file=sys.stderr,
            )
            return []
        # This repo matches no agent-mail project archive, so there are no
        # reservations that can be evaluated. A guard must not turn missing
        # mailbox state into a universal commit gate.
        print(
            f"WARNING: mcp-agent-mail: no agent-mail archive matches project {PROJECT!r}; "
            "nothing to guard, allowing",
            file=sys.stderr,
        )
        return []

    reservations_dir = os.path.join(archive_root, "file_reservations")
    if not is_real_directory(reservations_dir):
        return []

    now = datetime.datetime.now(datetime.timezone.utc)
    active = []
    try:
        entries = sorted(os.scandir(reservations_dir), key=lambda entry: entry.name)
    except OSError as exc:
        # Real reservation data exists but cannot be read: stay fail-closed,
        # but through the GH#224 path that honors warn mode + names bypass.
        fail_closed("mcp-agent-mail: guard failed to read reservations: " + str(exc))

    for entry in entries:
        try:
            if not entry.is_file(follow_symlinks=False):
                continue
        except OSError:
            continue
        if not entry.name.endswith(".json"):
            continue
        if entry.is_symlink():
            continue
        try:
            with open(entry.path, "r", encoding="utf-8") as handle:
                record = json.load(handle)
        except Exception:
            continue

        if released_ts_marks_released(record.get("released_ts")):
            continue
        if is_expired(record.get("expires_ts"), now):
            continue

        pattern = str(record.get("path_pattern") or record.get("path") or "").strip()
        holder = str(record.get("agent_name") or record.get("agent") or "").strip()
        if not pattern or not holder or record.get("exclusive") is not True:
            continue

        active.append(
            {
                "path_pattern": pattern,
                "agent_name": holder,
                "expires_ts": record.get("expires_ts"),
            }
        )

    return active

def core_ignorecase_enabled():
    """Detect git core.ignorecase for path comparison parity with Rust guard."""
    try:
        res = _run_git_with_retry(
            ["git", "config", "--bool", "core.ignorecase"],
            capture_output=True,
            text=True,
        )
        if res.returncode == 0:
            value = (res.stdout or "").strip().lower()
            return value in ("1", "true", "yes", "on")
    except Exception:
        pass
    # Windows repositories are usually case-insensitive by default.
    return os.name == "nt"

CASE_INSENSITIVE_REPO = core_ignorecase_enabled()

def normalize_match_input(value):
    # Normalize slashes and trim leading/trailing slashes
    val = value.replace('\\', '/').strip('/')
    # Collapse redundant segments (like Rust core normalization)
    parts = []
    for component in val.split('/'):
        if component == '' or component == '.':
            continue
        if component == '..':
            if parts:
                parts.pop()
        else:
            parts.append(component)
    val = '/'.join(parts)
    return val.lower() if CASE_INSENSITIVE_REPO else val

def is_default_exempt_path(path):
    normalized = normalize_match_input(path)
    return normalized == ".beads" or normalized.startswith(".beads/")

def _glob_translate_body(pattern):
    """Translate a shell glob into a partial regex body (no anchors).

    This deliberately does NOT call ``fnmatch.translate``. That function's
    output is a CPython implementation detail that is not stable across
    releases, and this guard MUST NOT depend on it:

      * <=3.11 terminate the wrapped body with the ``\\Z`` anchor;
      * 3.12/3.13 additionally emit atomic groups like ``(?>.*?/)`` for
        ``*`` runs, and 3.9/3.10 emit named-group backreferences;
      * 3.14 changed the terminating anchor from ``\\Z`` to ``\\z``
        (CPython gh-140922, undocumented).

    The previous implementation string-sliced ``fnmatch.translate`` output
    assuming a trailing ``\\Z``. On Python 3.14 the new ``\\z`` suffix was
    left in place, producing an unbalanced-parenthesis regex that raised
    ``re.error`` at match time -- which ``glob_match`` swallowed and reported
    as "no match". That silently DISABLED every glob (and even literal-path)
    reservation: a fail-OPEN of a security guard. Compiling the glob
    ourselves keeps match decisions identical across Python 3.9-3.14+.

    Metacharacter handling mirrors the Rust reservation matcher:

      ``*``     -> ``.*``  (rewritten to ``[^/]*`` below: one path segment)
      ``**``    -> ``\\0`` sentinel (expanded below to cross directories)
      ``?``     -> ``.``   (any single character)
      ``[..]``  -> character class (``[!`` -> ``[^``; unterminated -> literal)
      ``{a,b}`` -> emitted escaped for the brace-expansion pass below

    Every other character is escaped as a literal via ``re.escape``.
    """
    out = []
    i = 0
    n = len(pattern)
    while i < n:
        c = pattern[i]
        if c == "*":
            j = i
            while j < n and pattern[j] == "*":
                j += 1
            # A run of two or more '*' is a globstar (crosses directories).
            out.append("\0" if (j - i) >= 2 else ".*")
            i = j
            continue
        if c == "?":
            out.append(".")
            i += 1
            continue
        if c == "[":
            k = i + 1
            if k < n and pattern[k] in ("!", "^"):
                k += 1
            if k < n and pattern[k] == "]":
                k += 1
            while k < n and pattern[k] != "]":
                k += 1
            if k >= n:
                # Unterminated class: treat '[' as a literal character.
                out.append("\\[")
                i += 1
                continue
            inner = pattern[i + 1:k]
            if inner.startswith("!"):
                inner = "^" + inner[1:]
            inner = inner.replace("\\", "\\\\")
            out.append("[" + inner + "]")
            i = k + 1
            continue
        if c in "{}":
            out.append("\\" + c)
            i += 1
            continue
        if c == ",":
            out.append(",")
            i += 1
            continue
        out.append(re.escape(c))
        i += 1
    return "".join(out)

def glob_to_regex(pattern):
    """Convert shell-style glob to regex supporting **, [], and {} syntax.

    Version-agnostic: see ``_glob_translate_body`` for why this must not rely
    on ``fnmatch.translate``'s (unstable) output format.
    """
    regex = _glob_translate_body(pattern)
    # Replace single-star .* with [^/]* to respect directory boundaries.
    regex = regex.replace(".*", "[^/]*")
    # Restore ** logic, handling optional slashes.
    regex = regex.replace("/\0/", "(?:/|/.+/)")
    if regex.startswith("\0/"):
        regex = "(?:.+/|)" + regex[2:]
    regex = regex.replace("\0", ".*")
    # Handle {a,b} bash-style brace expansion.
    regex = re.sub(r"\\?\{(.+?)\\?\}", lambda m: "(" + m.group(1).replace("\\", "").replace(",", "|") + ")", regex)
    return regex

def glob_match(path, pattern):
    """Simple shell-style glob matching (similar to Rust implementation)."""
    # NOTE: path must be a concrete path, pattern is the glob.
    normalized_f = normalize_match_input(path)
    normalized_pattern = normalize_match_input(pattern)
    if not normalized_f or not normalized_pattern:
        return False
    try:
        return re.fullmatch(glob_to_regex(normalized_pattern), normalized_f) is not None
    except re.error as exc:
        # FAIL CLOSED. A reservation pattern we cannot compile into a matcher
        # must never be silently treated as "no conflict" -- that is exactly
        # the fail-OPEN the CPython 3.14 fnmatch change caused. Be
        # conservative: report a potential conflict so the commit/push is
        # blocked and the operator can investigate, and make it loud.
        sys.stderr.write(
            "mcp-agent-mail: guard could not evaluate reservation pattern "
            "%r (%s); treating as a conflict (fail-closed).\n" % (pattern, exc)
        )
        return True

def check_conflicts(paths, reservations, self_agent):
    """Check if any paths conflict with active reservations."""
    self_agent = self_agent.lower()
    conflicts = []
    for f in paths:
        if is_default_exempt_path(f):
            continue
        normalized_f = normalize_match_input(f)
        if not normalized_f:
            continue

        for res in reservations:
            pattern = res["path_pattern"]
            holder = res.get("agent_name", "unknown")
            if holder.lower() == self_agent:
                continue  # Skip our own reservations

            normalized_pattern = normalize_match_input(pattern)
            if not normalized_pattern:
                continue

            # 1. Glob matching: check if concrete path matches reserved glob
            if glob_match(normalized_f, normalized_pattern):
                conflicts.append((f, pattern, holder))
                break
            
            # Directory prefix matching
            has_glob = any(c in pattern for c in "*?[{")
            
            # 2. Reverse check: pattern is inside touched path (e.g. dir replaced by file)
            # This handles cases where a concrete parent directory is touched.
            if normalized_pattern.startswith(normalized_f + "/"):
                conflicts.append((f, pattern, holder))
                break

            # 3. Normal prefix check: file is inside reserved dir
            # This handles literal directory reservations.
            if not has_glob and normalized_f.startswith(normalized_pattern + "/"):
                conflicts.append((f, pattern, holder))
                break
    return conflicts

def is_truthy(val):
    if not val:
        return False
    return str(val).strip().lower() in ("1", "true", "t", "yes", "y")

def main():
    # GH#224: the bypass/enforcement toggles must be honored before any
    # fail-closed requirement (previously a missing AGENT_NAME exited 2 even
    # with AGENT_MAIL_BYPASS=1 set).
    if is_truthy(os.environ.get("AGENT_MAIL_BYPASS")):
        sys.exit(0)

    enforcement_enabled = os.environ.get("FILE_RESERVATIONS_ENFORCEMENT_ENABLED")
    if enforcement_enabled is not None and not is_truthy(enforcement_enabled):
        sys.exit(0)

    if HOOK_NAME == "pre-push":
        files_to_check = get_push_files()
    else:
        files_to_check = get_staged_files()

    if not files_to_check:
        sys.exit(0)

    reservations = get_active_reservations()
    if not reservations:
        sys.exit(0)

    # Prefer the explicit environment override, then recover the caller's
    # already-registered pane identity. Requiring AGENT_NAME alone made
    # regular commits fail even though the mailbox already knew the holder.
    agent_name = AGENT_NAME or resolve_identity_agent_name()
    if not agent_name:
        fail_closed(
            "mcp-agent-mail: AGENT_NAME is unset and no current-pane identity "
            "could be resolved to evaluate active file reservations"
        )

    conflicts = check_conflicts(files_to_check, reservations, agent_name)
    if not conflicts:
        sys.exit(0)

    msg = "mcp-agent-mail: file reservation conflict detected!\n"
    for path, pattern, holder in conflicts:
        msg += f"  {path} conflicts with reservation '{pattern}' held by {holder}\n"

    if GUARD_MODE == "warn":
        print(f"WARNING: {msg}", file=sys.stderr)
        sys.exit(0)
    else:
        print(f"ERROR: {msg}", file=sys.stderr)
        print(
            "Set AGENT_MAIL_GUARD_MODE=warn to allow commit anyway, or "
            "AGENT_MAIL_BYPASS=1 to skip this guard.",
            file=sys.stderr,
        )
        sys.exit(1)

if __name__ == "__main__":
    main()
"##;

    template
        .replace("__HOOK_NAME_TEXT__", hook_name)
        .replace("__PROJECT_TEXT__", &project.replace(['\n', '\r'], " "))
        .replace("__PROJECT_JSON__", &project_json)
        .replace("__HOOK_NAME_JSON__", &hook_name_json)
}

pub fn install_guard(project: &str, repo: &Path, install_prepush: bool) -> GuardResult<()> {
    if !repo.exists() {
        return Err(GuardError::InvalidRepo {
            path: repo.display().to_string(),
        });
    }

    // GH#223: install-time resolution refuses machine-wide (global/system)
    // core.hooksPath targets unless explicitly allowed.
    let hooks_dir = resolve_install_hooks_dir(repo)?;
    std::fs::create_dir_all(&hooks_dir)?;

    // Helper to install a single hook type
    let install_hook = |name: &str| -> GuardResult<()> {
        // Ensure hooks.d/<name> exists
        let run_dir = hooks_dir.join("hooks.d").join(name);
        std::fs::create_dir_all(&run_dir)?;

        let chain_path = hooks_dir.join(name);
        if chain_path.exists() {
            let content = std::fs::read_to_string(&chain_path).unwrap_or_default();
            let content = content.trim();
            // Idempotent: backup if not ours
            if !content.contains(&format!("mcp-agent-mail chain-runner ({name})")) {
                let orig = hooks_dir.join(format!("{name}.orig"));
                if !orig.exists() {
                    std::fs::rename(&chain_path, &orig)?;
                }
            }
        }

        // Write chain-runner
        let chain_script = render_chain_runner_script(name);
        write_guard_file_atomic(&chain_path, &chain_script, true)?;

        // Windows shims
        let cmd_path = hooks_dir.join(format!("{name}.cmd"));
        if !cmd_path.exists() {
            let body = format!(
                "@echo off\r\nsetlocal\r\nset \"DIR=%~dp0\"\r\npython \"%DIR%{name}\" %*\r\nexit /b %ERRORLEVEL%\r\n"
            );
            write_guard_file_atomic(&cmd_path, &body, false)?;
        }
        let ps1_path = hooks_dir.join(format!("{name}.ps1"));
        if !ps1_path.exists() {
            let body = format!(
                "$ErrorActionPreference = 'Stop'\n$hook = Join-Path $PSScriptRoot '{name}'\npython $hook @args\nexit $LASTEXITCODE\n"
            );
            write_guard_file_atomic(&ps1_path, &body, false)?;
        }

        // Write guard plugin
        let plugin_path = run_dir.join(PLUGIN_FILE_NAME);
        write_guard_file_atomic(
            &plugin_path,
            &render_guard_plugin_script(project, name),
            true,
        )?;

        Ok(())
    };

    install_hook("pre-commit")?;

    if install_prepush {
        install_hook("pre-push")?;
    }

    Ok(())
}

pub fn uninstall_guard(repo: &Path) -> GuardResult<()> {
    if !repo.exists() {
        return Err(GuardError::InvalidRepo {
            path: repo.display().to_string(),
        });
    }

    let hooks_dir = resolve_hooks_dir(repo)?;

    #[allow(clippy::items_after_statements)]
    fn has_other_plugins(run_dir: &Path) -> bool {
        let Ok(rd) = std::fs::read_dir(run_dir) else {
            return false;
        };
        rd.filter_map(Result::ok).any(|ent| {
            let p = ent.path();
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n != PLUGIN_FILE_NAME)
        })
    }

    // Remove our hooks.d plugins if present.
    for sub in ["pre-commit", "pre-push"] {
        let plugin = hooks_dir.join("hooks.d").join(sub).join(PLUGIN_FILE_NAME);
        if plugin.exists() {
            let _ = std::fs::remove_file(plugin);
        }
    }

    // Legacy top-level single-file uninstall (pre-chain-runner installs)
    // Only remove chain-runner if no other plugins depend on it.
    for hook_name in ["pre-commit", "pre-push"] {
        let hook_path = hooks_dir.join(hook_name);
        if !hook_path.exists() {
            continue;
        }

        let content = std::fs::read_to_string(&hook_path).unwrap_or_default();
        let content = content.trim();

        let is_chain_runner = content.contains("mcp-agent-mail chain-runner");
        let is_legacy_hook = is_legacy_single_file_guard(content);

        if is_chain_runner {
            let run_dir = hooks_dir.join("hooks.d").join(hook_name);
            let orig_path = hooks_dir.join(format!("{hook_name}.orig"));

            if has_other_plugins(&run_dir) {
                continue;
            }

            let _ = std::fs::remove_file(&hook_path);
            if orig_path.exists() {
                std::fs::rename(&orig_path, &hook_path)?;
            }
            let _ = std::fs::remove_file(hooks_dir.join(format!("{hook_name}.cmd")));
            let _ = std::fs::remove_file(hooks_dir.join(format!("{hook_name}.ps1")));
        } else if is_legacy_hook {
            let _ = std::fs::remove_file(&hook_path);
            let _ = std::fs::remove_file(hooks_dir.join(format!("{hook_name}.cmd")));
            let _ = std::fs::remove_file(hooks_dir.join(format!("{hook_name}.ps1")));
        }
    }

    Ok(())
}

/// Check the guard installation status for a repository.
pub fn guard_status(repo: &Path) -> GuardResult<GuardStatus> {
    if !repo.exists() {
        return Err(GuardError::InvalidRepo {
            path: repo.display().to_string(),
        });
    }

    let hooks_dir = resolve_hooks_dir(repo)?;
    let mode = GuardMode::from_env();

    let pre_commit_path = hooks_dir.join("pre-commit");
    let pre_push_path = hooks_dir.join("pre-push");

    let pre_commit_present = pre_commit_path.exists()
        && std::fs::read_to_string(&pre_commit_path).is_ok_and(|c| c.contains("mcp-agent-mail"));

    let pre_push_present = pre_push_path.exists()
        && std::fs::read_to_string(&pre_push_path).is_ok_and(|c| c.contains("mcp-agent-mail"));

    // Check if worktrees are enabled (core.hooksPath set)
    let worktrees_enabled = {
        let git_repo = git2::Repository::discover(repo)?;
        git_repo
            .config()
            .ok()
            .and_then(|c| c.get_string("core.hooksPath").ok())
            .is_some()
    };

    Ok(GuardStatus {
        worktrees_enabled,
        guard_mode: mode,
        hooks_dir: hooks_dir.display().to_string(),
        pre_commit_present,
        pre_push_present,
    })
}

fn is_truthy_value(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "t" | "yes" | "y"
        )
    })
}

fn is_guard_gated_from_values(
    enforcement_enabled: Option<&str>,
    _worktrees_enabled: Option<&str>,
    _git_identity_enabled: Option<&str>,
) -> bool {
    if let Some(val) = enforcement_enabled {
        return is_truthy_value(Some(val));
    }
    // Default to true: the file reservation guard is active unless explicitly
    // disabled via FILE_RESERVATIONS_ENFORCEMENT_ENABLED=false.
    // WORKTREES_ENABLED and GIT_IDENTITY_ENABLED control their own features
    // and must NOT gate the file reservation guard.
    true
}

/// Check if the guard gate is enabled.
///
/// The guard is active if `FILE_RESERVATIONS_ENFORCEMENT_ENABLED` is true (default).
#[must_use]
pub fn is_guard_gated() -> bool {
    is_guard_gated_from_values(
        std::env::var("FILE_RESERVATIONS_ENFORCEMENT_ENABLED")
            .ok()
            .as_deref(),
        std::env::var("WORKTREES_ENABLED").ok().as_deref(),
        std::env::var("GIT_IDENTITY_ENABLED").ok().as_deref(),
    )
}

/// Check if the guard bypass is active (`AGENT_MAIL_BYPASS=1`).
#[must_use]
pub fn is_bypass_active() -> bool {
    is_truthy_value(std::env::var("AGENT_MAIL_BYPASS").ok().as_deref())
}

/// Resolve the committer identity without requiring every shell to export it.
///
/// An explicit `AGENT_NAME` remains authoritative. When it is absent, use the
/// caller's current tmux-pane identity, which is the canonical identity
/// handoff used by Agent Mail's session startup flows.
///
/// GH#252: the pane lookup runs the identity liveness predicate. When the
/// pane's identity file is a binding verifiably live in a *different* pane,
/// the resolver refuses to hand out that name and returns `None`; callers
/// surface that as [`GuardError::MissingAgentName`] (never a panic), the same
/// outcome as an unregistered pane.
fn resolve_guard_agent_name(repo_root: &Path) -> Option<String> {
    if let Ok(explicit) = std::env::var("AGENT_NAME") {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            return Some(explicit.to_string());
        }
    }

    let project_key = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    mcp_agent_mail_core::resolve_identity_current_pane(&project_key.to_string_lossy())
}

/// Full guard check: reads reservations, checks conflicts, respects gate/bypass.
///
/// `archive_root` is the path to the project's agent-mail archive (containing `file_reservations/`).
/// `paths` are the file paths to check (relative to repo root).
///
/// Returns a `GuardCheckResult` with conflicts and mode info.
pub fn guard_check_full(
    archive_root: &Path,
    repo_root: &Path,
    paths: &[String],
) -> GuardResult<GuardCheckResult> {
    let mode = GuardMode::from_env();
    let ignorecase = detect_core_ignorecase(repo_root);

    // Check bypass
    if is_bypass_active() {
        return Ok(GuardCheckResult {
            conflicts: Vec::new(),
            mode,
            bypassed: true,
            gated: false,
        });
    }

    // Check gate (guard only active if enabled)
    if !is_guard_gated() {
        return Ok(GuardCheckResult {
            conflicts: Vec::new(),
            mode,
            bypassed: false,
            gated: true,
        });
    }

    let agent_name = resolve_guard_agent_name(repo_root).ok_or(GuardError::MissingAgentName)?;

    // Read reservations from the archive
    let reservations = read_active_reservations_from_archive(archive_root, ignorecase)?;

    let conflicts = check_path_conflicts(paths, &reservations, &agent_name, ignorecase)?;

    Ok(GuardCheckResult {
        conflicts,
        mode,
        bypassed: false,
        gated: false,
    })
}

/// Check if given paths conflict with active file reservations.
///
/// This is the Rust-native equivalent of the guard plugin's conflict detection.
/// Lower-level API: reads from archive, no gate/bypass handling.
pub fn guard_check(
    archive_root: &Path,
    repo_root: &Path,
    paths: &[String],
    _advisory: bool,
) -> GuardResult<Vec<GuardConflict>> {
    let ignorecase = detect_core_ignorecase(repo_root);
    let agent_name = resolve_guard_agent_name(repo_root).ok_or(GuardError::MissingAgentName)?;

    // Read reservations from archive JSON files
    let reservations = read_active_reservations_from_archive(archive_root, ignorecase)?;

    check_path_conflicts(paths, &reservations, &agent_name, ignorecase)
}

/// Core conflict detection: check paths against reservations using globset.
///
/// Skips reservations held by `self_agent`.
fn check_path_conflicts(
    paths: &[String],
    reservations: &[FileReservationRecord],
    self_agent: &str,
    ignorecase: bool,
) -> GuardResult<Vec<GuardConflict>> {
    // 1. Build a GlobSet for all relevant reservations (other agents, exclusive).
    // Map glob index back to reservation record for conflict reporting.
    let mut builder = GlobSetBuilder::new();
    let mut active_indices: Vec<&FileReservationRecord> = Vec::with_capacity(reservations.len());

    for res in reservations {
        if res.exclusive && !res.agent_name.eq_ignore_ascii_case(self_agent) {
            let mut glob_builder = globset::GlobBuilder::new(&res.normalized_pattern);
            glob_builder.literal_separator(true);
            if ignorecase {
                glob_builder.case_insensitive(true);
            }

            match glob_builder.build() {
                Ok(glob) => {
                    builder.add(glob);
                    active_indices.push(res);
                }
                Err(err) => {
                    eprintln!(
                        "[agent-mail guard] warning: invalid glob pattern '{}' in reservation by {}: {err}",
                        res.normalized_pattern, res.agent_name
                    );
                }
            }
        }
    }

    let glob_set = builder
        .build()
        .map_err(|err| GuardError::InvalidReservationPattern {
            pattern: "<globset>".to_string(),
            error: err.to_string(),
        })?;

    let mut conflicts = Vec::new();

    for path in paths {
        if is_default_exempt_path(path, ignorecase) {
            continue;
        }
        let normalized = normalize_path(path, ignorecase);

        // Check if path matches any reservation pattern
        let matches = glob_set.matches(&normalized);
        if !matches.is_empty() {
            // Report the first match
            let idx = matches[0];
            let res = active_indices[idx];
            conflicts.push(GuardConflict {
                path: path.clone(),
                pattern: res.path_pattern.clone(),
                holder: res.agent_name.clone(),
                expires_ts: res.expires_ts.clone(),
            });
            continue;
        }

        // Directory prefix check: if the path is a parent directory of the
        // reservation pattern's base, the path still conflicts. For example,
        // modifying `modules/submod` conflicts with reservation `modules/submod/**`
        // because the directory itself is within the reserved scope.
        for res in &active_indices {
            let res_pattern_lower;
            let res_pattern = if ignorecase {
                res_pattern_lower = res.normalized_pattern.to_lowercase();
                &res_pattern_lower
            } else {
                &res.normalized_pattern
            };

            // Check if the path is a prefix of the pattern's base directory
            // (e.g. path "src", pattern "src/main.rs" or "src/**")
            if res_pattern.starts_with(&normalized)
                && (normalized.is_empty()
                    || res_pattern
                        .as_bytes()
                        .get(normalized.len())
                        .is_some_and(|&c| c == b'/'))
            {
                conflicts.push(GuardConflict {
                    path: path.clone(),
                    pattern: res.path_pattern.clone(),
                    holder: res.agent_name.clone(),
                    expires_ts: res.expires_ts.clone(),
                });
                break;
            }

            // Also check the reverse: pattern's literal base is a prefix of the path.
            // This is needed for literal directory reservations (e.g. reserving "src/subdir"
            // blocks edits to "src/subdir/file.rs").
            // We do NOT do this if the pattern has a glob, because globs like "src/*"
            // should not recursively lock subdirectories (that's what "src/**" is for).
            if !res.has_glob {
                let literal_base = res_pattern;
                if normalized.starts_with(literal_base)
                    && (literal_base.is_empty()
                        || normalized
                            .as_bytes()
                            .get(literal_base.len())
                            .is_some_and(|&c| c == b'/'))
                {
                    conflicts.push(GuardConflict {
                        path: path.clone(),
                        pattern: res.path_pattern.clone(),
                        holder: res.agent_name.clone(),
                        expires_ts: res.expires_ts.clone(),
                    });
                    break;
                }
            }
        }
    }

    Ok(conflicts)
}

/// Normalize a path for matching: forward slashes, strip leading `./` and `/`,
/// and collapse `..` segments to prevent path traversal mismatches.
fn normalize_path(path: &str, ignorecase: bool) -> String {
    let slashed = path.replace('\\', "/");
    // Collapse redundant components: strip leading `./`, resolve `..`
    let mut parts: Vec<&str> = Vec::new();
    for component in slashed.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if parts.is_empty() {
                    // Clamp traversal at root so `../x` normalizes to `x`.
                    // This keeps matching conservative and prevents escape prefixes.
                } else {
                    parts.pop();
                }
            }
            other => parts.push(other),
        }
    }
    let normalized = parts.join("/");
    if ignorecase {
        // Use Unicode-aware lowercase (not ASCII-only) so that
        // non-ASCII path components on case-insensitive filesystems
        // (macOS HFS+, Windows NTFS) are matched correctly.
        normalized.to_lowercase()
    } else {
        normalized
    }
}

fn detect_core_ignorecase(repo_hint: &Path) -> bool {
    git2::Repository::discover(repo_hint)
        .ok()
        .and_then(|repo| repo.config().ok())
        .and_then(|cfg| cfg.get_bool("core.ignorecase").ok())
        .unwrap_or(false)
}

/// Returns true if the string contains glob metacharacters.
fn contains_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{')
}

fn is_default_exempt_path(path: &str, ignorecase: bool) -> bool {
    let normalized = normalize_path(path, ignorecase);
    DEFAULT_EXEMPT_PATH_PREFIXES.iter().any(|prefix| {
        normalized == *prefix
            || normalized
                .strip_prefix(*prefix)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

/// Unreleased reservation artifacts plus, per reservation id, the newest
/// released artifact seen (GH#299).
type ReservationArtifactScan = (
    Vec<(std::path::PathBuf, serde_json::Value)>,
    std::collections::HashMap<u64, ReservationArtifactStamp>,
);

fn scan_reservation_artifacts(reservations_dir: &Path) -> GuardResult<ReservationArtifactScan> {
    // GH#299: after a mailbox rebuild the archive can hold two generation
    // stamped artifacts for one reservation id: `id-<id>-g<old>.json` with
    // `released_ts: null` from the previous database generation and
    // `id-<id>-g<current>.json` released under the current one. The old
    // artifact is history, not an active reservation. Track, per id, the
    // newest released artifact so a superseded foreign-generation active
    // twin is skipped below.
    let mut candidates: Vec<(std::path::PathBuf, serde_json::Value)> = Vec::new();
    let mut released_by_id: std::collections::HashMap<u64, ReservationArtifactStamp> =
        std::collections::HashMap::new();

    let entries = std::fs::read_dir(reservations_dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        // Only process .json files
        if !file_type.is_file()
            || file_type.is_symlink()
            || path.extension().and_then(|e| e.to_str()) != Some("json")
        {
            continue;
        }

        // Defend against arbitrary large files in the archive causing OOM in the pre-commit hook.
        let metadata = entry.metadata().ok();
        if let Some(meta) = &metadata
            && meta.len() > 1024 * 1024
        {
            // 1MB limit for reservation JSON
            continue;
        }

        // Skip unreadable files and invalid JSON.
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };

        // Skip released reservations, remembering them per reservation id.
        // Older archive artifacts sometimes persisted zero-like sentinels for
        // still-active reservations, and the DB layer still treats those as active.
        if released_ts_marks_record_released(&val["released_ts"]) {
            if let Some(stamp) = reservation_artifact_stamp(&path, &val, metadata.as_ref()) {
                released_by_id
                    .entry(stamp.id)
                    .and_modify(|existing| {
                        if stamp.modified > existing.modified {
                            *existing = stamp.clone();
                        }
                    })
                    .or_insert(stamp);
            }
            continue;
        }
        candidates.push((path, val));
    }
    Ok((candidates, released_by_id))
}

/// Read active file reservations from the archive's `file_reservations/` directory.
///
/// Parses each `*.json` file and returns records that are:
/// - Not released (`released_ts` is null or a legacy zero-like value)
/// - Not expired (`expires_ts > now`; at exact boundary reservation is expired)
/// - Exclusive
fn read_active_reservations_from_archive(
    archive_root: &Path,
    ignorecase: bool,
) -> GuardResult<Vec<FileReservationRecord>> {
    let reservations_dir = archive_root.join("file_reservations");
    if !is_real_directory(&reservations_dir) {
        return Ok(Vec::new());
    }

    let now = chrono::Utc::now();
    let mut records = Vec::new();
    let (candidates, released_by_id) = scan_reservation_artifacts(&reservations_dir)?;

    for (path, val) in candidates {
        // GH#299: an unreleased artifact from another database generation is
        // superseded by a released twin of the same id that is at least as
        // recent (the current generation's release), and never blocks delivery.
        if let Some(stamp) =
            reservation_artifact_stamp(&path, &val, std::fs::metadata(&path).ok().as_ref())
            && let Some(released) = released_by_id.get(&stamp.id)
            && released.generation != stamp.generation
            && released.modified >= stamp.modified
        {
            continue;
        }

        // Parse expires_ts and check expiry
        let expires_str = match val["expires_ts"].as_str() {
            Some(s) => s.to_string(),
            None => {
                if let Some(num) = val["expires_ts"].as_i64() {
                    // It's a numeric timestamp (microseconds). Convert it to a string so `is_expired` can parse it,
                    // or just check expiry right here.
                    let now_micros = now.timestamp_micros();
                    if num <= now_micros {
                        continue; // expired
                    }
                    // Not expired, generate an ISO string or just let it pass.
                    // For simplicity, we can pass a future ISO string to `is_expired` or
                    // bypass the string logic. It's cleaner to format it.
                    let nanos = u32::try_from((num % 1_000_000) * 1000).unwrap_or(0);
                    match <chrono::Utc as chrono::TimeZone>::timestamp_opt(
                        &chrono::Utc,
                        num / 1_000_000,
                        nanos,
                    ) {
                        chrono::LocalResult::Single(dt) => dt.to_rfc3339(),
                        _ => continue,
                    }
                } else {
                    continue;
                }
            }
        };
        if is_expired(&expires_str, &now) {
            continue;
        }

        // Extract fields
        let pattern = val["path_pattern"]
            .as_str()
            .or_else(|| val["path"].as_str())
            .map_or("", str::trim)
            .to_string();
        if pattern.is_empty() {
            continue;
        }

        let Some(exclusive) = val["exclusive"].as_bool() else {
            continue;
        };
        let agent_name = val["agent_name"]
            .as_str()
            .or_else(|| val["agent"].as_str())
            .map_or("", str::trim)
            .to_string();
        if agent_name.is_empty() {
            continue;
        }

        let normalized_pattern = normalize_path(&pattern, ignorecase);
        let has_glob = contains_glob(&pattern);

        records.push(FileReservationRecord {
            path_pattern: pattern,
            agent_name,
            exclusive,
            expires_ts: expires_str,
            released_ts: None,
            normalized_pattern,
            has_glob,
        });
    }

    Ok(records)
}

/// Identity of one reservation artifact: numeric reservation id, the
/// database generation it was written under (from the `id-<id>-g<gen>.json`
/// name, else the record's `db_generation`, else none), and its mtime.
#[derive(Clone, Debug)]
struct ReservationArtifactStamp {
    id: u64,
    generation: Option<String>,
    modified: std::time::SystemTime,
}

fn reservation_artifact_stamp(
    path: &Path,
    record: &serde_json::Value,
    metadata: Option<&std::fs::Metadata>,
) -> Option<ReservationArtifactStamp> {
    let stem = path.file_stem()?.to_str()?;
    let rest = stem.strip_prefix("id-")?;
    let (id_text, name_generation) = match rest.split_once("-g") {
        Some((id, generation)) if !generation.is_empty() => (id, Some(generation.to_string())),
        _ => (rest, None),
    };
    let id = id_text.parse::<u64>().ok()?;
    let generation = name_generation.or_else(|| {
        record["db_generation"]
            .as_str()
            .map(str::trim)
            .filter(|generation| !generation.is_empty())
            .map(str::to_string)
    });
    let modified = metadata
        .and_then(|meta| meta.modified().ok())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    Some(ReservationArtifactStamp {
        id,
        generation,
        modified,
    })
}

fn released_ts_marks_record_released(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Number(number) => number.as_f64().is_some_and(|value| value > 0.0),
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty()
                || trimmed.eq_ignore_ascii_case("null")
                || trimmed.eq_ignore_ascii_case("none")
                || trimmed == "0"
            {
                return false;
            }

            if trimmed
                .chars()
                .all(|ch| ch.is_ascii_digit() || matches!(ch, '.' | '+' | '-'))
            {
                // Fail-closed: unparseable numeric strings are NOT released.
                return trimmed.parse::<f64>().is_ok_and(|value| value > 0.0);
            }

            // Non-numeric, non-empty, non-null strings (e.g. ISO timestamps)
            // are treated as valid release markers.
            true
        }
        // Fail-closed: null and unexpected JSON types are NOT treated as released.
        _ => false,
    }
}

/// Check if a timestamp string is expired relative to `now`.
fn is_expired(ts_str: &str, now: &chrono::DateTime<chrono::Utc>) -> bool {
    // Try parsing ISO-8601 with timezone
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str) {
        return dt <= *now;
    }
    // Try parsing ISO-8601 without timezone (assume UTC).
    // Use `<=` to match the RFC3339 branch and the DB layer's `expires_ts > now`
    // semantics (i.e., expired means expires_ts <= now).
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%S%.f") {
        let utc = dt.and_utc();
        return utc <= *now;
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%S") {
        let utc = dt.and_utc();
        return utc <= *now;
    }
    // Fallback for string-wrapped numeric timestamps (microseconds)
    if let Ok(num) = ts_str.parse::<i64>() {
        return num <= now.timestamp_micros();
    }
    // If we can't parse, treat as NOT expired (conservative/fail-closed)
    false
}

// ---------------------------------------------------------------------------
// Git helpers: staged paths and push paths
// ---------------------------------------------------------------------------

/// Run a git Command with SIGSEGV retry (br-8ujfs.5.5 / E5).
///
/// The guard cannot use `run_git_locked` (it executes inside the user's
/// pre-commit process, same pid as the git commit that invoked us;
/// wrapping with flock would deadlock). But we CAN still retry on
/// SIGSEGV locally — same bug, same fingerprint, same fix. Matches the
/// policy in `mcp_agent_mail_core::git_cmd::GitCmd::run` (3 retries,
/// 100/400/1600ms jittered). Only retries segfault-shaped exits.
fn guard_run_git_with_retry(mut cmd: Command) -> std::io::Result<std::process::Output> {
    const BACKOFFS_MS: [u64; 3] = [100, 400, 1600];
    let mut last_output: Option<std::process::Output> = None;
    for (attempt, maybe_base) in BACKOFFS_MS
        .into_iter()
        .map(Some)
        .chain(std::iter::once(None))
        .enumerate()
    {
        let output = cmd.output()?;
        let signal_segfault = {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                matches!(output.status.signal(), Some(11 | 7))
            }
            #[cfg(not(unix))]
            false
        };
        let code_segfault = matches!(output.status.code(), Some(139 | 135));
        if !signal_segfault && !code_segfault {
            return Ok(output);
        }
        let Some(base) = maybe_base else {
            tracing::warn!(
                target: "mcp_agent_mail::guard::segfault_retry",
                attempt = attempt,
                "guard_git_segfault_retry_exhausted"
            );
            last_output = Some(output);
            break;
        };
        tracing::warn!(
            target: "mcp_agent_mail::guard::segfault_retry",
            attempt = attempt,
            "guard_git_segfault_retry"
        );
        // Jitter formula MUST match mcp_agent_mail_core::git_cmd::jitter_ms
        // so the guard's retry cadence is identical to the server's.
        //   span = base / 2           (half the base)
        //   low  = base - span / 2    (~ 0.75 * base)
        //   jitter ∈ [low, low + span)
        // For base=100 → [75, 125), base=400 → [300, 500), base=1600 → [1200, 2000).
        let span = base / 2;
        let low = base - span / 2;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::from(d.subsec_nanos()));
        let offset = nanos % span.max(1);
        let jitter = low + offset;
        std::thread::sleep(std::time::Duration::from_millis(jitter));
    }
    Ok(last_output.expect("last_output set before break"))
}

/// Get staged file paths from git, including rename handling.
///
/// Uses `git diff --cached --name-status -M -z` to capture both old and new names
/// for renames (R status), and all modified/added/deleted paths.
pub fn get_staged_paths(repo_root: &Path) -> GuardResult<Vec<String>> {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_root)
        .args(["diff", "--cached", "--name-status", "-M", "-z"]);
    let output = guard_run_git_with_retry(cmd)?;

    if !output.status.success() {
        // Fail-closed: if git diff fails, return an error so the guard blocks
        // the commit rather than silently allowing it through with no checks.
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GuardError::Io(std::io::Error::other(format!(
            "git diff --cached failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr.trim(),
        ))));
    }

    parse_name_status_z(&output.stdout)
}

/// Get paths changed in a push range (for pre-push hook).
///
/// Parses stdin ref tuples `<local_ref> <local_sha> <remote_ref> <remote_sha>` and
/// uses `git diff --name-status -M -z <remote>..<local>` to find changed files.
pub fn get_push_paths(repo_root: &Path, stdin_lines: &str) -> GuardResult<Vec<String>> {
    let mut all_paths = Vec::new();

    for line in stdin_lines.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let local_sha = parts[1];
        let remote_sha = parts[3];

        // Skip delete pushes (local is all zeros)
        if local_sha.chars().all(|c| c == '0') {
            continue;
        }

        let mut rev_list_cmd = Command::new("git");
        rev_list_cmd
            .current_dir(repo_root)
            .args(["rev-list", "--topo-order"]);
        let diff_range = if remote_sha.chars().all(|c| c == '0') {
            rev_list_cmd.args([local_sha, "--not", "--remotes"]);
            None
        } else {
            let r = format!("{remote_sha}..{local_sha}");
            rev_list_cmd.arg(&r);
            Some(r)
        };

        // Prefer per-commit path enumeration (legacy guard.py parity): this catches paths
        // that were touched in any pushed commit, even if the net diff ends up empty.
        let rev_list = guard_run_git_with_retry(rev_list_cmd)?;

        if rev_list.status.success() {
            for sha in String::from_utf8_lossy(&rev_list.stdout)
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let mut diff_tree_cmd = Command::new("git");
                // `--cc` (not `-m`): on a merge commit `-m` explodes the diff
                // into one section PER PARENT, flagging every file merely carried
                // in from origin as a "pushed change" (false positive, issue
                // #238). `--cc` reports only the files the merge itself changed
                // relative to ALL parents — preserving the fail-closed check for
                // genuine conflict-resolution edits to reserved paths while
                // dropping carried files. On a regular (single-parent) commit
                // `--cc` and `-m` produce identical --name-status output, so
                // there is no false negative on normal commits.
                diff_tree_cmd.current_dir(repo_root).args([
                    "diff-tree",
                    "--root",
                    "-r",
                    "--no-commit-id",
                    "--name-status",
                    "-M",
                    "--no-ext-diff",
                    "--diff-filter=ACMRDTU",
                    "-z",
                    "--cc",
                    sha,
                ]);
                let output = guard_run_git_with_retry(diff_tree_cmd)?;

                if output.status.success() {
                    let paths = parse_name_status_z(&output.stdout)?;
                    all_paths.extend(paths);
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(GuardError::Io(std::io::Error::other(format!(
                        "git diff-tree failed for {sha} (exit {}): {}",
                        output.status.code().unwrap_or(-1),
                        stderr.trim(),
                    ))));
                }
            }
        } else if let Some(range) = diff_range {
            // Fallback: net diff across the range (less precise, but better than nothing).
            let mut diff_cmd = Command::new("git");
            diff_cmd
                .current_dir(repo_root)
                .args(["diff", "--name-status", "-M", "-z", &range]);
            let output = guard_run_git_with_retry(diff_cmd)?;

            if output.status.success() {
                let paths = parse_name_status_z(&output.stdout)?;
                all_paths.extend(paths);
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(GuardError::Io(std::io::Error::other(format!(
                    "git diff fallback failed for {range} (exit {}): {}",
                    output.status.code().unwrap_or(-1),
                    stderr.trim(),
                ))));
            }
        } else {
            // New-branch push (remote all-zeros) where `git rev-list … --not
            // --remotes` itself failed: fail CLOSED rather than silently allowing
            // the push with zero reservation enforcement. Mirrors the enforced
            // Python pre-push plugin, which exits non-zero on rev-list failure.
            let stderr = String::from_utf8_lossy(&rev_list.stderr);
            return Err(GuardError::Io(std::io::Error::other(format!(
                "git rev-list failed for new-branch push (exit {}): {}",
                rev_list.status.code().unwrap_or(-1),
                stderr.trim(),
            ))));
        }
    }

    // Deduplicate
    all_paths.sort();
    all_paths.dedup();
    Ok(all_paths)
}

/// Parse NUL-delimited `git diff --name-status -z` output.
///
/// Format: `STATUS\0path\0` for most, `Rxx\0old\0new\0` for renames.
// Fallible by contract: the fuzz harness and the git callers treat parse failure as
// a guard error, even though the current parser tolerates every byte sequence.
#[allow(clippy::unnecessary_wraps)]
fn parse_name_status_z(raw: &[u8]) -> GuardResult<Vec<String>> {
    let text = String::from_utf8_lossy(raw);
    let parts: Vec<&str> = text.split('\0').collect();
    let mut paths = Vec::new();
    let mut i = 0;

    while i < parts.len() {
        let status = parts[i].trim();
        if status.is_empty() {
            i += 1;
            continue;
        }

        let first_char = status.chars().next().unwrap_or(' ');
        match first_char {
            'R' | 'C' => {
                // Renamed/Copied use 2 paths: old_path and new_path
                if i + 1 < parts.len() {
                    let old_p = parts[i + 1];
                    if !old_p.is_empty() {
                        paths.push(old_p.to_string());
                    }
                }
                if i + 2 < parts.len() {
                    let new_p = parts[i + 2];
                    if !new_p.is_empty() {
                        paths.push(new_p.to_string());
                    }
                }
                i += 3;
            }
            _ => {
                // Others use 1 path
                if i + 1 < parts.len() {
                    let p = parts[i + 1];
                    if !p.is_empty() {
                        paths.push(p.to_string());
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
    }

    Ok(paths)
}

#[cfg(fuzzing)]
#[must_use]
pub fn fuzz_normalize_path(path: &str, ignorecase: bool) -> String {
    normalize_path(path, ignorecase)
}

#[cfg(fuzzing)]
#[must_use]
pub fn fuzz_contains_glob(pattern: &str) -> bool {
    contains_glob(pattern)
}

#[cfg(fuzzing)]
pub fn fuzz_parse_name_status_z(raw: &[u8]) -> GuardResult<Vec<String>> {
    parse_name_status_z(raw)
}

#[cfg(fuzzing)]
pub fn fuzz_check_path_conflicts(
    paths: &[String],
    reservations: &[FileReservationRecord],
    self_agent: &str,
    ignorecase: bool,
) -> GuardResult<Vec<GuardConflict>> {
    check_path_conflicts(paths, reservations, self_agent, ignorecase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Simple fnmatch-style glob matching (like Python's `fnmatch.fnmatch`).
    /// `*` matches within a single directory, `**` matches across directories.
    fn fnmatch_simple(path: &str, pattern: &str) -> bool {
        if path == pattern {
            return true;
        }
        globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .is_ok_and(|g| g.compile_matcher().is_match(path))
    }

    /// Two paths/patterns conflict if:
    /// 1. They match each other via glob matching (symmetric), or
    /// 2. One is a directory prefix of the other (with `/` boundary).
    fn paths_conflict(a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        // Glob matching (symmetric)
        if fnmatch_simple(a, b) || fnmatch_simple(b, a) {
            return true;
        }
        // Directory prefix check: a is a prefix of b (or vice versa)
        if !a.is_empty() && !b.is_empty() {
            if b.starts_with(a) && b.as_bytes().get(a.len()) == Some(&b'/') {
                return true;
            }
            if a.starts_with(b) && a.as_bytes().get(b.len()) == Some(&b'/') {
                return true;
            }
        }
        false
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir);
        if args.first().is_some_and(|arg| *arg == "init")
            && !args.contains(&"-b")
            && !args.contains(&"--bare")
        {
            cmd.args(["init", "-b", "main"]);
            cmd.args(&args[1..]);
        } else {
            cmd.args(args);
        }
        let out = cmd.output().expect("git must run");
        assert!(
            out.status.success(),
            "git {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn run_git_stdout(dir: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir);
        if args.first().is_some_and(|arg| *arg == "init")
            && !args.contains(&"-b")
            && !args.contains(&"--bare")
        {
            cmd.args(["init", "-b", "main"]);
            cmd.args(&args[1..]);
        } else {
            cmd.args(args);
        }
        let out = cmd.output().expect("git must run");
        assert!(
            out.status.success(),
            "git {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    // -----------------------------------------------------------------------
    // Gate parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn truthy_value_parsing_matches_legacy() {
        assert!(is_truthy_value(Some("1")));
        assert!(is_truthy_value(Some(" true ")));
        assert!(is_truthy_value(Some("T")));
        assert!(is_truthy_value(Some("yes")));
        assert!(is_truthy_value(Some("Y")));

        assert!(!is_truthy_value(Some("0")));
        assert!(!is_truthy_value(Some("false")));
        assert!(!is_truthy_value(Some("no")));
        assert!(!is_truthy_value(Some("")));
        assert!(!is_truthy_value(None));
    }

    #[test]
    fn guard_gate_from_values_checks_enforcement_flag() {
        assert!(is_guard_gated_from_values(None, None, None)); // Default true
        assert!(is_guard_gated_from_values(Some("1"), None, None));
        assert!(is_guard_gated_from_values(
            Some("true"),
            Some("0"),
            Some("0")
        ));
        assert!(!is_guard_gated_from_values(Some("0"), Some("1"), Some("1")));
    }

    #[test]
    fn guard_gate_not_disabled_by_worktrees_false() {
        // WORKTREES_ENABLED=false must NOT disable the file reservation guard
        assert!(is_guard_gated_from_values(None, Some("false"), None));
        assert!(is_guard_gated_from_values(None, Some("0"), None));
        assert!(is_guard_gated_from_values(
            None,
            Some("false"),
            Some("false")
        ));
        assert!(is_guard_gated_from_values(None, None, Some("false")));
        assert!(is_guard_gated_from_values(None, None, Some("0")));
    }

    // -----------------------------------------------------------------------
    // Hook resolution tests (existing)
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_hooks_dir_default() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);

        let hooks = resolve_hooks_dir(&repo_dir).expect("hooks dir");
        assert_eq!(hooks, repo_dir.join(".git").join("hooks"));
    }

    #[test]
    fn resolve_hooks_dir_core_hooks_path_absolute() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);

        let abs = td.path().join("alt_hooks");
        let repo = git2::Repository::discover(&repo_dir).expect("repo");
        repo.config()
            .expect("config")
            .set_str("core.hooksPath", abs.to_str().expect("utf8 path"))
            .expect("set hooksPath");

        let hooks = resolve_hooks_dir(&repo_dir).expect("hooks dir");
        assert_eq!(hooks, abs);
    }

    #[test]
    fn resolve_hooks_dir_core_hooks_path_relative() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);

        let repo = git2::Repository::discover(&repo_dir).expect("repo");
        repo.config()
            .expect("config")
            .set_str("core.hooksPath", ".githooks")
            .expect("set hooksPath");

        let hooks = resolve_hooks_dir(&repo_dir).expect("hooks dir");
        assert_eq!(hooks, repo_dir.join(".githooks"));
    }

    #[test]
    fn resolve_hooks_dir_worktree_uses_common_git_dir_hooks() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@example.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        // Create an initial commit so we can create a branch/worktree.
        std::fs::write(repo_dir.join("README"), "x").expect("write");
        run_git(&repo_dir, &["add", "README"]);
        run_git(&repo_dir, &["commit", "-qm", "init"]);
        run_git(&repo_dir, &["branch", "branch2"]);

        let wt_dir = td.path().join("wt");
        run_git(
            &repo_dir,
            &["worktree", "add", "-q", wt_dir.to_str().unwrap(), "branch2"],
        );

        let hooks = resolve_hooks_dir(&wt_dir).expect("hooks dir");
        assert_eq!(hooks, repo_dir.join(".git").join("hooks"));
    }

    #[test]
    fn install_and_uninstall_guard_preserves_existing_hook() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);

        let hooks_dir = repo_dir.join(".git").join("hooks");
        let pre_commit = hooks_dir.join("pre-commit");
        let orig_body = "#!/bin/sh\necho original\n";
        std::fs::write(&pre_commit, orig_body).expect("write pre-commit");

        install_guard("/abs/path/backend", &repo_dir, false).expect("install_guard");

        let chain_body = std::fs::read_to_string(&pre_commit).expect("read chain");
        assert!(
            chain_body.contains("mcp-agent-mail chain-runner (pre-commit)"),
            "expected chain-runner sentinel"
        );

        let preserved = std::fs::read_to_string(hooks_dir.join("pre-commit.orig"))
            .expect("read pre-commit.orig");
        assert_eq!(preserved, orig_body);

        let plugin_path = hooks_dir
            .join("hooks.d")
            .join("pre-commit")
            .join(PLUGIN_FILE_NAME);
        assert!(plugin_path.exists(), "expected plugin file to exist");

        uninstall_guard(&repo_dir).expect("uninstall_guard");

        assert!(!plugin_path.exists(), "expected plugin file to be removed");
        let restored = std::fs::read_to_string(&pre_commit).expect("read restored pre-commit");
        assert_eq!(restored, orig_body);
    }

    #[test]
    fn render_guard_plugin_script_uses_valid_python_booleans_in_slugify() {
        let script = render_guard_plugin_script("/abs/path/backend", "pre-commit");
        assert!(
            script.contains("prev_dash = False"),
            "slugify helper should emit valid Python booleans"
        );
        assert!(
            script.contains("prev_dash = True"),
            "slugify helper should emit valid Python booleans"
        );
        assert!(
            !script.contains("prev_dash = false"),
            "slugify helper must not emit Rust-style lowercase booleans"
        );
    }

    // -----------------------------------------------------------------------
    // Path normalization tests
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_strips_leading_slash_and_backslashes() {
        assert_eq!(
            normalize_path("/app/api/users.py", false),
            "app/api/users.py"
        );
        assert_eq!(
            normalize_path("app\\api\\users.py", false),
            "app/api/users.py"
        );
        assert_eq!(normalize_path("\\app\\api", false), "app/api");
        assert_eq!(normalize_path("already/clean", false), "already/clean");

        // Dot-dot collapse
        assert_eq!(normalize_path("app/../api/users.py", false), "api/users.py");
        assert_eq!(
            normalize_path("app/models/../../api/users.py", false),
            "api/users.py"
        );
        // Leading .. can't go above root, so collapses to nothing
        assert_eq!(normalize_path("../evil", false), "evil");
        assert_eq!(normalize_path("../../evil", false), "evil");
        // Single dot removal
        assert_eq!(
            normalize_path("app/./api/./file.py", false),
            "app/api/file.py"
        );
        // Mixed
        assert_eq!(
            normalize_path("app/other/../api/./lib.rs", false),
            "app/api/lib.rs"
        );
        // Case-insensitive mode
        assert_eq!(normalize_path("App/../SRC/Lib.rs", true), "src/lib.rs");
    }

    // -----------------------------------------------------------------------
    // Path conflict matching tests
    // -----------------------------------------------------------------------

    #[test]
    fn exact_match() {
        assert!(paths_conflict("app/api/users.py", "app/api/users.py"));
    }

    #[test]
    fn glob_star_match() {
        assert!(paths_conflict("app/api/users.py", "app/api/*.py"));
        // Symmetric: pattern matches file either direction
        assert!(paths_conflict("app/api/*.py", "app/api/users.py"));
    }

    #[test]
    fn glob_double_star_match() {
        assert!(paths_conflict("app/api/v2/deep/users.py", "app/**/*.py"));
        assert!(paths_conflict("src/main.rs", "**/*.rs"));
    }

    #[test]
    fn directory_prefix_match() {
        assert!(paths_conflict("app/api/users.py", "app/api"));
        // Does not match unrelated path
        assert!(!paths_conflict("app/other/users.py", "app/api"));
    }

    #[test]
    fn no_false_positives() {
        assert!(!paths_conflict("app/api/users.py", "app/models/*.py"));
        assert!(!paths_conflict("src/main.rs", "tests/*.rs"));
        assert!(!paths_conflict("README.md", "app/*"));
    }

    #[test]
    fn wildcard_directory_match() {
        assert!(paths_conflict("app/api/users.py", "app/api/*"));
        assert!(paths_conflict("app/api/v2/users.py", "app/api/**"));
    }

    #[test]
    fn question_mark_glob() {
        assert!(paths_conflict("app/v1/users.py", "app/v?/users.py"));
        assert!(!paths_conflict("app/v12/users.py", "app/v?/users.py"));
    }

    // -----------------------------------------------------------------------
    // fnmatch_simple tests
    // -----------------------------------------------------------------------

    #[test]
    fn fnmatch_basic() {
        assert!(fnmatch_simple("foo.py", "*.py"));
        assert!(fnmatch_simple("foo.py", "foo.*"));
        assert!(fnmatch_simple("foo.py", "foo.py"));
        assert!(!fnmatch_simple("foo.py", "*.rs"));
    }

    #[test]
    fn fnmatch_double_star() {
        assert!(fnmatch_simple("a/b/c.py", "**/*.py"));
        assert!(fnmatch_simple("a/b/c/d.py", "**/d.py"));
        assert!(!fnmatch_simple("a/b/c.rs", "**/*.py"));
    }

    #[test]
    fn fnmatch_unicode_does_not_panic() {
        // Regression: the '*' backtracking logic must only slice at UTF-8 char boundaries.
        assert!(fnmatch_simple("a/ß.py", "**/*.py"));
        assert!(fnmatch_simple("ß.py", "*.py"));
        assert!(!fnmatch_simple("ß.rs", "*.py"));
    }

    #[test]
    fn fnmatch_question() {
        assert!(fnmatch_simple("a.py", "?.py"));
        assert!(!fnmatch_simple("ab.py", "?.py"));
    }

    // -----------------------------------------------------------------------
    // Reservation reading tests
    // -----------------------------------------------------------------------

    fn make_archive_with_reservations(td: &Path) -> PathBuf {
        let archive = td.join("archive");
        let res_dir = archive.join("file_reservations");
        std::fs::create_dir_all(&res_dir).expect("mkdir");

        // Active exclusive reservation by OtherAgent
        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let res1 = serde_json::json!({
            "path_pattern": "app/api/*.py",
            "agent_name": "OtherAgent",
            "exclusive": true,
            "expires_ts": future.to_rfc3339(),
            "released_ts": null
        });
        std::fs::write(res_dir.join("res1.json"), res1.to_string()).expect("write");

        // Released reservation (should be skipped)
        let res2 = serde_json::json!({
            "path_pattern": "docs/*",
            "agent_name": "OtherAgent",
            "exclusive": true,
            "expires_ts": future.to_rfc3339(),
            "released_ts": "2025-01-01T00:00:00Z"
        });
        std::fs::write(res_dir.join("res2.json"), res2.to_string()).expect("write");

        // Expired reservation (should be skipped)
        let past = chrono::Utc::now() - chrono::Duration::hours(1);
        let res3 = serde_json::json!({
            "path_pattern": "old/*",
            "agent_name": "ExpiredAgent",
            "exclusive": true,
            "expires_ts": past.to_rfc3339(),
            "released_ts": null
        });
        std::fs::write(res_dir.join("res3.json"), res3.to_string()).expect("write");

        // Non-exclusive reservation by OtherAgent (should be included but won't block)
        let res4 = serde_json::json!({
            "path_pattern": "shared/*",
            "agent_name": "SharedAgent",
            "exclusive": false,
            "expires_ts": future.to_rfc3339(),
            "released_ts": null
        });
        std::fs::write(res_dir.join("res4.json"), res4.to_string()).expect("write");

        // Self-owned reservation
        let res5 = serde_json::json!({
            "path_pattern": "my/stuff/*",
            "agent_name": "MyAgent",
            "exclusive": true,
            "expires_ts": future.to_rfc3339(),
            "released_ts": null
        });
        std::fs::write(res_dir.join("res5.json"), res5.to_string()).expect("write");

        archive
    }

    fn reservation(pattern: &str, holder: &str, exclusive: bool) -> FileReservationRecord {
        FileReservationRecord {
            path_pattern: pattern.to_string(),
            agent_name: holder.to_string(),
            exclusive,
            expires_ts: "2099-01-01T00:00:00Z".to_string(),
            released_ts: None,
            normalized_pattern: normalize_path(pattern, false),
            has_glob: contains_glob(pattern),
        }
    }

    /// GH#299: after a mailbox rebuild, an unreleased artifact from the previous
    /// database generation must not block delivery when the same reservation
    /// id has a released artifact under the current generation.
    #[test]
    fn foreign_generation_active_artifact_is_superseded_by_released_current_twin() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("archive");
        let res_dir = archive.join("file_reservations");
        std::fs::create_dir_all(&res_dir).expect("mkdir");
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();

        // Old generation: still "active" on disk.
        let old = serde_json::json!({
            "path_pattern": "src/lib.rs",
            "agent_name": "OtherAgent",
            "exclusive": true,
            "expires_ts": future,
            "released_ts": null,
            "db_generation": "oldgen00"
        });
        std::fs::write(res_dir.join("id-123-goldgen00.json"), old.to_string()).expect("write");
        // Current generation: released. Written afterwards, so at least as recent.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let current = serde_json::json!({
            "path_pattern": "src/lib.rs",
            "agent_name": "OtherAgent",
            "exclusive": true,
            "expires_ts": future,
            "released_ts": "2026-09-02T00:00:00Z",
            "db_generation": "curgen11"
        });
        std::fs::write(res_dir.join("id-123-gcurgen11.json"), current.to_string()).expect("write");
        // A different id with only an old-generation active artifact stays active:
        // nothing in the current generation says it was released.
        let lone = serde_json::json!({
            "path_pattern": "src/other.rs",
            "agent_name": "OtherAgent",
            "exclusive": true,
            "expires_ts": future,
            "released_ts": null,
            "db_generation": "oldgen00"
        });
        std::fs::write(res_dir.join("id-124-goldgen00.json"), lone.to_string()).expect("write");

        let active = read_active_reservations_from_archive(&archive, false).expect("read");
        let patterns: Vec<&str> = active.iter().map(|r| r.path_pattern.as_str()).collect();
        assert_eq!(
            patterns,
            vec!["src/other.rs"],
            "the superseded foreign-generation artifact must be skipped: {patterns:?}"
        );
    }

    #[test]
    fn read_active_reservations_filters_correctly() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("empty_archive");
        // No file_reservations dir at all
        let records = read_active_reservations_from_archive(&archive, false).expect("read");
        assert!(records.is_empty());
    }

    #[test]
    fn read_reservations_keeps_legacy_zero_like_released_ts_values_active() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("archive");
        let dir = archive.join("file_reservations");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let released_values = [
            serde_json::json!(0),
            serde_json::json!("0"),
            serde_json::json!(""),
            serde_json::json!("null"),
            serde_json::json!("none"),
            serde_json::json!(-1),
        ];

        for (index, released_ts) in released_values.into_iter().enumerate() {
            let payload = serde_json::json!({
                "path_pattern": format!("legacy/{index}/*"),
                "agent_name": format!("Legacy{index}"),
                "exclusive": true,
                "expires_ts": "2099-01-01T00:00:00Z",
                "released_ts": released_ts,
            });
            let path = dir.join(format!("legacy-{index}.json"));
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&payload).expect("serialize"),
            )
            .expect("write reservation");
        }

        let records = read_active_reservations_from_archive(&archive, false).expect("read");
        assert_eq!(records.len(), 6);
        for index in 0..6 {
            assert!(
                records
                    .iter()
                    .any(|record| record.path_pattern == format!("legacy/{index}/*")),
                "missing legacy reservation {index}"
            );
        }
    }

    #[test]
    fn read_reservations_skips_positive_numeric_released_ts() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("archive");
        let dir = archive.join("file_reservations");
        std::fs::create_dir_all(&dir).expect("create file_reservations");

        let payload = serde_json::json!({
            "path_pattern": "released/*",
            "agent_name": "ReleasedAgent",
            "exclusive": true,
            "expires_ts": "2099-01-01T00:00:00Z",
            "released_ts": 42,
        });
        let path = dir.join("released.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&payload).expect("serialize"),
        )
        .expect("write reservation");

        let records = read_active_reservations_from_archive(&archive, false).expect("read");
        assert!(
            records.is_empty(),
            "positive numeric released_ts should skip reservation"
        );
    }

    // -----------------------------------------------------------------------
    // Conflict detection integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn check_path_conflicts_detects_matching_reservations() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let reservations = read_active_reservations_from_archive(&archive, false).expect("read");
        let paths = vec!["app/api/users.py".to_string()];

        let conflicts =
            check_path_conflicts(&paths, &reservations, "MyAgent", false).expect("conflicts");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].holder, "OtherAgent");
        assert_eq!(conflicts[0].pattern, "app/api/*.py");
        assert_eq!(conflicts[0].path, "app/api/users.py");
    }

    #[test]
    fn check_path_conflicts_skips_own_reservations() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let reservations = read_active_reservations_from_archive(&archive, false).expect("read");
        let paths = vec!["my/stuff/file.txt".to_string()];

        // "MyAgent" should not conflict with its own reservation
        let conflicts =
            check_path_conflicts(&paths, &reservations, "MyAgent", false).expect("conflicts");
        assert!(conflicts.is_empty(), "own reservations should be skipped");
    }

    #[test]
    fn check_path_conflicts_skips_own_reservations_case_insensitively() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let reservations = read_active_reservations_from_archive(&archive, false).expect("read");
        let paths = vec!["src/main.rs".to_string()];

        let conflicts =
            check_path_conflicts(&paths, &reservations, "bluelake", false).expect("conflicts");
        assert!(
            conflicts.is_empty(),
            "own reservations should be skipped regardless of agent-name casing"
        );
    }

    #[test]
    fn check_path_conflicts_exempts_beads_metadata_but_not_source_files() {
        let reservations = vec![
            FileReservationRecord {
                path_pattern: ".beads/**".to_string(),
                agent_name: "OtherAgent".to_string(),
                exclusive: true,
                expires_ts: "2099-01-01T00:00:00Z".to_string(),
                released_ts: None,
                normalized_pattern: ".beads/**".to_string(),
                has_glob: true,
            },
            FileReservationRecord {
                path_pattern: "src/**".to_string(),
                agent_name: "OtherAgent".to_string(),
                exclusive: true,
                expires_ts: "2099-01-01T00:00:00Z".to_string(),
                released_ts: None,
                normalized_pattern: "src/**".to_string(),
                has_glob: true,
            },
        ];
        let paths = vec![".beads/issues.jsonl".to_string(), "src/lib.rs".to_string()];

        let conflicts =
            check_path_conflicts(&paths, &reservations, "Committer", false).expect("conflicts");

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "src/lib.rs");
    }

    #[test]
    fn check_path_conflicts_skips_non_exclusive() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let reservations = read_active_reservations_from_archive(&archive, false).expect("read");
        let paths = vec!["shared/README.md".to_string()];

        // SharedAgent's non-exclusive reservation should not block
        let conflicts = check_path_conflicts(&paths, &reservations, "SomeOtherAgent", false)
            .expect("conflicts");
        assert!(
            conflicts.is_empty(),
            "non-exclusive reservations should not conflict"
        );
    }

    #[test]
    fn check_path_conflicts_no_match() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let reservations = read_active_reservations_from_archive(&archive, false).expect("read");
        let paths = vec!["unrelated/file.txt".to_string()];

        let conflicts =
            check_path_conflicts(&paths, &reservations, "MyAgent", false).expect("conflicts");
        assert!(conflicts.is_empty());
    }

    #[test]
    fn check_path_conflicts_root_reservation_blocks_everything() {
        let reservations = vec![FileReservationRecord {
            path_pattern: String::new(),
            agent_name: "OtherAgent".to_string(),
            exclusive: true,
            expires_ts: "2099-01-01T00:00:00Z".to_string(),
            released_ts: None,
            normalized_pattern: String::new(),
            has_glob: false,
        }];

        let conflicts = check_path_conflicts(
            &["src/main.rs".to_string(), "any/path".to_string()],
            &reservations,
            "SelfAgent",
            false,
        )
        .unwrap();

        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].path, "src/main.rs");
        assert_eq!(conflicts[1].path, "any/path");
    }

    #[test]
    fn check_path_conflicts_glob_prefix_blocks_subdirectories() {
        let reservations = vec![FileReservationRecord {
            path_pattern: "src/*".to_string(),
            agent_name: "OtherAgent".to_string(),
            exclusive: true,
            expires_ts: "2099-01-01T00:00:00Z".to_string(),
            released_ts: None,
            normalized_pattern: "src/*".to_string(),
            has_glob: true,
        }];

        // "src/*" with literal_separator(true) normally wouldn't match "src/subdir/file.rs"
        // and we removed the literal_base prefix logic for globs.
        let conflicts = check_path_conflicts(
            &["src/subdir/file.rs".to_string()],
            &reservations,
            "SelfAgent",
            false,
        )
        .unwrap();

        assert_eq!(conflicts.len(), 0);
    }

    #[test]
    fn check_path_conflicts_multiple_paths() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let reservations = read_active_reservations_from_archive(&archive, false).expect("read");
        let paths = vec![
            "app/api/users.py".to_string(),
            "app/api/models.py".to_string(),
            "unrelated.txt".to_string(),
        ];

        let conflicts =
            check_path_conflicts(&paths, &reservations, "SomeAgent", false).expect("conflicts");
        assert_eq!(conflicts.len(), 2, "two paths should conflict");
        assert!(conflicts.iter().all(|c| c.holder == "OtherAgent"));
    }

    #[test]
    fn check_path_conflicts_empty_reservations_allows_all_paths() {
        let paths = vec![
            "app/api/users.py".to_string(),
            "bin/tool.exe".to_string(),
            "modules/submod".to_string(),
        ];
        let conflicts = check_path_conflicts(&paths, &[], "AnyAgent", false).expect("conflicts");
        assert!(
            conflicts.is_empty(),
            "empty reservation set should never block"
        );
    }

    #[test]
    fn check_path_conflicts_submodule_pointer_path_matches_recursive_pattern() {
        let paths = vec!["modules/submod".to_string()];
        let reservations = vec![reservation("modules/submod/**", "OtherAgent", true)];
        let conflicts =
            check_path_conflicts(&paths, &reservations, "MyAgent", false).expect("conflicts");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].holder, "OtherAgent");
    }

    #[test]
    fn check_path_conflicts_non_glob_directory_prefix_matches_contained_file() {
        let paths = vec!["src/utils/file.rs".to_string()];
        // Reservation is a literal directory without glob metacharacters
        let reservations = vec![reservation("src/utils", "OtherAgent", true)];
        let conflicts =
            check_path_conflicts(&paths, &reservations, "MyAgent", false).expect("conflicts");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].holder, "OtherAgent");
    }

    #[test]
    fn check_path_conflicts_binary_file_matches_glob() {
        let paths = vec!["bin/tool.exe".to_string()];
        let reservations = vec![reservation("bin/*.exe", "Locker", true)];
        let conflicts =
            check_path_conflicts(&paths, &reservations, "MyAgent", false).expect("conflicts");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "bin/tool.exe");
    }

    #[test]
    fn check_path_conflicts_overlapping_shared_and_exclusive_blocks_on_exclusive() {
        let paths = vec!["app/api/users.py".to_string()];
        let reservations = vec![
            reservation("app/api/*.py", "SharedAgent", false),
            reservation("app/**", "ExclusiveAgent", true),
        ];
        let conflicts =
            check_path_conflicts(&paths, &reservations, "MyAgent", false).expect("conflicts");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].holder, "ExclusiveAgent");
        assert_eq!(conflicts[0].pattern, "app/**");
    }

    #[test]
    fn check_path_conflicts_rename_old_and_new_paths_conflict_independently() {
        let renamed_paths = parse_name_status_z(b"R100\0src/old.rs\0src/new.rs\0").expect("parse");
        let reservations = vec![
            reservation("src/old.rs", "OldOwner", true),
            reservation("src/new.rs", "NewOwner", true),
        ];

        let conflicts = check_path_conflicts(&renamed_paths, &reservations, "MyAgent", false)
            .expect("conflicts");
        assert_eq!(conflicts.len(), 2);
        assert!(
            conflicts
                .iter()
                .any(|c| c.path == "src/old.rs" && c.holder == "OldOwner")
        );
        assert!(
            conflicts
                .iter()
                .any(|c| c.path == "src/new.rs" && c.holder == "NewOwner")
        );
    }

    #[test]
    fn check_path_conflicts_large_reservation_set_still_finds_match() {
        let mut reservations = Vec::with_capacity(1_200);
        for i in 0..1_199usize {
            reservations.push(reservation(
                &format!("src/no_match_{i}.rs"),
                "BulkOwner",
                true,
            ));
        }
        reservations.push(reservation("src/target.rs", "TargetOwner", true));

        let paths = vec!["src/target.rs".to_string()];
        let conflicts =
            check_path_conflicts(&paths, &reservations, "MyAgent", false).expect("conflicts");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].holder, "TargetOwner");
    }

    #[test]
    fn check_path_conflicts_skips_invalid_reservation_pattern() {
        let paths = vec!["src/main.rs".to_string()];
        let reservations = vec![reservation("src/[abc", "OtherAgent", true)];

        let conflicts = check_path_conflicts(&paths, &reservations, "MyAgent", false)
            .expect("invalid reservation pattern should be ignored");
        assert!(conflicts.is_empty());
    }

    // -----------------------------------------------------------------------
    // Expiry parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn is_expired_rfc3339() {
        let now = chrono::Utc::now();
        let past = (now - chrono::Duration::hours(1)).to_rfc3339();
        let future = (now + chrono::Duration::hours(1)).to_rfc3339();

        assert!(is_expired(&past, &now));
        assert!(!is_expired(&future, &now));
    }

    #[test]
    fn is_expired_naive_datetime() {
        let now = chrono::Utc::now();
        let past = (now - chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S%.6f")
            .to_string();
        assert!(is_expired(&past, &now));
    }

    #[test]
    fn is_expired_unparseable_is_not_expired() {
        let now = chrono::Utc::now();
        assert!(!is_expired("not-a-date", &now));
    }

    // -----------------------------------------------------------------------
    // parse_name_status_z tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_name_status_simple() {
        // Simulate: A\0file.py\0M\0other.py\0
        let raw = b"A\0file.py\0M\0other.py\0";
        let paths = parse_name_status_z(raw).expect("parse");
        assert_eq!(paths, vec!["file.py", "other.py"]);
    }

    #[test]
    fn parse_name_status_rename() {
        // Simulate: R100\0old.py\0new.py\0
        let raw = b"R100\0old.py\0new.py\0";
        let paths = parse_name_status_z(raw).expect("parse");
        assert_eq!(paths, vec!["old.py", "new.py"]);
    }

    #[test]
    fn parse_name_status_mixed() {
        // A\0added.py\0R050\0old.py\0new.py\0D\0deleted.py\0
        let raw = b"A\0added.py\0R050\0old.py\0new.py\0D\0deleted.py\0";
        let paths = parse_name_status_z(raw).expect("parse");
        assert_eq!(paths, vec!["added.py", "old.py", "new.py", "deleted.py"]);
    }

    #[test]
    fn parse_name_status_empty() {
        let paths = parse_name_status_z(b"").expect("parse");
        assert_eq!(paths, [] as [std::string::String; 0]);
    }

    // -----------------------------------------------------------------------
    // Git integration: staged paths with renames
    // -----------------------------------------------------------------------

    #[test]
    fn staged_paths_includes_renames() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        // Create and commit a file
        std::fs::write(repo_dir.join("old_name.py"), "print('hello')").expect("write");
        run_git(&repo_dir, &["add", "old_name.py"]);
        run_git(&repo_dir, &["commit", "-qm", "add old_name"]);

        // Rename it
        run_git(&repo_dir, &["mv", "old_name.py", "new_name.py"]);

        let paths = get_staged_paths(&repo_dir).expect("staged paths");
        // Should have both old and new path
        assert!(
            paths.contains(&"old_name.py".to_string())
                || paths.contains(&"new_name.py".to_string()),
            "staged paths should include rename: {paths:?}"
        );
    }

    #[test]
    fn staged_paths_simple_add() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        // Create initial commit
        std::fs::write(repo_dir.join("init.txt"), "init").expect("write");
        run_git(&repo_dir, &["add", "init.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "init"]);

        // Stage a new file
        std::fs::write(repo_dir.join("new_file.py"), "# new").expect("write");
        run_git(&repo_dir, &["add", "new_file.py"]);

        let paths = get_staged_paths(&repo_dir).expect("staged paths");
        assert_eq!(paths, vec!["new_file.py"]);
    }

    #[test]
    fn staged_paths_includes_binary_file() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(repo_dir.join("bin")).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        std::fs::write(repo_dir.join("init.txt"), "init").expect("write");
        run_git(&repo_dir, &["add", "init.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "init"]);

        std::fs::write(
            repo_dir.join("bin").join("tool.exe"),
            [0u8, 159u8, 146u8, 150u8],
        )
        .expect("write binary");
        run_git(&repo_dir, &["add", "bin/tool.exe"]);

        let paths = get_staged_paths(&repo_dir).expect("staged paths");
        assert!(
            paths.contains(&"bin/tool.exe".to_string()),
            "expected staged binary path in output, got {paths:?}"
        );
    }

    #[test]
    fn staged_paths_submodule_pointer_update_included() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let super_repo = td.path().join("super");
        let sub_repo = td.path().join("sub");
        std::fs::create_dir_all(&super_repo).expect("mkdir super");
        std::fs::create_dir_all(&sub_repo).expect("mkdir sub");

        run_git(&sub_repo, &["init", "-q"]);
        run_git(&sub_repo, &["config", "user.email", "test@test.com"]);
        run_git(&sub_repo, &["config", "user.name", "test"]);
        std::fs::write(sub_repo.join("lib.rs"), "pub fn one() {}\n").expect("write sub lib");
        run_git(&sub_repo, &["add", "lib.rs"]);
        run_git(&sub_repo, &["commit", "-qm", "sub init"]);

        run_git(&super_repo, &["init", "-q"]);
        run_git(&super_repo, &["config", "user.email", "test@test.com"]);
        run_git(&super_repo, &["config", "user.name", "test"]);
        run_git(
            &super_repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub_repo.to_str().expect("utf8"),
                "modules/submod",
            ],
        );
        run_git(&super_repo, &["commit", "-qm", "add submodule"]);

        let sub_worktree = super_repo.join("modules").join("submod");
        run_git(&sub_worktree, &["config", "user.email", "test@test.com"]);
        run_git(&sub_worktree, &["config", "user.name", "test"]);
        std::fs::write(sub_worktree.join("lib.rs"), "pub fn two() {}\n").expect("write sub update");
        run_git(&sub_worktree, &["add", "lib.rs"]);
        run_git(&sub_worktree, &["commit", "-qm", "sub update"]);

        run_git(&super_repo, &["add", "modules/submod"]);

        let paths = get_staged_paths(&super_repo).expect("staged paths");
        assert!(
            paths.contains(&"modules/submod".to_string()),
            "expected staged submodule pointer path, got {paths:?}"
        );
    }

    #[test]
    fn staged_paths_empty_when_nothing_staged() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);

        let paths = get_staged_paths(&repo_dir).expect("staged paths");
        assert_eq!(paths, [] as [std::string::String; 0]);
    }

    // -----------------------------------------------------------------------
    // Git integration: pushed paths (pre-push)
    // -----------------------------------------------------------------------

    #[test]
    fn push_paths_includes_touched_files_even_if_net_diff_is_empty() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        let file = repo_dir.join("a.txt");
        std::fs::write(&file, "base\n").expect("write base");
        run_git(&repo_dir, &["add", "a.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "base"]);
        let remote_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        // Commit 1 touches the file.
        std::fs::write(&file, "one\n").expect("write one");
        run_git(&repo_dir, &["add", "a.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "touch"]);
        let _local_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        // Commit 2 reverts it so the net diff is empty even though the push touched the file.
        std::fs::write(&file, "base\n").expect("write revert");
        run_git(&repo_dir, &["add", "a.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "revert"]);
        let local_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        let stdin_lines = format!("refs/heads/main {local_sha} refs/heads/main {remote_sha}\n");
        let paths = get_push_paths(&repo_dir, &stdin_lines).expect("push paths");
        assert!(
            paths.contains(&"a.txt".to_string()),
            "expected a.txt in push paths, got: {paths:?}"
        );
    }

    #[test]
    fn push_paths_includes_renames() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        std::fs::write(repo_dir.join("old_name.py"), "print('hello')\n").expect("write");
        run_git(&repo_dir, &["add", "old_name.py"]);
        run_git(&repo_dir, &["commit", "-qm", "add old_name"]);
        let remote_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        run_git(&repo_dir, &["mv", "old_name.py", "new_name.py"]);
        run_git(&repo_dir, &["commit", "-qm", "rename"]);
        let local_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        let stdin_lines = format!("refs/heads/main {local_sha} refs/heads/main {remote_sha}\n");
        let paths = get_push_paths(&repo_dir, &stdin_lines).expect("push paths");
        assert!(
            paths.contains(&"old_name.py".to_string()),
            "expected old_name.py in push paths, got: {paths:?}"
        );
        assert!(
            paths.contains(&"new_name.py".to_string()),
            "expected new_name.py in push paths, got: {paths:?}"
        );
    }

    /// Issue #238: a merge commit that merely CARRIES a file already present on
    /// origin (the merge itself does not change it) must NOT flag that file. The
    /// old `-m` exploded the merge per-parent and false-flagged the carried file
    /// even though it was already on the remote; `--cc` reports only files the
    /// merge changed vs ALL parents.
    ///
    /// The carried file is introduced on the FEATURE side and `remote_sha` is
    /// set to the feature tip, so the feature commit is NOT in the pushed range
    /// (`feature..merge`) — only the merge commit and the divergent main commit
    /// are. This isolates the merge-commit per-parent explosion, which is the
    /// actual #238 bug.
    #[test]
    fn push_paths_merge_does_not_flag_carried_origin_files() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        // Base commit.
        std::fs::write(repo_dir.join("base.txt"), "base\n").expect("write base");
        run_git(&repo_dir, &["add", "base.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "base"]);

        // Feature branch adds carried.txt. Its tip is what origin already has,
        // so this commit is NOT part of the push.
        run_git(&repo_dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo_dir.join("carried.txt"), "from feature\n").expect("write carried");
        run_git(&repo_dir, &["add", "carried.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "add carried"]);
        // Origin's pre-push tip already contains the feature commit (carried.txt).
        let remote_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        // main diverges from base with its own change.
        run_git(&repo_dir, &["checkout", "-q", "main"]);
        std::fs::write(repo_dir.join("main_only.txt"), "main\n").expect("write main_only");
        run_git(&repo_dir, &["add", "main_only.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "main change"]);

        // Merge feature into main with no conflict — the merge commit only
        // CARRIES carried.txt (already on origin); it does not itself modify it.
        run_git(
            &repo_dir,
            &["merge", "--no-ff", "-q", "-m", "merge feature", "feature"],
        );
        let local_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        let stdin_lines = format!("refs/heads/main {local_sha} refs/heads/main {remote_sha}\n");
        let paths = get_push_paths(&repo_dir, &stdin_lines).expect("push paths");
        // carried.txt is already on origin (in remote_sha) and the merge commit
        // itself does not touch it, so --cc must omit it. With the old `-m`, the
        // merge's per-parent diff vs main's parent would have flagged it.
        assert!(
            !paths.contains(&"carried.txt".to_string()),
            "merge-carried origin file must NOT be flagged (issue #238), got: {paths:?}"
        );
        // main_only.txt was changed by a real (non-merge) pushed commit and must
        // still be flagged — the range still covers it.
        assert!(
            paths.contains(&"main_only.txt".to_string()),
            "non-merge pushed change must still be flagged, got: {paths:?}"
        );
    }

    /// Issue #238 (other half): a merge that ACTUALLY modifies a file relative
    /// to all parents (conflict resolution / evil merge) MUST still be flagged —
    /// `--cc` preserves the fail-closed check for real merge edits.
    #[test]
    fn push_paths_merge_flags_files_modified_by_the_merge_itself() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        // Base with shared.txt that both sides will edit (to force a conflict).
        std::fs::write(repo_dir.join("shared.txt"), "base line\n").expect("write shared");
        run_git(&repo_dir, &["add", "shared.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "base"]);
        let remote_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        // Feature edits shared.txt one way.
        run_git(&repo_dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo_dir.join("shared.txt"), "feature edit\n").expect("write feature");
        run_git(&repo_dir, &["add", "shared.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "feature edit"]);

        // main edits shared.txt a conflicting way.
        run_git(&repo_dir, &["checkout", "-q", "main"]);
        std::fs::write(repo_dir.join("shared.txt"), "main edit\n").expect("write main");
        run_git(&repo_dir, &["add", "shared.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "main edit"]);

        // Merge conflicts; resolve to a value that differs from BOTH parents so
        // the merge commit itself genuinely changes shared.txt.
        let merge = Command::new("git")
            .current_dir(&repo_dir)
            .args(["merge", "--no-ff", "-q", "-m", "merge", "feature"])
            .output()
            .expect("git merge runs");
        assert!(
            !merge.status.success(),
            "expected a merge conflict on shared.txt"
        );
        std::fs::write(repo_dir.join("shared.txt"), "merged resolution\n").expect("resolve");
        run_git(&repo_dir, &["add", "shared.txt"]);
        run_git(&repo_dir, &["commit", "-q", "--no-edit"]);
        let local_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        let stdin_lines = format!("refs/heads/main {local_sha} refs/heads/main {remote_sha}\n");
        let paths = get_push_paths(&repo_dir, &stdin_lines).expect("push paths");
        assert!(
            paths.contains(&"shared.txt".to_string()),
            "a file genuinely modified by the merge resolution must be flagged \
             (fail-closed preserved), got: {paths:?}"
        );
    }

    #[test]
    fn push_paths_skips_delete_pushes() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        // Delete push: local sha is all zeros. Should not attempt git and should return empty.
        let stdin_lines = "refs/heads/main 0000000000000000000000000000000000000000 refs/heads/main 1234567890abcdef1234567890abcdef12345678\n";
        let paths = get_push_paths(&repo_dir, stdin_lines).expect("push paths");
        assert_eq!(paths, [] as [std::string::String; 0]);
    }

    #[test]
    fn push_paths_initial_push_includes_root_commit_files() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        std::fs::write(repo_dir.join("tracked.rs"), "fn main() {}\n").expect("write tracked");
        run_git(&repo_dir, &["add", "tracked.rs"]);
        run_git(&repo_dir, &["commit", "-qm", "initial"]);
        let local_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        let stdin_lines = format!(
            "refs/heads/main {local_sha} refs/heads/main 0000000000000000000000000000000000000000\n"
        );
        let paths = get_push_paths(&repo_dir, &stdin_lines).expect("push paths");
        assert!(
            paths.contains(&"tracked.rs".to_string()),
            "expected tracked.rs in initial-push paths, got {paths:?}"
        );
    }

    #[test]
    fn push_paths_new_branch_remote_zero_still_enumerates_commits() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        std::fs::write(repo_dir.join("a.txt"), "base\n").expect("write base");
        run_git(&repo_dir, &["add", "a.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "base"]);
        let remote_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        run_git(&repo_dir, &["checkout", "--detach", "HEAD"]);
        std::fs::write(repo_dir.join("detached.txt"), "detached\n").expect("write detached");
        run_git(&repo_dir, &["add", "detached.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "detached commit"]);
        let local_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        let stdin_lines = format!("HEAD {local_sha} refs/heads/main {remote_sha}\n");
        let paths = get_push_paths(&repo_dir, &stdin_lines).expect("push paths");
        assert!(
            paths.contains(&"detached.txt".to_string()),
            "expected detached.txt in push paths, got {paths:?}"
        );
    }

    #[test]
    fn push_paths_detached_head_range_still_enumerates_changed_files() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        std::fs::write(repo_dir.join("base.txt"), "base\n").expect("write base");
        run_git(&repo_dir, &["add", "base.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "base"]);
        let remote_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        run_git(&repo_dir, &["checkout", "--detach", "HEAD"]);
        std::fs::write(repo_dir.join("detached.txt"), "detached\n").expect("write detached");
        run_git(&repo_dir, &["add", "detached.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "detached commit"]);
        let local_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        let stdin_lines = format!("HEAD {local_sha} refs/heads/main {remote_sha}\n");
        let paths = get_push_paths(&repo_dir, &stdin_lines).expect("push paths");
        assert!(
            paths.contains(&"detached.txt".to_string()),
            "expected detached.txt in push paths, got {paths:?}"
        );
    }

    // -----------------------------------------------------------------------
    // contains_glob tests
    // -----------------------------------------------------------------------

    #[test]
    fn contains_glob_detection() {
        assert!(contains_glob("*.py"));
        assert!(contains_glob("app/**"));
        assert!(contains_glob("file?.txt"));
        assert!(contains_glob("[abc].txt"));
        assert!(!contains_glob("app/api/users.py"));
        assert!(!contains_glob("plain_path"));
    }

    // -----------------------------------------------------------------------
    // guard_status tests
    // -----------------------------------------------------------------------

    #[test]
    fn guard_status_on_fresh_repo_no_guard_installed() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);

        let status = guard_status(&repo_dir).expect("guard_status");
        assert!(!status.pre_commit_present);
        assert!(!status.pre_push_present);
        assert!(!status.worktrees_enabled);
    }

    #[test]
    fn guard_status_after_install_shows_pre_commit_present() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);

        install_guard("/test/project", &repo_dir, false).expect("install");

        let status = guard_status(&repo_dir).expect("guard_status");
        assert!(
            status.pre_commit_present,
            "pre-commit hook should be detected after install"
        );
        assert!(
            status.hooks_dir.contains("hooks"),
            "hooks_dir should point to hooks directory"
        );
    }

    #[test]
    fn guard_status_invalid_repo_returns_error() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let nonexistent = td.path().join("does_not_exist");

        let result = guard_status(&nonexistent);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GuardError::InvalidRepo { .. }),
            "expected InvalidRepo, got: {err:?}"
        );
    }

    #[test]
    fn guard_status_worktrees_detected_when_hooks_path_set() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);

        // Set core.hooksPath so worktrees_enabled becomes true
        let repo = git2::Repository::discover(&repo_dir).expect("repo");
        repo.config()
            .expect("config")
            .set_str("core.hooksPath", "/some/hooks")
            .expect("set");

        let status = guard_status(&repo_dir).expect("guard_status");
        assert!(
            status.worktrees_enabled,
            "worktrees_enabled should be true when core.hooksPath is set"
        );
    }

    // -----------------------------------------------------------------------
    // Reservation edge case tests
    // -----------------------------------------------------------------------

    #[test]
    fn read_reservations_skips_malformed_json_files() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("archive");
        let res_dir = archive.join("file_reservations");
        std::fs::create_dir_all(&res_dir).expect("mkdir");

        // Write malformed JSON
        std::fs::write(res_dir.join("bad.json"), "this is not json {{{").expect("write");

        // Write a valid reservation too
        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let valid = serde_json::json!({
            "path_pattern": "src/**",
            "agent_name": "ValidAgent",
            "exclusive": true,
            "expires_ts": future.to_rfc3339(),
            "released_ts": null
        });
        std::fs::write(res_dir.join("valid.json"), valid.to_string()).expect("write");

        let records = read_active_reservations_from_archive(&archive, false).expect("read");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].agent_name, "ValidAgent");
    }

    #[test]
    fn read_reservations_skips_non_json_files() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("archive");
        let res_dir = archive.join("file_reservations");
        std::fs::create_dir_all(&res_dir).expect("mkdir");

        // Write a non-JSON file
        std::fs::write(res_dir.join("readme.txt"), "this is a readme").expect("write");
        std::fs::write(res_dir.join("notes.md"), "# notes").expect("write");

        let records = read_active_reservations_from_archive(&archive, false).expect("read");
        assert!(records.is_empty(), "non-json files should be ignored");
    }

    #[test]
    fn read_reservations_skips_empty_path_pattern() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("archive");
        let res_dir = archive.join("file_reservations");
        std::fs::create_dir_all(&res_dir).expect("mkdir");

        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let empty_pattern = serde_json::json!({
            "path_pattern": "",
            "agent_name": "Agent",
            "exclusive": true,
            "expires_ts": future.to_rfc3339(),
            "released_ts": null
        });
        std::fs::write(res_dir.join("empty.json"), empty_pattern.to_string()).expect("write");

        let records = read_active_reservations_from_archive(&archive, false).expect("read");
        assert!(
            records.is_empty(),
            "empty reservation patterns should be ignored"
        );
    }

    #[test]
    fn read_reservations_missing_agent_is_skipped() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("archive");
        let res_dir = archive.join("file_reservations");
        std::fs::create_dir_all(&res_dir).expect("mkdir");

        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let payload = serde_json::json!({
            "path_pattern": "src/**",
            "exclusive": true,
            "expires_ts": future.to_rfc3339(),
            "released_ts": null
        });
        std::fs::write(res_dir.join("missing-agent.json"), payload.to_string()).expect("write");

        let records = read_active_reservations_from_archive(&archive, false).expect("read");
        assert!(
            records.is_empty(),
            "missing agent metadata should be ignored"
        );
    }

    #[test]
    fn read_reservations_missing_exclusive_flag_is_skipped() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("archive");
        let res_dir = archive.join("file_reservations");
        std::fs::create_dir_all(&res_dir).expect("mkdir");

        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let payload = serde_json::json!({
            "path_pattern": "src/**",
            "agent_name": "Agent",
            "expires_ts": future.to_rfc3339(),
            "released_ts": null
        });
        std::fs::write(res_dir.join("missing-exclusive.json"), payload.to_string()).expect("write");

        let records = read_active_reservations_from_archive(&archive, false).expect("read");
        assert!(
            records.is_empty(),
            "missing exclusive flag should be ignored"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_reservations_skips_symlinked_reservations_directory() {
        use std::os::unix::fs::symlink;

        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("archive");
        let outside = td.path().join("outside");
        let outside_res_dir = outside.join("file_reservations");
        std::fs::create_dir_all(&outside_res_dir).expect("mkdir outside reservations");
        std::fs::create_dir_all(&archive).expect("mkdir archive");

        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let payload = serde_json::json!({
            "path_pattern": "src/**",
            "agent_name": "EscapedAgent",
            "exclusive": true,
            "expires_ts": future.to_rfc3339(),
            "released_ts": null
        });
        std::fs::write(outside_res_dir.join("escaped.json"), payload.to_string())
            .expect("write escaped reservation");
        symlink(&outside_res_dir, archive.join("file_reservations"))
            .expect("symlink reservations dir");

        let records = read_active_reservations_from_archive(&archive, false).expect("read");
        assert!(
            records.is_empty(),
            "symlinked file_reservations directory should be ignored"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_reservations_skips_symlinked_json_files() {
        use std::os::unix::fs::symlink;

        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("archive");
        let res_dir = archive.join("file_reservations");
        let outside = td.path().join("outside");
        std::fs::create_dir_all(&res_dir).expect("mkdir reservations");
        std::fs::create_dir_all(&outside).expect("mkdir outside");

        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let escaped = serde_json::json!({
            "path_pattern": "outside/**",
            "agent_name": "EscapedAgent",
            "exclusive": true,
            "expires_ts": future.to_rfc3339(),
            "released_ts": null
        });
        let local = serde_json::json!({
            "path_pattern": "inside/**",
            "agent_name": "LocalAgent",
            "exclusive": true,
            "expires_ts": future.to_rfc3339(),
            "released_ts": null
        });

        let outside_file = outside.join("escaped.json");
        std::fs::write(&outside_file, escaped.to_string()).expect("write outside reservation");
        symlink(&outside_file, res_dir.join("escaped.json")).expect("symlink reservation file");
        std::fs::write(res_dir.join("local.json"), local.to_string())
            .expect("write local reservation");

        let records = read_active_reservations_from_archive(&archive, false).expect("read");
        assert_eq!(
            records.len(),
            1,
            "only real reservation files should be read"
        );
        assert_eq!(records[0].agent_name, "LocalAgent");
    }

    // -----------------------------------------------------------------------
    // Additional paths_conflict boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn paths_conflict_slash_star_prefix_length_boundary() {
        // Pattern "app/*" with path == prefix "app" (no trailing slash).
        // path.len() == prefix.len() → should match (line 787 condition).
        assert!(paths_conflict("app", "app/*"));
    }

    #[test]
    fn paths_conflict_slash_star_not_matching_sibling() {
        // Pattern "app/*" should NOT match "application/file.py"
        // because "application" starts with "app" but next char is 'l', not '/'.
        assert!(!paths_conflict("application/file.py", "app/*"));
    }

    #[test]
    fn paths_conflict_double_star_suffix_matches_any_depth() {
        // Pattern "src/**" should match deeply nested paths.
        assert!(paths_conflict("src/a/b/c/d/e.rs", "src/**"));
        // And direct children too.
        assert!(paths_conflict("src/lib.rs", "src/**"));
    }

    #[test]
    fn paths_conflict_empty_strings() {
        assert!(paths_conflict("", ""));
        assert!(!paths_conflict("", "app/api"));
        assert!(!paths_conflict("app/api", ""));
    }

    #[test]
    fn paths_conflict_symmetric_directory_match() {
        // Reverse direction: path is the directory, pattern is the file.
        assert!(paths_conflict("app/api", "app/api/users.py"));
    }

    #[test]
    fn paths_conflict_directory_match_no_false_substring() {
        // "app/api" should NOT match "app/api_v2/file.py" (no slash boundary).
        assert!(!paths_conflict("app/api_v2/file.py", "app/api"));
    }

    // -----------------------------------------------------------------------
    // Additional parse_name_status_z edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn parse_name_status_incomplete_rename() {
        // Incomplete rename: status says R but only one path follows.
        let raw = b"R100\0old.rs\0";
        let paths = parse_name_status_z(raw).expect("parse");
        // Should break out of loop without crashing (i + 2 >= parts.len()).
        assert!(paths.is_empty() || paths == vec!["old.rs"]);
    }

    #[test]
    fn parse_name_status_unknown_status() {
        // Unknown status character should be skipped.
        let raw = b"X\0mystery.rs\0A\0known.rs\0";
        let paths = parse_name_status_z(raw).expect("parse");
        assert!(
            paths.contains(&"known.rs".to_string()),
            "known.rs should be parsed after unknown status"
        );
    }

    #[test]
    fn parse_name_status_trailing_nuls() {
        // Trailing NUL bytes should not produce spurious paths.
        let raw = b"M\0src/lib.rs\0\0\0";
        let paths = parse_name_status_z(raw).expect("parse");
        assert_eq!(paths, vec!["src/lib.rs"]);
    }

    #[test]
    fn parse_name_status_copy_entry() {
        // 'C' (copy) status should produce both old and new paths.
        let raw = b"C100\0original.rs\0copy.rs\0";
        let paths = parse_name_status_z(raw).expect("parse");
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"original.rs".to_string()));
        assert!(paths.contains(&"copy.rs".to_string()));
    }

    #[test]
    fn parse_name_status_all_status_types() {
        // Verify all known status types are handled.
        let raw = b"A\0added.rs\0M\0modified.rs\0D\0deleted.rs\0T\0typechange.rs\0U\0unmerged.rs\0";
        let paths = parse_name_status_z(raw).expect("parse");
        assert_eq!(paths.len(), 5);
    }

    // -----------------------------------------------------------------------
    // Additional fnmatch edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn fnmatch_star_does_not_cross_directory() {
        // Single * should NOT match across directory separators.
        assert!(!fnmatch_simple("a/b/c.py", "a/*.py"));
        assert!(fnmatch_simple("a/c.py", "a/*.py"));
    }

    #[test]
    fn fnmatch_empty_pattern_only_matches_empty() {
        assert!(fnmatch_simple("", ""));
        assert!(!fnmatch_simple("anything", ""));
    }

    #[test]
    fn fnmatch_star_at_beginning() {
        assert!(fnmatch_simple("test.py", "*.py"));
        assert!(fnmatch_simple(".py", "*.py"));
    }

    // -----------------------------------------------------------------------

    fn python_executable() -> Option<String> {
        for candidate in ["python3", "python"] {
            let Ok(output) = Command::new(candidate)
                .args(["-c", "import sys; print(sys.executable)"])
                .output()
            else {
                continue;
            };
            if !output.status.success() {
                continue;
            }
            let executable = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !executable.is_empty() {
                return Some(executable);
            }
        }
        None
    }

    #[test]
    fn guard_plugin_script_contains_project() {
        let script = render_guard_plugin_script("/my/project", "pre-commit");
        assert!(script.contains("mcp-agent-mail guard plugin (pre-commit)"));
        assert!(script.contains("PROJECT = \"/my/project\""));
        assert!(script.contains("get_staged_files"));
        assert!(script.contains("check_conflicts"));
        assert!(script.contains("def glob_to_regex"));
        assert!(script.contains("core.ignorecase"));
        assert!(script.contains("def resolve_identity_agent_name"));
        assert!(script.contains("def is_default_exempt_path"));
        assert!(script.contains("def check_conflicts(paths, reservations, self_agent):"));
        assert!(script.contains("self_agent = self_agent.lower()"));
        assert!(script.contains("if holder.lower() == self_agent:"));
        assert!(script.contains("has_glob = any(c in pattern for c in \"*?[{\")"));
        assert!(script.contains("record.get(\"path_pattern\") or record.get(\"path\")"));
        assert!(script.contains("def resolve_archive_root"));
        assert!(script.contains("def looks_like_project_slug"));
        assert!(script.contains("project.json"));
        assert!(script.contains("STORAGE_ROOT"));
        assert!(script.contains("project_value_is_slug = looks_like_project_slug(project_value)"));
        assert!(script.contains("\"/\" not in name"));
        assert!(script.contains("AGENT_NAME is unset and no current-pane identity"));
        assert!(script.contains("sys.exit(2)"));
    }

    #[test]
    fn guard_plugin_slug_collision_warns_instead_of_blocking_unrelated_commit() {
        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo").join("a").join("b");
        let colliding_project = td.path().join("repo").join("a-b");
        let storage_root = td.path().join("storage");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        std::fs::create_dir_all(&colliding_project).expect("mkdir colliding project");
        run_git(&repo_dir, &["init", "-q"]);

        let staged = repo_dir.join("src").join("main.rs");
        std::fs::create_dir_all(staged.parent().expect("src dir")).expect("mkdir src");
        std::fs::write(&staged, "fn main() {}\n").expect("write staged file");
        run_git(&repo_dir, &["add", "src/main.rs"]);

        let repo_identity =
            mcp_agent_mail_core::resolve_project_identity(&repo_dir.to_string_lossy());
        let colliding_identity =
            mcp_agent_mail_core::resolve_project_identity(&colliding_project.to_string_lossy());
        assert_eq!(
            repo_identity.slug, colliding_identity.slug,
            "test setup needs slug collision"
        );

        let archive_root = storage_root.join("projects").join(&repo_identity.slug);
        let reservations_dir = archive_root.join("file_reservations");
        std::fs::create_dir_all(&reservations_dir).expect("mkdir reservations");
        std::fs::write(
            archive_root.join("project.json"),
            serde_json::json!({
                "slug": colliding_identity.slug,
                "human_key": colliding_project.to_string_lossy(),
            })
            .to_string(),
        )
        .expect("write colliding metadata");
        std::fs::write(
            reservations_dir.join("conflict.json"),
            serde_json::json!({
                "path_pattern": "src/main.rs",
                "agent_name": "OtherAgent",
                "exclusive": true,
                "expires_ts": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                "released_ts": serde_json::Value::Null,
            })
            .to_string(),
        )
        .expect("write reservation");

        let script_path = td.path().join("guard.py");
        std::fs::write(
            &script_path,
            render_guard_plugin_script(&repo_dir.to_string_lossy(), "pre-commit"),
        )
        .expect("write guard script");

        let output = Command::new(&python)
            .current_dir(&repo_dir)
            .env("AGENT_NAME", "PinkStone")
            .env("STORAGE_ROOT", &storage_root)
            .arg(&script_path)
            .output()
            .expect("run guard script");

        assert_eq!(
            output.status.code(),
            Some(0),
            "guard should warn, not block, when only a colliding archive exists: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("WARNING: mcp-agent-mail: guard could not locate archive"),
            "expected archive lookup warning, got stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// GH#228: an archive-resolution ERROR (permissions) must not be coerced
    /// into the "no project matches → allow" path. chmod 000 on
    /// `storage/projects` previously turned a must-block commit into a silent
    /// allow; it must now fail closed (and honor `AGENT_MAIL_GUARD_MODE=warn`).
    #[cfg(unix)]
    #[test]
    fn guard_plugin_archive_resolution_error_fails_closed() {
        use std::os::unix::fs::PermissionsExt;

        let Some(python) = python_executable() else {
            return;
        };
        // chmod 000 does not restrict root; the scenario is unprovable there.
        let euid = Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
        if euid.as_deref() == Some("0") {
            return;
        }

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        let storage_root = td.path().join("storage");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);

        let staged = repo_dir.join("src").join("main.rs");
        std::fs::create_dir_all(staged.parent().expect("src dir")).expect("mkdir src");
        std::fs::write(&staged, "fn main() {}\n").expect("write staged file");
        run_git(&repo_dir, &["add", "src/main.rs"]);

        let repo_identity =
            mcp_agent_mail_core::resolve_project_identity(&repo_dir.to_string_lossy());
        let archive_root = storage_root.join("projects").join(&repo_identity.slug);
        let reservations_dir = archive_root.join("file_reservations");
        std::fs::create_dir_all(&reservations_dir).expect("mkdir reservations");
        std::fs::write(
            archive_root.join("project.json"),
            serde_json::json!({
                "slug": repo_identity.slug,
                "human_key": repo_dir.to_string_lossy(),
            })
            .to_string(),
        )
        .expect("write metadata");
        std::fs::write(
            reservations_dir.join("conflict.json"),
            serde_json::json!({
                "path_pattern": "src/main.rs",
                "agent_name": "OtherAgent",
                "exclusive": true,
                "expires_ts": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                "released_ts": serde_json::Value::Null,
            })
            .to_string(),
        )
        .expect("write reservation");

        let script_path = td.path().join("guard.py");
        std::fs::write(
            &script_path,
            render_guard_plugin_script(&repo_dir.to_string_lossy(), "pre-commit"),
        )
        .expect("write guard script");

        // Make the projects dir uninspectable — resolution now ERRORS rather
        // than proving "no match".
        let projects_dir = storage_root.join("projects");
        let saved_mode = std::fs::metadata(&projects_dir)
            .expect("projects metadata")
            .permissions();
        std::fs::set_permissions(&projects_dir, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000 projects");

        let run = |mode: Option<&str>| {
            let mut cmd = Command::new(&python);
            cmd.current_dir(&repo_dir)
                .env("AGENT_NAME", "PinkStone")
                .env("STORAGE_ROOT", &storage_root)
                .arg(&script_path);
            if let Some(mode) = mode {
                cmd.env("AGENT_MAIL_GUARD_MODE", mode);
            }
            cmd.output().expect("run guard script")
        };

        let blocked = run(None);
        let warned = run(Some("warn"));

        // Restore permissions BEFORE asserting so tempdir cleanup always works.
        std::fs::set_permissions(&projects_dir, saved_mode).expect("restore projects perms");

        assert_eq!(
            blocked.status.code(),
            Some(2),
            "resolution errors must fail closed: stdout={}, stderr={}",
            String::from_utf8_lossy(&blocked.stdout),
            String::from_utf8_lossy(&blocked.stderr),
        );
        let blocked_stderr = String::from_utf8_lossy(&blocked.stderr);
        assert!(
            blocked_stderr.contains("could not resolve the archive")
                && blocked_stderr.contains("AGENT_MAIL_BYPASS=1"),
            "errored outcome must explain itself and advertise the escape hatch: {blocked_stderr}",
        );

        assert_eq!(
            warned.status.code(),
            Some(0),
            "AGENT_MAIL_GUARD_MODE=warn must allow with a warning: stdout={}, stderr={}",
            String::from_utf8_lossy(&warned.stdout),
            String::from_utf8_lossy(&warned.stderr),
        );
        assert!(
            String::from_utf8_lossy(&warned.stderr).contains("WARNING"),
            "warn mode must still surface the degraded protection",
        );
    }

    #[test]
    fn guard_plugin_pre_commit_fails_closed_when_git_is_unavailable() {
        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        std::fs::write(repo_dir.join("tracked.rs"), "fn main() {}\n").expect("write tracked");
        run_git(&repo_dir, &["add", "tracked.rs"]);

        let script_path = td.path().join("guard_pre_commit.py");
        std::fs::write(
            &script_path,
            render_guard_plugin_script(&repo_dir.to_string_lossy(), "pre-commit"),
        )
        .expect("write guard script");

        let output = Command::new(&python)
            .current_dir(&repo_dir)
            .env("AGENT_NAME", "PinkStone")
            .env("PATH", "")
            .arg(&script_path)
            .output()
            .expect("run guard script");

        assert_eq!(
            output.status.code(),
            Some(2),
            "guard should fail closed when git is unavailable for staged-file inspection: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("failed to inspect staged files"),
            "expected staged-file inspection failure, got stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn guard_plugin_pre_push_fails_closed_when_git_is_unavailable() {
        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);
        std::fs::write(repo_dir.join("tracked.rs"), "fn main() {}\n").expect("write tracked");
        run_git(&repo_dir, &["add", "tracked.rs"]);
        run_git(&repo_dir, &["commit", "-qm", "init"]);
        let head = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        let script_path = td.path().join("guard_pre_push.py");
        std::fs::write(
            &script_path,
            render_guard_plugin_script(&repo_dir.to_string_lossy(), "pre-push"),
        )
        .expect("write guard script");

        let mut child = Command::new(&python)
            .current_dir(&repo_dir)
            .env("AGENT_NAME", "PinkStone")
            .env("PATH", "")
            .arg(&script_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn guard script");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(
                format!(
                    "refs/heads/main {head} refs/heads/main 0000000000000000000000000000000000000000\n"
                )
                .as_bytes(),
            )
            .expect("write stdin");
        let output = child.wait_with_output().expect("wait output");

        assert_eq!(
            output.status.code(),
            Some(2),
            "guard should fail closed when git is unavailable for push inspection: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("failed to inspect push files")
                || String::from_utf8_lossy(&output.stderr)
                    .contains("failed to enumerate pushed commits"),
            "expected push inspection failure, got stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn guard_plugin_pre_push_blocks_initial_push_root_commit_conflict() {
        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);
        std::fs::write(repo_dir.join("tracked.rs"), "fn main() {}\n").expect("write tracked");
        run_git(&repo_dir, &["add", "tracked.rs"]);
        run_git(&repo_dir, &["commit", "-qm", "initial"]);
        let head = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        let reservations_dir = repo_dir.join("file_reservations");
        std::fs::create_dir_all(&reservations_dir).expect("mkdir reservations");
        std::fs::write(
            reservations_dir.join("conflict.json"),
            serde_json::json!({
                "path_pattern": "tracked.rs",
                "agent_name": "OtherAgent",
                "exclusive": true,
                "expires_ts": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                "released_ts": serde_json::Value::Null,
            })
            .to_string(),
        )
        .expect("write reservation");

        let script_path = td.path().join("guard_pre_push_initial.py");
        std::fs::write(
            &script_path,
            render_guard_plugin_script(&repo_dir.to_string_lossy(), "pre-push"),
        )
        .expect("write guard script");

        let mut child = Command::new(&python)
            .current_dir(&repo_dir)
            .env("AGENT_NAME", "PinkStone")
            .arg(&script_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn guard script");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(
                format!(
                    "refs/heads/main {head} refs/heads/main 0000000000000000000000000000000000000000\n"
                )
                .as_bytes(),
            )
            .expect("write stdin");
        let output = child.wait_with_output().expect("wait output");

        assert_eq!(
            output.status.code(),
            Some(1),
            "guard should block initial-push root-commit conflicts: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("tracked.rs"),
            "expected tracked.rs conflict in stderr, got stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// Discover every distinct Python interpreter available on this machine so
    /// the guard-matching e2e test can exercise the generated script under each
    /// one. This is what catches the `CPython` 3.14 `fnmatch.translate` fail-open
    /// regression: `python_executable()` above only returns the first `python3`
    /// (often 3.13), whereas the guard must remain correct on 3.9-3.14+.
    fn python_executables() -> Vec<String> {
        let candidates = [
            "python3.15",
            "python3.14",
            "python3.13",
            "python3.12",
            "python3.11",
            "python3.10",
            "python3.9",
            "python3",
            "python",
        ];
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for candidate in candidates {
            let Ok(output) = Command::new(candidate)
                .args(["-c", "import sys; print(sys.executable)"])
                .output()
            else {
                continue;
            };
            if !output.status.success() {
                continue;
            }
            let executable = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !executable.is_empty() && seen.insert(executable.clone()) {
                out.push(executable);
            }
        }
        out
    }

    /// The generated guard must NOT depend on `fnmatch.translate`'s output
    /// format (a `CPython` implementation detail that changed in 3.14 and broke
    /// the guard's regex surgery, causing a security-relevant fail-open). This
    /// unit test runs without Python and locks in the structural fix.
    #[test]
    fn guard_plugin_glob_matcher_is_fnmatch_translate_free() {
        for hook in ["pre-commit", "pre-push"] {
            let script = render_guard_plugin_script("/abs/project", hook);
            assert!(
                !script.contains("fnmatch.translate("),
                "guard {hook} must not call fnmatch.translate (unstable across Python versions)"
            );
            assert!(
                !script.contains("\nimport fnmatch\n"),
                "guard {hook} must not import fnmatch (no longer used)"
            );
            assert!(
                script.contains("def _glob_translate_body(pattern):"),
                "guard {hook} must use the version-agnostic glob compiler"
            );
            // The fail-open swallow (`except re.error: return False`) must be gone;
            // an uncompilable pattern must be treated conservatively as a conflict.
            assert!(
                script.contains("treating as a conflict (fail-closed)"),
                "guard {hook} must fail closed when a reservation pattern cannot be evaluated"
            );
        }
    }

    /// End-to-end proof that glob reservations are enforced (and evaluated
    /// correctly) under EVERY installed Python interpreter — most importantly
    /// Python 3.14, where the previous `fnmatch.translate`-based matcher produced
    /// an uncompilable regex, silently matched nothing, and let conflicting
    /// commits through (fail-open). A non-holder must be blocked (exit 1) and the
    /// holder of the same reservation must pass (exit 0), which proves the
    /// matcher genuinely evaluates the glob rather than merely erroring.
    #[test]
    fn guard_plugin_glob_reservation_blocks_pre_commit_across_python_versions() {
        let pythons = python_executables();
        if pythons.is_empty() {
            return;
        }

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        // Stage a nested file that is only reachable via a **/*.rs globstar.
        let staged = repo_dir.join("src").join("app").join("main.rs");
        std::fs::create_dir_all(staged.parent().expect("src dir")).expect("mkdir src");
        std::fs::write(&staged, "fn main() {}\n").expect("write staged file");
        run_git(&repo_dir, &["add", "src/app/main.rs"]);

        // Reservation uses a glob pattern (the exact class of pattern the 3.14
        // regression silently disabled).
        let reservations_dir = repo_dir.join("file_reservations");
        std::fs::create_dir_all(&reservations_dir).expect("mkdir reservations");
        std::fs::write(
            reservations_dir.join("conflict.json"),
            serde_json::json!({
                "path_pattern": "**/*.rs",
                "agent_name": "OtherAgent",
                "exclusive": true,
                "expires_ts": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                "released_ts": serde_json::Value::Null,
            })
            .to_string(),
        )
        .expect("write reservation");

        let script_path = td.path().join("guard_glob.py");
        std::fs::write(
            &script_path,
            render_guard_plugin_script(&repo_dir.to_string_lossy(), "pre-commit"),
        )
        .expect("write guard script");

        for python in &pythons {
            // Sanity: the generated script must be syntactically valid Python.
            let compile = Command::new(python)
                .args(["-m", "py_compile"])
                .arg(&script_path)
                .output()
                .expect("run py_compile");
            assert!(
                compile.status.success(),
                "generated guard is not valid Python under {python}: stderr={}",
                String::from_utf8_lossy(&compile.stderr),
            );

            // A different agent MUST be blocked: the glob conflict is detected.
            let blocked = Command::new(python)
                .current_dir(&repo_dir)
                .env("AGENT_NAME", "PinkStone")
                .arg(&script_path)
                .output()
                .expect("run guard script (non-holder)");
            assert_eq!(
                blocked.status.code(),
                Some(1),
                "guard under {python} must block a glob-reserved path for a non-holder \
                 (fail-open regression if it does not): stdout={}, stderr={}",
                String::from_utf8_lossy(&blocked.stdout),
                String::from_utf8_lossy(&blocked.stderr),
            );
            assert!(
                String::from_utf8_lossy(&blocked.stderr).contains("main.rs"),
                "guard under {python} should name the conflicting path: stdout={}, stderr={}",
                String::from_utf8_lossy(&blocked.stdout),
                String::from_utf8_lossy(&blocked.stderr),
            );

            // The holder of the same reservation MUST pass: this proves the glob
            // is genuinely evaluated (not merely erroring / always-blocking).
            let allowed = Command::new(python)
                .current_dir(&repo_dir)
                .env("AGENT_NAME", "OtherAgent")
                .arg(&script_path)
                .output()
                .expect("run guard script (holder)");
            assert_eq!(
                allowed.status.code(),
                Some(0),
                "guard under {python} must let the reservation holder commit: stdout={}, stderr={}",
                String::from_utf8_lossy(&allowed.stdout),
                String::from_utf8_lossy(&allowed.stderr),
            );
        }
    }

    /// GH#224: a repo that matches no agent-mail project has nothing to
    /// guard — the plugin must allow the commit (exit 0) instead of failing
    /// closed with exit 2.
    #[test]
    fn guard_plugin_allows_commit_in_repo_matching_no_project() {
        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("unrelated-repo");
        let storage_root = td.path().join("storage");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        std::fs::write(repo_dir.join("notes.md"), "hello\n").expect("write file");
        run_git(&repo_dir, &["add", "notes.md"]);

        // A populated storage root that contains only an UNRELATED project.
        let other_archive = storage_root.join("projects").join("some-other-project");
        std::fs::create_dir_all(other_archive.join("file_reservations")).expect("mkdir archive");
        std::fs::write(
            other_archive.join("project.json"),
            serde_json::json!({
                "slug": "some-other-project",
                "human_key": "/somewhere/else/entirely",
            })
            .to_string(),
        )
        .expect("write metadata");

        let script_path = td.path().join("guard.py");
        std::fs::write(
            &script_path,
            render_guard_plugin_script("/an/uninstalled/project", "pre-commit"),
        )
        .expect("write guard script");

        let output = Command::new(&python)
            .current_dir(&repo_dir)
            .env("AGENT_NAME", "PinkStone")
            .env("STORAGE_ROOT", &storage_root)
            .arg(&script_path)
            .output()
            .expect("run guard script");

        assert_eq!(
            output.status.code(),
            Some(0),
            "guard must fail open when no project matches this repo: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("WARNING: mcp-agent-mail: no agent-mail archive"),
            "expected a visible nothing-to-guard note, got stderr={}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// GH#224: `AGENT_MAIL_BYPASS` must work even when `AGENT_NAME` is unset
    /// (previously the `AGENT_NAME` requirement exited 2 before the bypass was
    /// consulted).
    #[test]
    fn guard_plugin_bypass_works_without_agent_name() {
        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        std::fs::write(repo_dir.join("a.rs"), "fn a() {}\n").expect("write file");
        run_git(&repo_dir, &["add", "a.rs"]);

        let script_path = td.path().join("guard.py");
        std::fs::write(
            &script_path,
            render_guard_plugin_script(&repo_dir.to_string_lossy(), "pre-commit"),
        )
        .expect("write guard script");

        let output = Command::new(&python)
            .current_dir(&repo_dir)
            .env_remove("AGENT_NAME")
            .env("AGENT_MAIL_BYPASS", "1")
            .arg(&script_path)
            .output()
            .expect("run guard script");

        assert_eq!(
            output.status.code(),
            Some(0),
            "AGENT_MAIL_BYPASS=1 must short-circuit before the AGENT_NAME requirement: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// When active reservations exist and neither `AGENT_NAME` nor a pane
    /// identity is available, the guard stays fail-closed in block mode
    /// (exit 2, naming the bypass) but honors warn mode (exit 0).
    #[test]
    fn guard_plugin_missing_agent_name_with_reservations_honors_warn_mode() {
        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        let storage_root = td.path().join("storage");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        std::fs::write(repo_dir.join("a.rs"), "fn a() {}\n").expect("write file");
        run_git(&repo_dir, &["add", "a.rs"]);

        let identity = mcp_agent_mail_core::resolve_project_identity(&repo_dir.to_string_lossy());
        let archive_root = storage_root.join("projects").join(&identity.slug);
        let reservations_dir = archive_root.join("file_reservations");
        std::fs::create_dir_all(&reservations_dir).expect("mkdir reservations");
        std::fs::write(
            archive_root.join("project.json"),
            serde_json::json!({
                "slug": identity.slug,
                "human_key": repo_dir.to_string_lossy(),
            })
            .to_string(),
        )
        .expect("write metadata");
        std::fs::write(
            reservations_dir.join("res.json"),
            serde_json::json!({
                "path_pattern": "a.rs",
                "agent_name": "OtherAgent",
                "exclusive": true,
                "expires_ts": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                "released_ts": serde_json::Value::Null,
            })
            .to_string(),
        )
        .expect("write reservation");

        let script_path = td.path().join("guard.py");
        std::fs::write(
            &script_path,
            render_guard_plugin_script(&repo_dir.to_string_lossy(), "pre-commit"),
        )
        .expect("write guard script");

        // Block mode (default): fail closed, and the failure must advertise
        // the bypass escape hatch.
        let blocked = Command::new(&python)
            .current_dir(&repo_dir)
            .env_remove("AGENT_NAME")
            .env("STORAGE_ROOT", &storage_root)
            .arg(&script_path)
            .output()
            .expect("run guard script (block)");
        assert_eq!(
            blocked.status.code(),
            Some(2),
            "missing AGENT_NAME with active reservations must fail closed: stdout={}, stderr={}",
            String::from_utf8_lossy(&blocked.stdout),
            String::from_utf8_lossy(&blocked.stderr),
        );
        let stderr = String::from_utf8_lossy(&blocked.stderr);
        assert!(
            stderr.contains("AGENT_NAME is unset and no current-pane identity"),
            "expected AGENT_NAME requirement, got stderr={stderr}",
        );
        assert!(
            stderr.contains("AGENT_MAIL_BYPASS=1"),
            "fail-closed output must advertise the bypass, got stderr={stderr}",
        );

        // Warn mode: same situation must allow with a warning.
        let warned = Command::new(&python)
            .current_dir(&repo_dir)
            .env_remove("AGENT_NAME")
            .env("STORAGE_ROOT", &storage_root)
            .env("AGENT_MAIL_GUARD_MODE", "warn")
            .arg(&script_path)
            .output()
            .expect("run guard script (warn)");
        assert_eq!(
            warned.status.code(),
            Some(0),
            "AGENT_MAIL_GUARD_MODE=warn must soften infrastructure fail-closed paths: stdout={}, stderr={}",
            String::from_utf8_lossy(&warned.stdout),
            String::from_utf8_lossy(&warned.stderr),
        );
        assert!(
            String::from_utf8_lossy(&warned.stderr).contains("WARNING"),
            "warn mode must still warn, got stderr={}",
            String::from_utf8_lossy(&warned.stderr),
        );
    }

    #[test]
    fn guard_plugin_resolves_legacy_pane_identity_when_agent_name_is_unset() {
        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        let storage_root = td.path().join("storage");
        let home_dir = td.path().join("home");
        let pane_id = "%guard-identity";
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        std::fs::write(repo_dir.join("a.rs"), "fn a() {}\n").expect("write file");
        run_git(&repo_dir, &["add", "a.rs"]);

        let identity = mcp_agent_mail_core::resolve_project_identity(&repo_dir.to_string_lossy());
        let archive_root = storage_root.join("projects").join(&identity.slug);
        let reservations_dir = archive_root.join("file_reservations");
        std::fs::create_dir_all(&reservations_dir).expect("mkdir reservations");
        std::fs::write(
            archive_root.join("project.json"),
            serde_json::json!({
                "slug": identity.slug,
                "human_key": repo_dir.to_string_lossy(),
            })
            .to_string(),
        )
        .expect("write metadata");
        std::fs::write(
            reservations_dir.join("res.json"),
            serde_json::json!({
                "path_pattern": "a.rs",
                "agent_name": "PaneAgent",
                "exclusive": true,
                "expires_ts": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                "released_ts": serde_json::Value::Null,
            })
            .to_string(),
        )
        .expect("write reservation");

        let legacy_identity = home_dir
            .join(".claude")
            .join("agent-mail")
            .join("identity.guard-identity");
        std::fs::create_dir_all(legacy_identity.parent().expect("identity parent"))
            .expect("create identity parent");
        std::fs::write(&legacy_identity, "PaneAgent\n").expect("write identity");

        let script_path = td.path().join("guard.py");
        std::fs::write(
            &script_path,
            render_guard_plugin_script(&repo_dir.to_string_lossy(), "pre-commit"),
        )
        .expect("write guard script");

        let output = Command::new(&python)
            .current_dir(&repo_dir)
            .env_remove("AGENT_NAME")
            .env("HOME", &home_dir)
            .env("TMUX_PANE", pane_id)
            .env("STORAGE_ROOT", &storage_root)
            .arg(&script_path)
            .output()
            .expect("run guard script");

        assert_eq!(
            output.status.code(),
            Some(0),
            "guard must resolve the pane owner before checking its own reservation: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn guard_plugin_exempts_beads_metadata_reserved_by_another_agent() {
        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        let storage_root = td.path().join("storage");
        std::fs::create_dir_all(repo_dir.join(".beads")).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        std::fs::write(repo_dir.join(".beads/issues.jsonl"), "{}\n").expect("write beads");
        run_git(&repo_dir, &["add", ".beads/issues.jsonl"]);

        let identity = mcp_agent_mail_core::resolve_project_identity(&repo_dir.to_string_lossy());
        let archive_root = storage_root.join("projects").join(&identity.slug);
        let reservations_dir = archive_root.join("file_reservations");
        std::fs::create_dir_all(&reservations_dir).expect("mkdir reservations");
        std::fs::write(
            archive_root.join("project.json"),
            serde_json::json!({
                "slug": identity.slug,
                "human_key": repo_dir.to_string_lossy(),
            })
            .to_string(),
        )
        .expect("write metadata");
        std::fs::write(
            reservations_dir.join("res.json"),
            serde_json::json!({
                "path_pattern": ".beads/**",
                "agent_name": "OtherAgent",
                "exclusive": true,
                "expires_ts": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                "released_ts": serde_json::Value::Null,
            })
            .to_string(),
        )
        .expect("write reservation");

        let script_path = td.path().join("guard.py");
        std::fs::write(
            &script_path,
            render_guard_plugin_script(&repo_dir.to_string_lossy(), "pre-commit"),
        )
        .expect("write guard script");

        let output = Command::new(&python)
            .current_dir(&repo_dir)
            .env("AGENT_NAME", "Committer")
            .env("STORAGE_ROOT", &storage_root)
            .arg(&script_path)
            .output()
            .expect("run guard script");

        assert_eq!(
            output.status.code(),
            Some(0),
            "guard must exempt shared Beads metadata: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// GH#223: installation must honor a repo-local core.hooksPath but refuse
    /// machine-wide (global/system/XDG) values by default.
    #[test]
    fn install_hooks_dir_honors_repo_local_hookspath() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(
            &repo_dir,
            &["config", "--local", "core.hooksPath", ".husky"],
        );

        let hooks = resolve_install_hooks_dir(&repo_dir).expect("resolve install hooks dir");
        assert_eq!(hooks, repo_dir.join(".husky"));
    }

    #[test]
    fn install_hooks_dir_defaults_to_git_hooks() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);

        let hooks = resolve_install_hooks_dir(&repo_dir).expect("resolve install hooks dir");
        let canon_hooks = hooks.canonicalize().unwrap_or(hooks);
        let expected = repo_dir.join(".git").join("hooks");
        let canon_expected = expected.canonicalize().unwrap_or(expected);
        assert_eq!(canon_hooks, canon_expected);
    }

    #[test]
    fn hookspath_level_refusal_matrix() {
        use git2::ConfigLevel;
        // Machine-wide levels refuse by default…
        assert!(hookspath_level_refuses_install(ConfigLevel::Global, false));
        assert!(hookspath_level_refuses_install(ConfigLevel::System, false));
        assert!(hookspath_level_refuses_install(ConfigLevel::XDG, false));
        // …but are honored under the explicit opt-in.
        assert!(!hookspath_level_refuses_install(ConfigLevel::Global, true));
        // Repo-scoped levels are always honored.
        assert!(!hookspath_level_refuses_install(ConfigLevel::Local, false));
        assert!(!hookspath_level_refuses_install(
            ConfigLevel::Worktree,
            false
        ));
        assert!(!hookspath_level_refuses_install(ConfigLevel::App, false));
    }

    /// Regression: `resolve_archive_root`'s explicit-name fast path (a
    /// slug-style PROJECT whose `projects/<slug>/file_reservations` archive
    /// exists) must return the `(archive_root, suspicious, errors)` tuple like
    /// every other return site. It previously returned the bare path, so the
    /// caller's tuple unpack raised `ValueError` — an unhandled traceback
    /// (exit 1) that blocked every commit on the *healthy* happy path, and
    /// that `AGENT_MAIL_GUARD_MODE=warn` could not soften.
    #[test]
    fn guard_plugin_explicit_slug_archive_match_does_not_crash() {
        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        let storage_root = td.path().join("storage");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        std::fs::write(repo_dir.join("a.rs"), "fn a() {}\n").expect("write file");
        run_git(&repo_dir, &["add", "a.rs"]);

        // Slug-registered project: archive dir named exactly after the
        // slug-style PROJECT baked into the plugin (the explicit-name fast
        // path — no project.json needed to match).
        let reservations_dir = storage_root
            .join("projects")
            .join("myproj")
            .join("file_reservations");
        std::fs::create_dir_all(&reservations_dir).expect("mkdir reservations");

        let script_path = td.path().join("guard.py");
        std::fs::write(
            &script_path,
            render_guard_plugin_script("myproj", "pre-commit"),
        )
        .expect("write guard script");

        // No active reservations: the archive matches and the commit must be
        // allowed (exit 0). Before the fix this crashed with a ValueError
        // traceback and exit 1.
        let output = Command::new(&python)
            .current_dir(&repo_dir)
            .env("AGENT_NAME", "PinkStone")
            .env("STORAGE_ROOT", &storage_root)
            .arg(&script_path)
            .output()
            .expect("run guard script");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("Traceback"),
            "guard plugin must not crash on the explicit-slug archive match: stderr={stderr}",
        );
        assert_eq!(
            output.status.code(),
            Some(0),
            "matching archive with no active reservations must allow: stdout={}, stderr={stderr}",
            String::from_utf8_lossy(&output.stdout),
        );

        // With an active exclusive reservation held by another agent, the
        // same fast path must produce the real conflict block (exit 1 with
        // the conflict message), not a crash.
        std::fs::write(
            reservations_dir.join("res.json"),
            serde_json::json!({
                "path_pattern": "a.rs",
                "agent_name": "OtherAgent",
                "exclusive": true,
                "expires_ts": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                "released_ts": serde_json::Value::Null,
            })
            .to_string(),
        )
        .expect("write reservation");

        let blocked = Command::new(&python)
            .current_dir(&repo_dir)
            .env("AGENT_NAME", "PinkStone")
            .env("STORAGE_ROOT", &storage_root)
            .arg(&script_path)
            .output()
            .expect("run guard script (conflict)");
        let blocked_stderr = String::from_utf8_lossy(&blocked.stderr);
        assert!(
            !blocked_stderr.contains("Traceback"),
            "conflict path must not crash: stderr={blocked_stderr}",
        );
        assert_eq!(
            blocked.status.code(),
            Some(1),
            "conflicting reservation via the explicit-slug fast path must block: stdout={}, stderr={blocked_stderr}",
            String::from_utf8_lossy(&blocked.stdout),
        );
        assert!(
            blocked_stderr.contains("file reservation conflict"),
            "expected the real conflict message, got stderr={blocked_stderr}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn chain_runner_propagates_real_guard_conflict_exit_status() {
        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        let hooks_dir = td.path().join("hooks");
        let run_dir = hooks_dir.join("hooks.d").join("pre-commit");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        std::fs::create_dir_all(&run_dir).expect("mkdir hook plugins");
        run_git(&repo_dir, &["init", "-q"]);
        std::fs::write(repo_dir.join("a.rs"), "fn a() {}\n").expect("write file");
        run_git(&repo_dir, &["add", "a.rs"]);

        let reservations_dir = repo_dir.join("file_reservations");
        std::fs::create_dir_all(&reservations_dir).expect("mkdir reservations");
        std::fs::write(
            reservations_dir.join("res.json"),
            serde_json::json!({
                "path_pattern": "a.rs",
                "agent_name": "OtherAgent",
                "exclusive": true,
                "expires_ts": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                "released_ts": serde_json::Value::Null,
            })
            .to_string(),
        )
        .expect("write reservation");

        let chain_path = hooks_dir.join("pre-commit");
        write_guard_file_atomic(&chain_path, &render_chain_runner_script("pre-commit"), true)
            .expect("write chain runner");
        write_guard_file_atomic(
            &run_dir.join(PLUGIN_FILE_NAME),
            &render_guard_plugin_script(&repo_dir.to_string_lossy(), "pre-commit"),
            true,
        )
        .expect("write guard plugin");

        let output = Command::new(&python)
            .current_dir(&repo_dir)
            .env("AGENT_NAME", "Committer")
            .arg(&chain_path)
            .output()
            .expect("run chain runner");

        assert_eq!(
            output.status.code(),
            Some(1),
            "the chain runner must preserve a blocking guard's exit code: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("file reservation conflict"),
            "expected the guard conflict in stderr"
        );
    }

    #[cfg(unix)]
    #[test]
    fn chain_runner_runs_later_pre_commit_hooks_after_guard_failure() {
        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        let marker = td.path().join("marker.txt");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        std::fs::write(repo_dir.join("a.rs"), "fn a() {}\n").expect("write staged file");
        run_git(&repo_dir, &["add", "a.rs"]);
        install_guard(&repo_dir.to_string_lossy(), &repo_dir, false).expect("install guard");

        // Identity is required only when a staged path is actually covered by
        // a live reservation. Keep this fixture concrete: a missing archive
        // is intentionally warning-and-allow, while this peer reservation
        // must fail closed with the guard's identity exit code.
        let reservations_dir = repo_dir.join("file_reservations");
        std::fs::create_dir_all(&reservations_dir).expect("mkdir reservations");
        std::fs::write(
            reservations_dir.join("peer.json"),
            serde_json::json!({
                "path_pattern": "a.rs",
                "agent_name": "OtherAgent",
                "exclusive": true,
                "expires_ts": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                "released_ts": serde_json::Value::Null,
            })
            .to_string(),
        )
        .expect("write active reservation");

        let hooks_dir = resolve_hooks_dir(&repo_dir).expect("hooks dir");
        write_guard_file_atomic(
            &hooks_dir
                .join("hooks.d")
                .join("pre-commit")
                .join("60-later"),
            "#!/bin/sh\nprintf 'later\\n' >> \"$CHAIN_MARKER\"\n",
            true,
        )
        .expect("write later hook");
        write_guard_file_atomic(
            &hooks_dir.join("pre-commit.orig"),
            "#!/bin/sh\nprintf 'orig\\n' >> \"$CHAIN_MARKER\"\n",
            true,
        )
        .expect("write original hook");

        let output = Command::new(&python)
            .current_dir(&repo_dir)
            .env_remove("TMUX_PANE")
            .env_remove("AGENT_NAME")
            .env("CHAIN_MARKER", &marker)
            .arg(hooks_dir.join("pre-commit"))
            .output()
            .expect("run pre-commit chain");

        assert_eq!(
            output.status.code(),
            Some(2),
            "chain must preserve the Agent Mail identity failure: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read marker"),
            "later\norig\n",
            "later and original hooks must run in order after a guard failure"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(PLUGIN_FILE_NAME),
            "chain should identify the failed plugin: stderr={}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[cfg(unix)]
    #[test]
    fn pre_push_chain_forwards_identical_stdin_after_guard_failure() {
        use std::io::Write;

        let Some(python) = python_executable() else {
            return;
        };

        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        let later_stdin = td.path().join("later.stdin");
        let original_stdin = td.path().join("original.stdin");
        let marker = td.path().join("marker.txt");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        run_git(&repo_dir, &["init", "-q"]);
        install_guard(&repo_dir.to_string_lossy(), &repo_dir, true).expect("install guard");

        let hooks_dir = resolve_hooks_dir(&repo_dir).expect("hooks dir");
        write_guard_file_atomic(
            &hooks_dir
                .join("hooks.d")
                .join("pre-push")
                .join("60-later"),
            "#!/bin/sh\ncat > \"$CHAIN_LATER_STDIN\"\nprintf 'later\\n' >> \"$CHAIN_MARKER\"\nexit 9\n",
            true,
        )
        .expect("write later hook");
        write_guard_file_atomic(
            &hooks_dir.join("pre-push.orig"),
            "#!/bin/sh\ncat > \"$CHAIN_ORIGINAL_STDIN\"\nprintf 'orig\\n' >> \"$CHAIN_MARKER\"\n",
            true,
        )
        .expect("write original hook");

        let ref_updates = b"refs/heads/main aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
refs/heads/main bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
refs/heads/topic cccccccccccccccccccccccccccccccccccccccc \
refs/heads/topic dddddddddddddddddddddddddddddddddddddddd\n";
        let mut child = Command::new(&python)
            .current_dir(&repo_dir)
            .env_remove("TMUX_PANE")
            .env_remove("AGENT_NAME")
            .env("CHAIN_LATER_STDIN", &later_stdin)
            .env("CHAIN_ORIGINAL_STDIN", &original_stdin)
            .env("CHAIN_MARKER", &marker)
            .arg(hooks_dir.join("pre-push"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn pre-push chain");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(ref_updates)
            .expect("write stdin");
        let output = child.wait_with_output().expect("wait output");

        assert_eq!(
            output.status.code(),
            Some(2),
            "chain must preserve the Agent Mail identity failure: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            std::fs::read(&later_stdin).expect("read later stdin"),
            ref_updates
        );
        assert_eq!(
            std::fs::read(&original_stdin).expect("read original stdin"),
            ref_updates
        );
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read marker"),
            "later\norig\n",
            "later and original hooks must run in order after a guard failure"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("{PLUGIN_FILE_NAME} exited with status 2"))
                && stderr.contains("60-later exited with status 9"),
            "chain should report every failed plugin while returning the first status: stderr={stderr}"
        );
    }
}
