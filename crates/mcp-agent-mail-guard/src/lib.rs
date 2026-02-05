#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("not implemented")]
    NotImplemented,
    #[error("invalid repository path: {path}")]
    InvalidRepo { path: String },
    #[error("missing AGENT_NAME env var")]
    MissingAgentName,
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
    if let Some(p) = std::env::var_os("HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }

    // Windows fallbacks (best-effort; tests run on Linux, but keep portable).
    if let Some(p) = std::env::var_os("USERPROFILE") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
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
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
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

const PLUGIN_FILE_NAME: &str = "50-agent-mail.py";

#[cfg(unix)]
fn chmod_exec(path: &Path) -> GuardResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn chmod_exec(_path: &Path) -> GuardResult<()> {
    Ok(())
}

fn is_legacy_single_file_guard(contents: &str) -> bool {
    // Legacy (pre-chain-runner) guard installs used a single hook file.
    // Keep this detection permissive and sentinel-based.
    contents.contains("mcp-agent-mail guard hook")
        || contents.contains("AGENT_NAME environment variable is required.")
}

fn render_chain_runner_script(hook_name: &str) -> String {
    // Mirrors legacy behavior: run hooks.d/<hook>/* in lexical order; forward stdin for pre-push.
    let mut lines: Vec<String> = vec![
        "#!/usr/bin/env python3".to_string(),
        format!("# mcp-agent-mail chain-runner ({hook_name})"),
        "import os".to_string(),
        "import sys".to_string(),
        "import stat".to_string(),
        "import subprocess".to_string(),
        "from pathlib import Path".to_string(),
        "".to_string(),
        "HOOK_DIR = Path(__file__).parent".to_string(),
        format!("RUN_DIR = HOOK_DIR / 'hooks.d' / '{hook_name}'"),
        format!("ORIG = HOOK_DIR / '{hook_name}.orig'"),
        "".to_string(),
        "def _is_exec(p: Path) -> bool:".to_string(),
        "    try:".to_string(),
        "        st = p.stat()".to_string(),
        "        return bool(st.st_mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH))"
            .to_string(),
        "    except Exception:".to_string(),
        "        return False".to_string(),
        "".to_string(),
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
        "".to_string(),
        "def _run_child(path: Path, * , stdin_bytes=None):".to_string(),
        "    # On Windows, prefer 'python' for .py plugins to avoid PATHEXT reliance.".to_string(),
        "    if os.name != 'posix' and path.suffix.lower() == '.py':".to_string(),
        "        return subprocess.run(['python', str(path)], input=stdin_bytes, check=False).returncode"
            .to_string(),
        "    return subprocess.run([str(path)], input=stdin_bytes, check=False).returncode"
            .to_string(),
        "".to_string(),
    ];

    if hook_name == "pre-push" {
        lines.extend([
            "# Read STDIN once (Git passes ref tuples); forward to children".to_string(),
            "stdin_bytes = sys.stdin.buffer.read()".to_string(),
            "for exe in _list_execs():".to_string(),
            "    rc = _run_child(exe, stdin_bytes=stdin_bytes)".to_string(),
            "    if rc != 0:".to_string(),
            "        sys.exit(rc)".to_string(),
            "".to_string(),
            "if ORIG.exists():".to_string(),
            "    rc = _run_child(ORIG, stdin_bytes=stdin_bytes)".to_string(),
            "    if rc != 0:".to_string(),
            "        sys.exit(rc)".to_string(),
            "sys.exit(0)".to_string(),
        ]);
    } else {
        lines.extend([
            "for exe in _list_execs():".to_string(),
            "    rc = _run_child(exe)".to_string(),
            "    if rc != 0:".to_string(),
            "        sys.exit(rc)".to_string(),
            "".to_string(),
            "if ORIG.exists():".to_string(),
            "    rc = _run_child(ORIG)".to_string(),
            "    if rc != 0:".to_string(),
            "        sys.exit(rc)".to_string(),
            "sys.exit(0)".to_string(),
        ]);
    }

    format!("{}\n", lines.join("\n"))
}

fn render_guard_plugin_script(project: &str, hook_name: &str) -> String {
    // Real guard plugin: checks active file reservations against staged changes.
    // Uses the `am` CLI binary to query reservations and compare against `git diff --cached`.
    format!(
        r#"#!/usr/bin/env python3
# mcp-agent-mail guard plugin ({hook_name})
# project: {project}
# Auto-generated by mcp-agent-mail install_guard

import json
import os
import subprocess
import sys
from fnmatch import fnmatch

PROJECT = "{project}"
AGENT_NAME = os.environ.get("AGENT_NAME", "")
GUARD_MODE = os.environ.get("AGENT_MAIL_GUARD_MODE", "block")

def get_staged_files():
    """Get list of staged files from git."""
    try:
        result = subprocess.run(
            ["git", "diff", "--cached", "--name-only", "--diff-filter=ACMR"],
            capture_output=True, text=True, check=True,
        )
        return [f.strip() for f in result.stdout.strip().split("\n") if f.strip()]
    except subprocess.CalledProcessError:
        return []

def get_active_reservations():
    """Query active exclusive file reservations from the database."""
    db_path = os.environ.get("AGENT_MAIL_DB", "")
    if not db_path:
        # Try default locations
        storage_root = os.environ.get("AGENT_MAIL_STORAGE_ROOT", "")
        if storage_root:
            db_path = os.path.join(storage_root, "..", "storage.sqlite3")
    if not db_path or not os.path.exists(db_path):
        return []
    try:
        import sqlite3
        conn = sqlite3.connect(db_path, timeout=5)
        conn.row_factory = sqlite3.Row
        now_micros = int(__import__("time").time() * 1_000_000)
        rows = conn.execute(
            "SELECT fr.path_pattern, fr.agent_id, fr.expires_ts, a.name as agent_name "
            "FROM file_reservations fr "
            "JOIN agents a ON a.id = fr.agent_id "
            "JOIN projects p ON p.id = fr.project_id "
            "WHERE fr.exclusive = 1 AND fr.released_ts IS NULL "
            "AND fr.expires_ts > ? AND p.human_key = ?",
            (now_micros, PROJECT),
        ).fetchall()
        conn.close()
        return [dict(r) for r in rows]
    except Exception:
        return []

def check_conflicts(staged, reservations):
    """Check if any staged files conflict with active reservations."""
    conflicts = []
    for f in staged:
        for res in reservations:
            pattern = res["path_pattern"]
            holder = res.get("agent_name", "unknown")
            if holder == AGENT_NAME:
                continue  # Skip our own reservations
            if fnmatch(f, pattern) or fnmatch(pattern, f) or f.startswith(pattern.rstrip("*")):
                conflicts.append((f, pattern, holder))
                break
    return conflicts

def main():
    if not AGENT_NAME:
        # No agent context; skip guard check
        sys.exit(0)

    staged = get_staged_files()
    if not staged:
        sys.exit(0)

    reservations = get_active_reservations()
    if not reservations:
        sys.exit(0)

    conflicts = check_conflicts(staged, reservations)
    if not conflicts:
        sys.exit(0)

    msg = "mcp-agent-mail: file reservation conflict detected!\n"
    for path, pattern, holder in conflicts:
        msg += f"  {{path}} conflicts with reservation '{{pattern}}' held by {{holder}}\n"

    if GUARD_MODE == "warn":
        print(f"WARNING: {{msg}}", file=sys.stderr)
        sys.exit(0)
    else:
        print(f"ERROR: {{msg}}", file=sys.stderr)
        print("Set AGENT_MAIL_GUARD_MODE=warn to allow commit anyway.", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
"#
    )
}

pub fn install_guard(_project: &str, repo: &Path) -> GuardResult<()> {
    if !repo.exists() {
        return Err(GuardError::InvalidRepo {
            path: repo.display().to_string(),
        });
    }

    let hooks_dir = resolve_hooks_dir(repo)?;
    std::fs::create_dir_all(&hooks_dir)?;

    // Ensure hooks.d/pre-commit exists.
    let run_dir = hooks_dir.join("hooks.d").join("pre-commit");
    std::fs::create_dir_all(&run_dir)?;

    let chain_path = hooks_dir.join("pre-commit");
    if chain_path.exists() {
        let content = std::fs::read_to_string(&chain_path).unwrap_or_default();
        let content = content.trim();
        if !content.contains("mcp-agent-mail chain-runner (pre-commit)") {
            let orig = hooks_dir.join("pre-commit.orig");
            if !orig.exists() {
                std::fs::rename(&chain_path, &orig)?;
            }
        }
    }

    // Write/overwrite chain-runner.
    let chain_script = render_chain_runner_script("pre-commit");
    std::fs::write(&chain_path, chain_script)?;
    chmod_exec(&chain_path)?;

    // Windows shims (.cmd/.ps1) to invoke the Python chain-runner.
    let cmd_path = hooks_dir.join("pre-commit.cmd");
    if !cmd_path.exists() {
        let body = "@echo off\r\nsetlocal\r\nset \"DIR=%~dp0\"\r\npython \"%DIR%pre-commit\" %*\r\nexit /b %ERRORLEVEL%\r\n";
        std::fs::write(&cmd_path, body)?;
    }
    let ps1_path = hooks_dir.join("pre-commit.ps1");
    if !ps1_path.exists() {
        let body = "$ErrorActionPreference = 'Stop'\n$hook = Join-Path $PSScriptRoot 'pre-commit'\npython $hook @args\nexit $LASTEXITCODE\n";
        std::fs::write(&ps1_path, body)?;
    }

    // Write our guard plugin (currently stubbed).
    let plugin_path = run_dir.join(PLUGIN_FILE_NAME);
    std::fs::write(
        &plugin_path,
        render_guard_plugin_script(_project, "pre-commit"),
    )?;
    chmod_exec(&plugin_path)?;

    Ok(())
}

pub fn uninstall_guard(repo: &Path) -> GuardResult<()> {
    if !repo.exists() {
        return Err(GuardError::InvalidRepo {
            path: repo.display().to_string(),
        });
    }

    let hooks_dir = resolve_hooks_dir(repo)?;

    fn has_other_plugins(run_dir: &Path) -> bool {
        let Ok(rd) = std::fs::read_dir(run_dir) else {
            return false;
        };
        rd.filter_map(Result::ok).any(|ent| {
            let p = ent.path();
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n != PLUGIN_FILE_NAME)
                    .unwrap_or(false)
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

            if orig_path.exists() {
                let _ = std::fs::remove_file(&hook_path);
                std::fs::rename(&orig_path, &hook_path)?;
            } else {
                let _ = std::fs::remove_file(&hook_path);
            }
        } else if is_legacy_hook {
            let _ = std::fs::remove_file(&hook_path);
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
        && std::fs::read_to_string(&pre_commit_path)
            .map(|c| c.contains("mcp-agent-mail"))
            .unwrap_or(false);

    let pre_push_present = pre_push_path.exists()
        && std::fs::read_to_string(&pre_push_path)
            .map(|c| c.contains("mcp-agent-mail"))
            .unwrap_or(false);

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

/// Check if the guard gate is enabled.
///
/// The guard is only active if `WORKTREES_ENABLED` or `GIT_IDENTITY_ENABLED` is truthy.
/// Returns `false` (gate closed = guard inactive) by default.
#[must_use]
pub fn is_guard_gated() -> bool {
    fn is_truthy(var: &str) -> bool {
        std::env::var(var)
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "t" | "yes" | "y"
                )
            })
            .unwrap_or(false)
    }
    is_truthy("WORKTREES_ENABLED") || is_truthy("GIT_IDENTITY_ENABLED")
}

/// Check if the guard bypass is active (`AGENT_MAIL_BYPASS=1`).
#[must_use]
pub fn is_bypass_active() -> bool {
    std::env::var("AGENT_MAIL_BYPASS")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "t" | "yes" | "y"
            )
        })
        .unwrap_or(false)
}

