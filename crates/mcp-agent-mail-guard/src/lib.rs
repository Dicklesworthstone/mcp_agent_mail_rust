#![forbid(unsafe_code)]

use globset::GlobSetBuilder;
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
        "        return subprocess.run([sys.executable, str(path)], input=stdin_bytes, check=False).returncode"
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

fn render_guard_plugin_script(
    project: &str,
    hook_name: &str,
    default_db_path: Option<&str>,
) -> String {
    // Real guard plugin: checks active file reservations against staged changes (pre-commit)
    // or pushed commits (pre-push).
    let db_fallback_json =
        serde_json::to_string(default_db_path.unwrap_or("")).unwrap_or_else(|_| "\"\"".to_string());
    let project_json = serde_json::to_string(project).unwrap_or_else(|_| "\"\"".to_string());
    let hook_name_json = serde_json::to_string(hook_name).unwrap_or_else(|_| "\"\"".to_string());

    format!(
        r#"#!/usr/bin/env python3
# mcp-agent-mail guard plugin ({hook_name})
# project: {project}
# Auto-generated by mcp-agent-mail install_guard

import fnmatch
import json
import os
import re
import subprocess
import sys

PROJECT = {project_json}
HOOK_NAME = {hook_name_json}
AGENT_NAME = os.environ.get("AGENT_NAME", "")
GUARD_MODE = os.environ.get("AGENT_MAIL_GUARD_MODE", "block")
DEFAULT_DB_PATH = {db_fallback_json}

def get_staged_files():
    """Get list of staged files from git (for pre-commit)."""
    try:
        result = subprocess.run(
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
    except subprocess.CalledProcessError:
        return []
    except Exception:
        return []

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
            if set(local_sha) == {{'0'}}:
                continue

            if set(remote_sha) == {{'0'}}:
                rev_list_args = ["git", "rev-list", "--topo-order", local_sha, "--not", "--remotes"]
            else:
                rev_list_args = ["git", "rev-list", "--topo-order", f"{{remote_sha}}..{{local_sha}}"]

            # Get commits in range
            res = subprocess.run(
                rev_list_args,
                capture_output=True, text=True
            )
            if res.returncode != 0:
                continue

            commits = [c.strip() for c in res.stdout.splitlines() if c.strip()]

            for sha in commits:
                diff_res = subprocess.run(
                    ["git", "diff-tree", "-r", "--no-commit-id", "--name-status",
                     "-M", "--no-ext-diff", "--diff-filter=ACMRDTU", "-z", "-m", sha],
                    capture_output=True
                )
                if diff_res.returncode == 0:
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
    except Exception:
        pass
    return sorted(list(files))

def get_active_reservations():
    """Query active exclusive file reservations from the database."""
    db_path = os.environ.get("AGENT_MAIL_DB", "")
    if not db_path:
        if DEFAULT_DB_PATH:
            db_path = DEFAULT_DB_PATH
        else:
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
            "WHERE fr.exclusive = 1 AND (fr.released_ts IS NULL OR fr.released_ts = 0) "
            "AND fr.expires_ts > ? AND (p.human_key = ? OR p.slug = ?)",
            (now_micros, PROJECT, PROJECT),
        ).fetchall()
        conn.close()
        return [dict(r) for r in rows]
    except Exception:
        return []

def core_ignorecase_enabled():
    """Detect git core.ignorecase for path comparison parity with Rust guard."""
    try:
        res = subprocess.run(
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
    return value.lower() if CASE_INSENSITIVE_REPO else value

def glob_to_regex(pattern):
    """Convert shell-style glob to regex supporting **, [], and {} syntax."""
    # Hide ** so fnmatch does not convert it to .*
    pattern = pattern.replace("**", "\0")
    regex = fnmatch.translate(pattern)
    
    # Strip Python regex wrappers added by fnmatch
    if regex.startswith("(?s:"):
        regex = regex[4:]
    if regex.endswith(")\\Z"):
        regex = regex[:-3]
    elif regex.endswith("\\Z"):
        regex = regex[:-2]
    elif regex.endswith("$"):
        regex = regex[:-1]
        
    # Replace fnmatch's normal * (.*) with [^/]* to respect directory boundaries
    regex = regex.replace(".*", "[^/]*")
    # Restore ** as .*
    regex = regex.replace("\0", ".*")
    # Handle {a,b} bash-style brace expansion
    regex = re.sub(r"\\{([^}]+)\\}", lambda m: "(" + m.group(1).replace(",", "|") + ")", regex)
    return regex

def glob_match(path, pattern):
    """Simple shell-style glob matching (similar to Rust implementation)."""
    path = normalize_match_input(path)
    pattern = normalize_match_input(pattern)
    return re.fullmatch(glob_to_regex(pattern), path) is not None

def check_conflicts(paths, reservations):
    """Check if any paths conflict with active reservations."""
    conflicts = []
    for f in paths:
        for res in reservations:
            pattern = res["path_pattern"]
            holder = res.get("agent_name", "unknown")
            if holder == AGENT_NAME:
                continue  # Skip our own reservations

            normalized_f = normalize_match_input(f)
            normalized_pattern = normalize_match_input(pattern)
            
            # Symmetric glob matching
            if glob_match(f, pattern) or glob_match(pattern, f):
                conflicts.append((f, pattern, holder))
                break
            
            # Directory prefix matching for non-glob patterns
            has_glob = any(c in normalized_pattern for c in "*?[{{")
            if not has_glob:
                # Normal prefix check: file is inside reserved dir
                if normalized_f.startswith(normalized_pattern + "/"):
                    conflicts.append((f, pattern, holder))
                    break
                # Reverse check: pattern is inside touched file (e.g. dir replaced by file)
                if normalized_pattern.startswith(normalized_f + "/"):
                    conflicts.append((f, pattern, holder))
                    break
    return conflicts

def main():
    if not AGENT_NAME:
        # No agent context; skip guard check
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

    conflicts = check_conflicts(files_to_check, reservations)
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

pub fn install_guard(
    project: &str,
    repo: &Path,
    default_db_path: Option<&str>,
    install_prepush: bool,
) -> GuardResult<()> {
    if !repo.exists() {
        return Err(GuardError::InvalidRepo {
            path: repo.display().to_string(),
        });
    }

    let hooks_dir = resolve_hooks_dir(repo)?;
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
        std::fs::write(&chain_path, chain_script)?;
        chmod_exec(&chain_path)?;

        // Windows shims
        let cmd_path = hooks_dir.join(format!("{name}.cmd"));
        if !cmd_path.exists() {
            let body = format!(
                "@echo off\r\nsetlocal\r\nset \"DIR=%~dp0\"\r\npython \"%DIR%{name}\" %*\r\nexit /b %ERRORLEVEL%\r\n"
            );
            std::fs::write(&cmd_path, body)?;
        }
        let ps1_path = hooks_dir.join(format!("{name}.ps1"));
        if !ps1_path.exists() {
            let body = format!(
                "$ErrorActionPreference = 'Stop'\n$hook = Join-Path $PSScriptRoot '{name}'\npython $hook @args\nexit $LASTEXITCODE\n"
            );
            std::fs::write(&ps1_path, body)?;
        }

        // Write guard plugin
        let plugin_path = run_dir.join(PLUGIN_FILE_NAME);
        std::fs::write(
            &plugin_path,
            render_guard_plugin_script(project, name, default_db_path),
        )?;
        chmod_exec(&plugin_path)?;

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

fn is_truthy_value(value: Option<&str>) -> bool {
    value
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "t" | "yes" | "y"
            )
        })
        .unwrap_or(false)
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

    // Get current agent name from env
    let agent_name = std::env::var("AGENT_NAME").unwrap_or_default();
    if agent_name.is_empty() {
        return Err(GuardError::MissingAgentName);
    }

    // Read reservations from the archive
    let reservations = read_active_reservations_from_archive(archive_root)?;

    let conflicts = check_path_conflicts(paths, &reservations, &agent_name, ignorecase);

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
    // Get current agent name from env
    let agent_name = std::env::var("AGENT_NAME").unwrap_or_default();
    if agent_name.is_empty() {
        return Err(GuardError::MissingAgentName);
    }

    // Read reservations from archive JSON files
    let reservations = read_active_reservations_from_archive(archive_root)?;

    Ok(check_path_conflicts(
        paths,
        &reservations,
        &agent_name,
        ignorecase,
    ))
}

/// Core conflict detection: check paths against reservations using globset.
///
/// Skips reservations held by `self_agent`.
fn check_path_conflicts(
    paths: &[String],
    reservations: &[FileReservationRecord],
    self_agent: &str,
    ignorecase: bool,
) -> Vec<GuardConflict> {
    // 1. Build a GlobSet for all relevant reservations (other agents, exclusive).
    // Map glob index back to reservation record for conflict reporting.
    let mut builder = GlobSetBuilder::new();
    let mut active_indices: Vec<&FileReservationRecord> = Vec::with_capacity(reservations.len());

    for res in reservations {
        if res.exclusive && res.agent_name != self_agent {
            // Configure glob to match gitignore semantics (literal_separator(true) means * does not cross /)
            // matching the custom Python glob_to_regex behavior where * -> [^/]* and ** -> .*.
            // Use normalize_path to handle slashes before compiling.
            let pat_str = normalize_path(&res.path_pattern, ignorecase);
            let glob = globset::GlobBuilder::new(&pat_str)
                .literal_separator(true)
                .build();
            if let Ok(g) = glob {
                builder.add(g);
                active_indices.push(res);
            }
        }
    }

    let glob_set = match builder.build() {
        Ok(gs) => gs,
        Err(_) => return Vec::new(), // Should not happen with valid globs
    };

    let mut conflicts = Vec::new();

    for path in paths {
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
            let pat_norm = normalize_path(&res.path_pattern, ignorecase);

            // Check if the path is a prefix of the pattern's base directory
            // (e.g. path "src", pattern "src/main.rs" or "src/**")
            if pat_norm.starts_with(&normalized)
                && (normalized.is_empty()
                    || pat_norm
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

            // Also check the reverse: pattern's literal base is a prefix of the path
            // (needed for non-glob patterns like "src/utils" matching "src/utils/file.rs")
            let has_glob = res.path_pattern.contains('*')
                || res.path_pattern.contains('?')
                || res.path_pattern.contains('[')
                || res.path_pattern.contains('{');
            if !has_glob
                && normalized.starts_with(&pat_norm)
                && (pat_norm.is_empty()
                    || normalized
                        .as_bytes()
                        .get(pat_norm.len())
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

    conflicts
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
        normalized.to_ascii_lowercase()
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

/// Read active file reservations from the archive's `file_reservations/` directory.
///
/// Parses each `*.json` file and returns records that are:
/// - Not released (`released_ts` is null)
/// - Not expired (`expires_ts > now`; at exact boundary reservation is expired)
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
        let rev_list = rev_list_cmd.output()?;

        if rev_list.status.success() {
            for sha in String::from_utf8_lossy(&rev_list.stdout)
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let output = Command::new("git")
                    .current_dir(repo_root)
                    .args([
                        "diff-tree",
                        "-r",
                        "--no-commit-id",
                        "--name-status",
                        "-M",
                        "--no-ext-diff",
                        "--diff-filter=ACMRDTU",
                        "-z",
                        "-m",
                        sha,
                    ])
                    .output()?;

                if output.status.success() {
                    let paths = parse_name_status_z(&output.stdout)?;
                    all_paths.extend(paths);
                }
            }
        } else if let Some(range) = diff_range {
            // Fallback: net diff across the range (less precise, but better than nothing).
            let output = Command::new("git")
                .current_dir(repo_root)
                .args(["diff", "--name-status", "-M", "-z", &range])
                .output()?;

            if output.status.success() {
                let paths = parse_name_status_z(&output.stdout)?;
                all_paths.extend(paths);
            }
        };
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
                    // Incomplete rename/copy entry — skip remaining parts to
                    // avoid misaligning with subsequent status entries.
                    break;
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

    /// Simple fnmatch-style glob matching (like Python's `fnmatch.fnmatch`).
    /// `*` matches within a single directory, `**` matches across directories.
    fn fnmatch_simple(path: &str, pattern: &str) -> bool {
        if path == pattern {
            return true;
        }
        match globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
        {
            Ok(g) => g.compile_matcher().is_match(path),
            Err(_) => false,
        }
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

    /// Returns true if the string contains glob metacharacters.
    fn contains_glob(s: &str) -> bool {
        s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{')
    }

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

    fn run_git_stdout(dir: &Path, args: &[&str]) -> String {
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

        install_guard("/abs/path/backend", &repo_dir, None, false).expect("install_guard");

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
            normalize_path("./app/./api/./file.py", false),
            "app/api/file.py"
        );
        // Mixed
        assert_eq!(
            normalize_path("/./app/../src/./lib.rs", false),
            "src/lib.rs"
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
        }
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

        let conflicts = check_path_conflicts(&paths, &reservations, "MyAgent", false);
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
        let conflicts = check_path_conflicts(&paths, &reservations, "MyAgent", false);
        assert!(conflicts.is_empty(), "own reservations should be skipped");
    }

    #[test]
    fn check_path_conflicts_skips_non_exclusive() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = make_archive_with_reservations(td.path());

        let reservations = read_active_reservations_from_archive(&archive).expect("read");
        let paths = vec!["shared/README.md".to_string()];

        // SharedAgent's non-exclusive reservation should not block
        let conflicts = check_path_conflicts(&paths, &reservations, "SomeOtherAgent", false);
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

        let conflicts = check_path_conflicts(&paths, &reservations, "MyAgent", false);
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

        let conflicts = check_path_conflicts(&paths, &reservations, "SomeAgent", false);
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
        let conflicts = check_path_conflicts(&paths, &[], "AnyAgent", false);
        assert!(
            conflicts.is_empty(),
            "empty reservation set should never block"
        );
    }

    #[test]
    fn check_path_conflicts_submodule_pointer_path_matches_recursive_pattern() {
        let paths = vec!["modules/submod".to_string()];
        let reservations = vec![reservation("modules/submod/**", "OtherAgent", true)];
        let conflicts = check_path_conflicts(&paths, &reservations, "MyAgent", false);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].holder, "OtherAgent");
    }

    #[test]
    fn check_path_conflicts_non_glob_directory_prefix_matches_contained_file() {
        let paths = vec!["src/utils/file.rs".to_string()];
        // Reservation is a literal directory without glob metacharacters
        let reservations = vec![reservation("src/utils", "OtherAgent", true)];
        let conflicts = check_path_conflicts(&paths, &reservations, "MyAgent", false);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].holder, "OtherAgent");
    }

    #[test]
    fn check_path_conflicts_binary_file_matches_glob() {
        let paths = vec!["bin/tool.exe".to_string()];
        let reservations = vec![reservation("bin/*.exe", "Locker", true)];
        let conflicts = check_path_conflicts(&paths, &reservations, "MyAgent", false);
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
        let conflicts = check_path_conflicts(&paths, &reservations, "MyAgent", false);
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

        let conflicts = check_path_conflicts(&renamed_paths, &reservations, "MyAgent", false);
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
        let conflicts = check_path_conflicts(&paths, &reservations, "MyAgent", false);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].holder, "TargetOwner");
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
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);

        let paths = get_staged_paths(&repo_dir).expect("staged paths");
        assert!(paths.is_empty());
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

        // Commit 2 reverts it so net diff(remote..local) would be empty.
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

    #[test]
    fn push_paths_skips_delete_pushes() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        // Delete push: local sha is all zeros. Should not attempt git and should return empty.
        let stdin_lines = "refs/heads/main 0000000000000000000000000000000000000000 refs/heads/main 1234567890abcdef1234567890abcdef12345678\n";
        let paths = get_push_paths(&repo_dir, stdin_lines).expect("push paths");
        assert!(paths.is_empty());
    }

    #[test]
    fn push_paths_new_branch_remote_zero_still_enumerates_commits() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(&repo_dir, &["config", "user.name", "test"]);

        std::fs::write(repo_dir.join("a.txt"), "one\n").expect("write");
        run_git(&repo_dir, &["add", "a.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "c1"]);

        std::fs::write(repo_dir.join("b.txt"), "two\n").expect("write");
        run_git(&repo_dir, &["add", "b.txt"]);
        run_git(&repo_dir, &["commit", "-qm", "c2"]);
        let local_sha = run_git_stdout(&repo_dir, &["rev-parse", "HEAD"]);

        let stdin_lines = format!(
            "refs/heads/main {local_sha} refs/heads/main 0000000000000000000000000000000000000000\n"
        );
        let paths = get_push_paths(&repo_dir, &stdin_lines).expect("push paths");
        assert!(
            paths.contains(&"b.txt".to_string()),
            "expected b.txt in push paths, got: {paths:?}"
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

        install_guard("/test/project", &repo_dir, None, false).expect("install");

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

        let records = read_active_reservations_from_archive(&archive).expect("read");
        // Malformed JSON should be skipped, valid one should be read
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

        let records = read_active_reservations_from_archive(&archive).expect("read");
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

        let records = read_active_reservations_from_archive(&archive).expect("read");
        assert!(records.is_empty(), "empty path_pattern should be skipped");
    }

    #[test]
    fn read_reservations_uses_agent_field_fallback() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("archive");
        let res_dir = archive.join("file_reservations");
        std::fs::create_dir_all(&res_dir).expect("mkdir");

        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        // Use "agent" key instead of "agent_name"
        let alt_key = serde_json::json!({
            "path_pattern": "src/**",
            "agent": "FallbackAgent",
            "exclusive": true,
            "expires_ts": future.to_rfc3339(),
            "released_ts": null
        });
        std::fs::write(res_dir.join("alt.json"), alt_key.to_string()).expect("write");

        let records = read_active_reservations_from_archive(&archive).expect("read");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].agent_name, "FallbackAgent");
    }

    #[test]
    fn read_reservations_missing_agent_defaults_to_unknown() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let archive = td.path().join("archive");
        let res_dir = archive.join("file_reservations");
        std::fs::create_dir_all(&res_dir).expect("mkdir");

        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let no_agent = serde_json::json!({
            "path_pattern": "src/**",
            "exclusive": true,
            "expires_ts": future.to_rfc3339(),
            "released_ts": null
        });
        std::fs::write(res_dir.join("noagent.json"), no_agent.to_string()).expect("write");

        let records = read_active_reservations_from_archive(&archive).expect("read");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].agent_name, "unknown");
    }

    // -----------------------------------------------------------------------
    // resolve_hooks_dir error path tests
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_hooks_dir_nonexistent_path_returns_error() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let nonexistent = td.path().join("no_such_dir");
        let result = resolve_hooks_dir(&nonexistent);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GuardError::InvalidRepo { .. }
        ));
    }

    #[test]
    fn resolve_hooks_dir_bare_repo_returns_error() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let bare_dir = td.path().join("bare.git");
        run_git(
            td.path(),
            &["init", "-q", "--bare", bare_dir.to_str().unwrap()],
        );

        let result = resolve_hooks_dir(&bare_dir);
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), GuardError::InvalidRepo { .. }),
            "bare repos should be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // install_guard / uninstall_guard error and edge case tests
    // -----------------------------------------------------------------------

    #[test]
    fn install_guard_nonexistent_repo_returns_error() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let nonexistent = td.path().join("nonexistent");
        let result = install_guard("/test/project", &nonexistent, None, false);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GuardError::InvalidRepo { .. }
        ));
    }

    #[test]
    fn install_guard_idempotent() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);

        // Install twice - should not error
        install_guard("/test/project", &repo_dir, None, false).expect("first install");
        install_guard("/test/project", &repo_dir, None, false).expect("second install");

        let status = guard_status(&repo_dir).expect("status");
        assert!(status.pre_commit_present);
    }

    #[test]
    fn uninstall_guard_nonexistent_repo_returns_error() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let nonexistent = td.path().join("nonexistent");
        let result = uninstall_guard(&nonexistent);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GuardError::InvalidRepo { .. }
        ));
    }

    #[test]
    fn uninstall_guard_without_prior_install_is_noop() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let repo_dir = td.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-q"]);

        // Uninstall on a repo without guard should succeed silently
        uninstall_guard(&repo_dir).expect("uninstall without install");
    }

    // -----------------------------------------------------------------------
    // is_expired edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn is_expired_at_exact_boundary_is_expired() {
        let now = chrono::Utc::now();
        let ts = now.to_rfc3339();
        assert!(
            is_expired(&ts, &now),
            "expiry at exact now should be expired (<= semantics)"
        );
    }

    #[test]
    fn is_expired_naive_without_fractional() {
        let now = chrono::Utc::now();
        let past = (now - chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        assert!(is_expired(&past, &now));
    }

    // -----------------------------------------------------------------------
    // is_legacy_single_file_guard tests
    // -----------------------------------------------------------------------

    #[test]
    fn legacy_guard_detection() {
        assert!(is_legacy_single_file_guard(
            "#!/bin/sh\n# mcp-agent-mail guard hook\necho guard"
        ));
        assert!(is_legacy_single_file_guard(
            "AGENT_NAME environment variable is required."
        ));
        assert!(!is_legacy_single_file_guard("#!/bin/sh\necho hello\n"));
    }

    // -----------------------------------------------------------------------
    // expand_user tests
    // -----------------------------------------------------------------------

    #[test]
    fn expand_user_tilde_only() {
        let result = expand_user("~");
        // Should expand to home dir or fall back to "~"
        assert!(!result.to_string_lossy().is_empty());
    }

    #[test]
    fn expand_user_tilde_prefix() {
        let result = expand_user("~/foo/bar");
        let s = result.to_string_lossy();
        assert!(s.ends_with("foo/bar"));
    }

    #[test]
    fn expand_user_no_tilde() {
        let result = expand_user("/abs/path");
        assert_eq!(result, PathBuf::from("/abs/path"));
    }

    #[test]
    fn expand_user_relative_no_tilde() {
        let result = expand_user("relative/path");
        assert_eq!(result, PathBuf::from("relative/path"));
    }

    // -----------------------------------------------------------------------
    // render script tests
    // -----------------------------------------------------------------------

    #[test]
    fn chain_runner_pre_commit_contains_expected_markers() {
        let script = render_chain_runner_script("pre-commit");
        assert!(script.contains("mcp-agent-mail chain-runner (pre-commit)"));
        assert!(script.contains("hooks.d"));
        assert!(script.contains("pre-commit.orig"));
        // pre-commit should NOT have stdin forwarding
        assert!(!script.contains("stdin_bytes = sys.stdin.buffer.read()"));
    }

    #[test]
    fn chain_runner_pre_push_forwards_stdin() {
        let script = render_chain_runner_script("pre-push");
        assert!(script.contains("mcp-agent-mail chain-runner (pre-push)"));
        assert!(script.contains("stdin_bytes = sys.stdin.buffer.read()"));
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
    // contains_glob tests
    // -----------------------------------------------------------------------

    #[test]
    fn contains_glob_detects_all_chars() {
        assert!(contains_glob("app/*.py"));
        assert!(contains_glob("app/v?/file"));
        assert!(contains_glob("app/[abc]/file"));
        assert!(!contains_glob("app/api/file.py"));
        assert!(!contains_glob(""));
    }

    // -----------------------------------------------------------------------
    // is_expired tests
    // -----------------------------------------------------------------------

    #[test]
    fn is_expired_far_future_not_expired() {
        let now = chrono::Utc::now();
        assert!(!is_expired("2099-12-31T23:59:59Z", &now));
    }

    #[test]
    fn is_expired_far_past_is_expired() {
        let now = chrono::Utc::now();
        assert!(is_expired("2000-01-01T00:00:00Z", &now));
    }

    #[test]
    fn is_expired_empty_string_is_not_expired() {
        // Unparseable timestamps are treated as not-expired (safe default).
        let now = chrono::Utc::now();
        assert!(!is_expired("", &now));
    }

    #[test]
    fn guard_plugin_script_contains_project() {
        let script = render_guard_plugin_script("/my/project", "pre-commit", None);
        assert!(script.contains("mcp-agent-mail guard plugin (pre-commit)"));
        assert!(script.contains("PROJECT = \"/my/project\""));
        assert!(script.contains("get_staged_files"));
        assert!(script.contains("check_conflicts"));
        assert!(script.contains("def glob_to_regex"));
        assert!(script.contains("core.ignorecase"));
    }
}