/// Full guard check: reads reservations, checks conflicts, respects gate/bypass.
///
/// `archive_root` is the path to the project's agent-mail archive (containing `file_reservations/`).
/// `paths` are the file paths to check (relative to repo root).
///
/// Returns a `GuardCheckResult` with conflicts and mode info.
pub fn guard_check_full(archive_root: &Path, paths: &[String]) -> GuardResult<GuardCheckResult> {
    let mode = GuardMode::from_env();

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

    // Get current agent name from env
    let agent_name = std::env::var("AGENT_NAME").unwrap_or_default();
    if agent_name.is_empty() {
        return Err(GuardError::MissingAgentName);
    }

    // Read reservations from the archive
    let reservations = read_active_reservations_from_archive(archive_root)?;

    let conflicts = check_path_conflicts(paths, &reservations, &agent_name);

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
    paths: &[String],
    _advisory: bool,
) -> GuardResult<Vec<GuardConflict>> {
    // Get current agent name from env
    let agent_name = std::env::var("AGENT_NAME").unwrap_or_default();
    if agent_name.is_empty() {
        return Err(GuardError::MissingAgentName);
    }

    // Read reservations from archive JSON files
    let reservations = read_active_reservations_from_archive(archive_root)?;

    Ok(check_path_conflicts(paths, &reservations, &agent_name))
}

/// Core conflict detection: check paths against reservations.
///
/// Skips reservations held by `self_agent`. Uses symmetric fnmatch matching.
fn check_path_conflicts(
    paths: &[String],
    reservations: &[FileReservationRecord],
    self_agent: &str,
) -> Vec<GuardConflict> {
    let mut conflicts = Vec::new();
    for path in paths {
        let normalized = normalize_path(path);
        for res in reservations {
            // Skip our own reservations
            if res.agent_name == self_agent {
                continue;
            }
            // Only exclusive reservations block
            if !res.exclusive {
                continue;
            }
            let pattern_normalized = normalize_path(&res.path_pattern);
            // Symmetric glob matching
            if paths_conflict(&normalized, &pattern_normalized) {
                conflicts.push(GuardConflict {
                    path: path.clone(),
                    pattern: res.path_pattern.clone(),
                    holder: res.agent_name.clone(),
                    expires_ts: res.expires_ts.clone(),
                });
                break; // One conflict per path is enough
            }
        }
    }
    conflicts
}

/// Normalize a path for matching: forward slashes, strip leading slash.
fn normalize_path(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches('/').to_string()
}

/// Check if a pattern contains glob markers.
fn contains_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[')
}

/// Check if a file path and a reservation pattern conflict.
///
/// Uses symmetric fnmatch matching (Python parity):
/// `fnmatch(path, pattern) || fnmatch(pattern, path) || path == pattern`
///
/// For glob-vs-glob patterns, uses cross-matching.
fn paths_conflict(path: &str, pattern: &str) -> bool {
    // Direct equality
    if path == pattern {
        return true;
    }

    // If pattern ends with /* or /**, check prefix match
    if let Some(prefix) = pattern.strip_suffix("/*") {
        if path.starts_with(prefix)
            && (path.len() == prefix.len() || path.as_bytes().get(prefix.len()) == Some(&b'/'))
        {
            return true;
        }
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        if path.starts_with(prefix) {
            return true;
        }
    }

    // Symmetric fnmatch matching
    if contains_glob(path) || contains_glob(pattern) {
        return fnmatch_simple(path, pattern) || fnmatch_simple(pattern, path);
    }

    // For non-glob patterns, check prefix (directory match)
    if !contains_glob(pattern) && !contains_glob(path) {
        // pattern "app/api" should match "app/api/users.py"
        if path.starts_with(pattern) && path.as_bytes().get(pattern.len()) == Some(&b'/') {
            return true;
        }
        // reverse: path "app/api" should match pattern "app/api/users.py"
        if pattern.starts_with(path) && pattern.as_bytes().get(path.len()) == Some(&b'/') {
            return true;
        }
    }

    fnmatch_simple(path, pattern) || fnmatch_simple(pattern, path)
}

/// Simple glob pattern matching (supports * and ** wildcards).
/// Kept for backward compat; used internally by `paths_conflict`.
#[allow(dead_code)]
fn path_matches_pattern(path: &str, pattern: &str) -> bool {
    paths_conflict(&normalize_path(path), &normalize_path(pattern))
}

/// Simple fnmatch-style glob matching.
fn fnmatch_simple(name: &str, pattern: &str) -> bool {
    let mut name_chars = name.chars().peekable();
    let mut pat_chars = pattern.chars().peekable();

    while let Some(&pc) = pat_chars.peek() {
        match pc {
            '*' => {
                pat_chars.next();
                // Check for ** (match across directories)
                let double_star = pat_chars.peek() == Some(&'*');
                if double_star {
                    pat_chars.next();
                    if pat_chars.peek() == Some(&'/') {
                        pat_chars.next();
                    }
                }
                // Collect remaining pattern
                let rest: String = pat_chars.collect();
                if rest.is_empty() {
                    return true;
                }
                // Try matching rest of pattern from every position
                let remaining: String = name_chars.collect();
                for i in 0..=remaining.len() {
                    if !double_star && remaining[..i].contains('/') {
                        break;
                    }
                    if fnmatch_simple(&remaining[i..], &rest) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                pat_chars.next();
                if name_chars.next().is_none() {
                    return false;
                }
            }
            _ => {
                pat_chars.next();
                match name_chars.next() {
                    Some(nc) if nc == pc => {}
                    _ => return false,
                }
            }
        }
    }

    name_chars.peek().is_none()
}

/// Read active file reservations from the archive's `file_reservations/` directory.
///
/// Parses each `*.json` file and returns records that are:
/// - Not released (`released_ts` is null)
/// - Not expired (expires_ts > now)
/// - Exclusive
fn read_active_reservations_from_archive(
    archive_root: &Path,
) -> GuardResult<Vec<FileReservationRecord>> {
    let reservations_dir = archive_root.join("file_reservations");
    if !reservations_dir.is_dir() {
        return Ok(Vec::new());
    }

    let now = chrono::Utc::now();
    let mut records = Vec::new();

    let entries = std::fs::read_dir(&reservations_dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Only process .json files
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue, // Skip unreadable files
        };
        let val: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue, // Skip invalid JSON
        };

        // Skip released reservations
        if !val["released_ts"].is_null() {
            continue;
        }

        // Parse expires_ts and check expiry
        let expires_str = match val["expires_ts"].as_str() {
            Some(s) => s,
            None => continue,
        };
        if is_expired(expires_str, &now) {
            continue;
        }

        // Extract fields
        let pattern = val["path_pattern"].as_str().unwrap_or("").to_string();
        if pattern.is_empty() {
            continue;
        }

        let exclusive = val["exclusive"].as_bool().unwrap_or(true);
        let agent_name = val["agent_name"]
            .as_str()
            .or_else(|| val["agent"].as_str())
            .unwrap_or("unknown")
            .to_string();

        records.push(FileReservationRecord {
            path_pattern: pattern,
            agent_name,
            exclusive,
            expires_ts: expires_str.to_string(),
            released_ts: None,
        });
    }

    Ok(records)
}

/// Check if a timestamp string is expired relative to `now`.
fn is_expired(ts_str: &str, now: &chrono::DateTime<chrono::Utc>) -> bool {
    // Try parsing ISO-8601 with timezone
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str) {
        return dt < *now;
    }
    // Try parsing ISO-8601 without timezone (assume UTC)
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%S%.f") {
        let utc = dt.and_utc();
        return utc < *now;
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%S") {
        let utc = dt.and_utc();
        return utc < *now;
    }
    // If we can't parse, treat as not expired (conservative)
    false
}

// ---------------------------------------------------------------------------
// Git helpers: staged paths and push paths
// ---------------------------------------------------------------------------

/// Get staged file paths from git, including rename handling.
///
/// Uses `git diff --cached --name-status -M -z` to capture both old and new names
/// for renames (R status), and all modified/added/deleted paths.
pub fn get_staged_paths(repo_root: &Path) -> GuardResult<Vec<String>> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["diff", "--cached", "--name-status", "-M", "-z"])
        .output()?;

    if !output.status.success() {
        return Ok(Vec::new());
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

        let range = if remote_sha.chars().all(|c| c == '0') {
            // New branch: compare against merge-base with HEAD
            local_sha.to_string()
        } else {
            format!("{remote_sha}..{local_sha}")
        };

        let output = Command::new("git")
            .current_dir(repo_root)
            .args(["diff", "--name-status", "-M", "-z", &range])
            .output()?;

        if output.status.success() {
            let paths = parse_name_status_z(&output.stdout)?;
            all_paths.extend(paths);
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
                // Rename/Copy: next two entries are old and new path
                if i + 2 < parts.len() {
                    let old_path = parts[i + 1];
                    let new_path = parts[i + 2];
                    if !old_path.is_empty() {
                        paths.push(old_path.to_string());
                    }
                    if !new_path.is_empty() {
                        paths.push(new_path.to_string());
                    }
                    i += 3;
                } else {
                    i += 1;
                }
            }
            'A' | 'M' | 'D' | 'T' | 'U' => {
                // Added/Modified/Deleted/Type-change/Unmerged: next entry is path
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
            _ => {
                // Unknown status, skip
                i += 1;
            }
        }
    }

    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git must run");
        assert!(
            out.status.success(),
            "git {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
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

        install_guard("/abs/path/backend", &repo_dir).expect("install_guard");

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

    // -----------------------------------------------------------------------
    // Path normalization tests
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_strips_leading_slash_and_backslashes() {
        assert_eq!(normalize_path("/app/api/users.py"), "app/api/users.py");
        assert_eq!(normalize_path("app\\api\\users.py"), "app/api/users.py");
        assert_eq!(normalize_path("\\app\\api"), "app/api");
        assert_eq!(normalize_path("already/clean"), "already/clean");
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

    #[test]
    fn read_active_reservations_filters_correctly() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let records = read_active_reservations_from_archive(&archive).expect("read");
        // Should have: res1 (active exclusive), res4 (active non-exclusive), res5 (active exclusive self)
        // res2 (released) and res3 (expired) should be filtered out
        assert_eq!(
            records.len(),
            3,
            "expected 3 active records, got {}",
            records.len()
        );

        let patterns: Vec<&str> = records.iter().map(|r| r.path_pattern.as_str()).collect();
        assert!(patterns.contains(&"app/api/*.py"));
        assert!(patterns.contains(&"shared/*"));
        assert!(patterns.contains(&"my/stuff/*"));
    }

    #[test]
    fn read_active_reservations_empty_dir() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("empty_archive");
        // No file_reservations dir at all
        let records = read_active_reservations_from_archive(&archive).expect("read");
        assert!(records.is_empty());
    }

    // -----------------------------------------------------------------------
    // Conflict detection integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn check_path_conflicts_detects_matching_reservations() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let reservations = read_active_reservations_from_archive(&archive).expect("read");
        let paths = vec!["app/api/users.py".to_string()];

        let conflicts = check_path_conflicts(&paths, &reservations, "MyAgent");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].holder, "OtherAgent");
        assert_eq!(conflicts[0].pattern, "app/api/*.py");
        assert_eq!(conflicts[0].path, "app/api/users.py");
    }

    #[test]
    fn check_path_conflicts_skips_own_reservations() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let reservations = read_active_reservations_from_archive(&archive).expect("read");
        let paths = vec!["my/stuff/file.txt".to_string()];

        // "MyAgent" should not conflict with its own reservation
        let conflicts = check_path_conflicts(&paths, &reservations, "MyAgent");
        assert!(conflicts.is_empty(), "own reservations should be skipped");
    }

    #[test]
    fn check_path_conflicts_skips_non_exclusive() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let reservations = read_active_reservations_from_archive(&archive).expect("read");
        let paths = vec!["shared/README.md".to_string()];

        // SharedAgent's non-exclusive reservation should not block
        let conflicts = check_path_conflicts(&paths, &reservations, "SomeOtherAgent");
        assert!(
            conflicts.is_empty(),
            "non-exclusive reservations should not conflict"
        );
    }

    #[test]
    fn check_path_conflicts_no_match() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let reservations = read_active_reservations_from_archive(&archive).expect("read");
        let paths = vec!["unrelated/file.txt".to_string()];

        let conflicts = check_path_conflicts(&paths, &reservations, "MyAgent");
        assert!(conflicts.is_empty());
    }

    #[test]
    fn check_path_conflicts_multiple_paths() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let reservations = read_active_reservations_from_archive(&archive).expect("read");
        let paths = vec![
            "app/api/users.py".to_string(),
            "app/api/models.py".to_string(),
            "unrelated.txt".to_string(),
        ];

        let conflicts = check_path_conflicts(&paths, &reservations, "SomeAgent");
        assert_eq!(conflicts.len(), 2, "two paths should conflict");
        assert!(conflicts.iter().all(|c| c.holder == "OtherAgent"));
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
        assert!(paths.is_empty());
    }

    // -----------------------------------------------------------------------
    // Git integration: staged paths with renames
    // -----------------------------------------------------------------------

    #[test]
    fn staged_paths_includes_renames() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
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
            "staged paths should include rename: {:?}",
            paths
        );
    }

    #[test]
    fn staged_paths_simple_add() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
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
    fn staged_paths_empty_when_nothing_staged() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);

        let paths = get_staged_paths(&repo_dir).expect("staged paths");
        assert!(paths.is_empty());
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
}
