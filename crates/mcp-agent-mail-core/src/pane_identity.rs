//! Canonical per-pane agent identity file contract.
//!
//! Resolves the diverging conventions described in `mcp_agent_mail#111`:
//!
//! - Claude Code: `~/.claude/agent-mail/identity.$TMUX_PANE` (persistent, not project-scoped)
//! - NTM #68: `/tmp/agent-mail-name.<hash>.<pane_id>` (project-scoped, ephemeral)
//!
//! The canonical contract:
//!
//! - **Path**: `~/.config/agent-mail/identity/<project_hash>/<pane_key>`
//! - **Pane key**: Composite `session_name:window_index:pane_index` via
//!   `tmux display-message`, falling back to bare `$TMUX_PANE` (see #41).
//! - **Content**: Plain text file containing the agent name (trimmed, single line)
//! - **Fallback**: Reads from legacy bare-pane-ID files and older paths for
//!   backwards compatibility
//! - **Cleanup**: Stale identity files (panes that no longer exist) can be pruned
//!
//! All agent runtimes (Claude Code, NTM/Codex, Gemini, etc.) should converge on
//! [`write_identity`] and [`resolve_identity`] as the single source of truth.

use sha1::{Digest, Sha1};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Top-level directory under `~/.config` for agent-mail pane identity files.
const IDENTITY_DIR_NAME: &str = "agent-mail/identity";

/// How many hex chars of the project hash to use in the directory name.
const PROJECT_HASH_LEN: usize = 12;

#[cfg(test)]
static TEST_CONFIG_BASE_DIR: std::sync::LazyLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
static TEST_LIVE_TMUX_PANES: std::sync::LazyLock<std::sync::Mutex<Option<Vec<String>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compute the canonical identity file path for a given project and tmux pane.
///
/// Returns `~/.config/agent-mail/identity/<project_hash>/<sanitized_pane_id>`.
/// The `project_key` is typically the absolute path to the project directory.
/// The `pane_id` is either a composite key (e.g., `main:0:2`) produced by
/// [`get_composite_tmux_pane_id`], or a bare tmux pane identifier (e.g., `%3`).
#[must_use]
pub fn canonical_identity_path(project_key: &str, pane_id: &str) -> PathBuf {
    let base = config_base_dir();
    let hash = project_hash(project_key);
    let sanitized_pane = sanitize_pane_id(pane_id);
    base.join(IDENTITY_DIR_NAME).join(hash).join(sanitized_pane)
}

/// Write an agent name to the canonical identity file for a pane.
///
/// Creates parent directories as needed. Returns the path written to on
/// success, or an IO error on failure.
///
/// # Arguments
/// - `project_key`: Absolute path to the project directory (used for hashing)
/// - `pane_id`: Tmux pane identifier (e.g., `%0`)
/// - `agent_name`: The agent name to write (e.g., `BlueLake`)
pub fn write_identity(
    project_key: &str,
    pane_id: &str,
    agent_name: &str,
) -> std::io::Result<PathBuf> {
    let path = canonical_identity_path(project_key, pane_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, format!("{}\n", agent_name.trim()))?;
    Ok(path)
}

/// Resolve the agent name for a given project and tmux pane.
///
/// Checks the following locations in order:
/// 1. Canonical path: `~/.config/agent-mail/identity/<project_hash>/<pane_id>`
/// 2. Legacy Claude Code path: `~/.claude/agent-mail/identity.<pane_id>`
/// 3. Legacy NTM path: `/tmp/agent-mail-name.<project_hash>.<pane_id>`
///
/// Returns `None` if no identity file is found or all are empty.
#[must_use]
pub fn resolve_identity(project_key: &str, pane_id: &str) -> Option<String> {
    resolve_identity_with_path(project_key, pane_id).map(|(name, _)| name)
}

/// Resolve the agent name and the identity file path actually used.
///
/// This follows the same lookup order as [`resolve_identity`], but returns the
/// concrete file path that produced the winning match. Callers that surface the
/// resolved path to operators should prefer this helper so diagnostics reflect
/// reality when a legacy fallback file is read.
///
/// When `pane_id` is a composite key (contains `:`), also tries a legacy
/// lookup using the bare `$TMUX_PANE` value to ensure backwards compatibility
/// with identity files written before the composite key migration.
#[must_use]
pub fn resolve_identity_with_path(project_key: &str, pane_id: &str) -> Option<(String, PathBuf)> {
    // 1. Canonical path (composite or bare)
    let canonical = canonical_identity_path(project_key, pane_id);
    if let Some(name) = read_identity_file(&canonical) {
        return Some((name, canonical));
    }

    // 1b. If pane_id is a composite key, try legacy bare $TMUX_PANE canonical path.
    //     A composite key contains `:`, e.g., `main:0:2`. The bare pane env var
    //     is something like `%3`. We check the env so we can find files written
    //     before the composite key migration.
    if pane_id.contains(':')
        && let Some(bare) = tmux_pane_env()
    {
        let bare = bare.trim().to_string();
        if !bare.is_empty() {
            let legacy_canonical = canonical_identity_path(project_key, &bare);
            if let Some(name) = read_identity_file(&legacy_canonical) {
                return Some((name, legacy_canonical));
            }
        }
    }

    // 2. Legacy Claude Code path: ~/.claude/agent-mail/identity.$TMUX_PANE
    if let Some(home) = home_dir() {
        let sanitized = sanitize_pane_id(pane_id);
        let legacy_claude = home
            .join(".claude")
            .join("agent-mail")
            .join(format!("identity.{sanitized}"));
        if let Some(name) = read_identity_file(&legacy_claude) {
            return Some((name, legacy_claude));
        }

        // 2b. If composite key, also try bare pane ID for legacy Claude Code path
        if pane_id.contains(':')
            && let Some(bare) = tmux_pane_env()
        {
            let bare_sanitized = sanitize_pane_id(bare.trim());
            if bare_sanitized != sanitized {
                let legacy_claude_bare = home
                    .join(".claude")
                    .join("agent-mail")
                    .join(format!("identity.{bare_sanitized}"));
                if let Some(name) = read_identity_file(&legacy_claude_bare) {
                    return Some((name, legacy_claude_bare));
                }
            }
        }
    }

    // 3. Legacy NTM path: /tmp/agent-mail-name.<project_hash>.<pane_id>
    let hash = project_hash(project_key);
    let sanitized = sanitize_pane_id(pane_id);
    let legacy_ntm = PathBuf::from(format!("/tmp/agent-mail-name.{hash}.{sanitized}"));
    if let Some(name) = read_identity_file(&legacy_ntm) {
        return Some((name, legacy_ntm));
    }

    // 3b. If composite key, also try bare pane ID for legacy NTM path
    if pane_id.contains(':')
        && let Some(bare) = tmux_pane_env()
    {
        let bare_sanitized = sanitize_pane_id(bare.trim());
        if bare_sanitized != sanitized {
            let legacy_ntm_bare =
                PathBuf::from(format!("/tmp/agent-mail-name.{hash}.{bare_sanitized}"));
            if let Some(name) = read_identity_file(&legacy_ntm_bare) {
                return Some((name, legacy_ntm_bare));
            }
        }
    }

    None
}

/// Resolve the agent name for the current tmux pane.
///
/// Uses [`get_composite_tmux_pane_id`] to obtain a session-unique composite
/// key (e.g., `main:0:2`), falling back to bare `$TMUX_PANE` if unavailable.
/// Returns `None` if no pane identifier can be determined.
#[must_use]
pub fn resolve_identity_current_pane(project_key: &str) -> Option<String> {
    let pane_id = get_composite_tmux_pane_id();
    resolve_identity_for_pane(project_key, pane_id.as_deref())
}

/// Write identity for the current tmux pane.
///
/// Uses [`get_composite_tmux_pane_id`] to obtain a session-unique composite
/// key (e.g., `main:0:2`), falling back to bare `$TMUX_PANE` if unavailable.
/// Returns `None` if no pane identifier can be determined.
#[must_use]
pub fn write_identity_current_pane(
    project_key: &str,
    agent_name: &str,
) -> Option<std::io::Result<PathBuf>> {
    let pane_id = get_composite_tmux_pane_id();
    write_identity_for_pane(project_key, pane_id.as_deref(), agent_name)
}

/// Remove stale identity files for panes that no longer exist.
///
/// Queries tmux for active composite pane keys and bare pane IDs, then removes
/// any identity files under the given project hash directory whose names do
/// not correspond to a live pane.
///
/// Returns the list of removed file paths.
#[must_use]
pub fn cleanup_stale_identities(project_key: &str) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let base = config_base_dir();
    let hash = project_hash(project_key);
    let project_dir = base.join(IDENTITY_DIR_NAME).join(&hash);

    if !path_is_real_directory(&project_dir) {
        return removed;
    }

    let live_panes = list_live_tmux_panes();

    let Ok(entries) = std::fs::read_dir(&project_dir) else {
        return removed;
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();

        // If tmux is not running (empty live_panes list), skip cleanup
        // to avoid accidentally removing everything.
        if live_panes.is_empty() {
            break;
        }

        if !file_name_matches_live_pane(&file_name, &live_panes) {
            let path = entry.path();
            if dir_entry_is_real_file(&entry) && std::fs::remove_file(&path).is_ok() {
                removed.push(path);
            }
        }
    }

    removed
}

/// Clean up stale identities across all project hash directories.
///
/// Iterates over every `<project_hash>/` directory under the identity root
/// and prunes files for dead panes. Returns all removed file paths.
#[must_use]
pub fn cleanup_all_stale_identities() -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let base = config_base_dir();
    let identity_root = base.join(IDENTITY_DIR_NAME);

    if !path_is_real_directory(&identity_root) {
        return removed;
    }

    let live_panes = list_live_tmux_panes();
    if live_panes.is_empty() {
        // tmux not running or no panes — skip to avoid wiping everything
        return removed;
    }

    let Ok(entries) = std::fs::read_dir(&identity_root) else {
        return removed;
    };

    for dir_entry in entries.flatten() {
        let project_dir = dir_entry.path();
        if !dir_entry_is_real_directory(&dir_entry) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&project_dir) else {
            continue;
        };
        for file_entry in files.flatten() {
            let file_name = file_entry.file_name();
            if !file_name_matches_live_pane(&file_name, &live_panes) {
                let path = file_entry.path();
                if dir_entry_is_real_file(&file_entry) && std::fs::remove_file(&path).is_ok() {
                    removed.push(path);
                }
            }
        }
    }

    removed
}

/// List all identity entries for a project (for diagnostic/debug use).
///
/// Returns `(pane_id, agent_name)` pairs from the canonical directory.
#[must_use]
pub fn list_identities(project_key: &str) -> Vec<(String, String)> {
    let base = config_base_dir();
    let hash = project_hash(project_key);
    let project_dir = base.join(IDENTITY_DIR_NAME).join(hash);

    if !path_is_real_directory(&project_dir) {
        return Vec::new();
    }

    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(&project_dir) else {
        return result;
    };

    for entry in entries.flatten() {
        let pane_id = entry.file_name().to_string_lossy().to_string();
        if let Some(name) = read_identity_file(&entry.path()) {
            result.push((pane_id, name));
        }
    }

    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn path_is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn dir_entry_is_real_directory(entry: &std::fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|file_type| file_type.is_dir())
}

fn dir_entry_is_real_file(entry: &std::fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|file_type| file_type.is_file())
}

fn file_name_matches_live_pane(file_name: &OsStr, live_panes: &[String]) -> bool {
    let name = file_name.to_string_lossy();
    live_panes.iter().any(|pane| pane.as_str() == name.as_ref())
}

/// Compute a truncated SHA-1 hex hash of the project key.
fn project_hash(project_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(project_key.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    hex.chars().take(PROJECT_HASH_LEN).collect()
}

/// Sanitize a tmux pane ID for use as a filename.
///
/// Strips the leading `%` character and replaces any filesystem-unsafe
/// characters with hyphens (for `:` in composite keys like
/// `session:window:pane`) or underscores (for other unsafe chars).
///
/// The `%` prefix is conventional in tmux (e.g., `%0`, `%3`) but not
/// great for filenames. Composite keys use `:` as separator which becomes
/// `-` so that `mysession:0:2` becomes `mysession-0-2`.
fn sanitize_pane_id(pane_id: &str) -> String {
    let stripped = pane_id.strip_prefix('%').unwrap_or(pane_id);
    let mut out = String::with_capacity(stripped.len());
    for ch in stripped.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch == ':' {
            out.push('-');
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

/// Read and trim the contents of an identity file. Returns `None` if the
/// file doesn't exist or is empty.
fn read_identity_file(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[must_use]
fn resolve_identity_for_pane(project_key: &str, pane_id: Option<&str>) -> Option<String> {
    let pane_id = pane_id?.trim();
    if pane_id.is_empty() {
        return None;
    }
    resolve_identity(project_key, pane_id)
}

fn write_identity_for_pane(
    project_key: &str,
    pane_id: Option<&str>,
    agent_name: &str,
) -> Option<std::io::Result<PathBuf>> {
    let pane_id = pane_id?.trim();
    if pane_id.is_empty() {
        return None;
    }
    Some(write_identity(project_key, pane_id, agent_name))
}

/// Get the XDG-compatible config base directory (`~/.config`).
fn config_base_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = test_config_base_dir() {
        return path;
    }

    if let Some(path) = env_path("XDG_CONFIG_HOME") {
        return path;
    }

    home_dir()
        .map(|home| home.join(".config"))
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp").join(".config"))
}

fn home_dir() -> Option<PathBuf> {
    env_path("HOME").or_else(dirs::home_dir)
}

fn env_path(key: &str) -> Option<PathBuf> {
    crate::config::process_env_value(key).and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(shellexpand::tilde(trimmed).into_owned()))
        }
    })
}

fn tmux_pane_env() -> Option<String> {
    crate::config::process_env_value("TMUX_PANE").filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
fn test_config_base_dir() -> Option<PathBuf> {
    TEST_CONFIG_BASE_DIR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
fn set_test_config_base_dir(path: Option<PathBuf>) {
    *TEST_CONFIG_BASE_DIR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
}

#[cfg(test)]
fn test_live_tmux_panes() -> Option<Vec<String>> {
    TEST_LIVE_TMUX_PANES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
fn set_test_live_tmux_panes(panes: Option<Vec<String>>) {
    *TEST_LIVE_TMUX_PANES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = panes;
}

/// Query tmux for all live pane IDs (sanitized).
///
/// Returns composite keys (`session_name:window_index:pane_index`) for each
/// live pane, plus the legacy bare pane ID (e.g., `%3` -> `3`) for backwards
/// compatibility during cleanup. Returns an empty vec if tmux is not running
/// or the command fails.
fn list_live_tmux_panes() -> Vec<String> {
    #[cfg(test)]
    if let Some(panes) = test_live_tmux_panes() {
        return panes;
    }

    let output = std::process::Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}:#{window_index}:#{pane_index}:#{pane_id}",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut ids = Vec::new();
            for line in text.lines().filter(|l| !l.is_empty()) {
                let line = line.trim();
                // Parse "session:window:pane_idx:pane_id" format.
                // The composite key is the first three fields joined by `:`.
                // We also include the bare pane_id for backwards compat.
                if let Some((composite, bare_id)) = line.rsplit_once(':') {
                    ids.push(sanitize_pane_id(composite));
                    ids.push(sanitize_pane_id(bare_id));
                } else {
                    // Fallback: treat the entire line as a bare pane ID
                    ids.push(sanitize_pane_id(line));
                }
            }
            ids.sort();
            ids.dedup();
            ids
        }
        _ => Vec::new(),
    }
}

/// Get a composite tmux pane identifier for the current pane.
///
/// Runs `tmux display-message -p '#{session_name}:#{window_index}:#{pane_index}'`
/// to produce a key like `main:0:2` that is unique across tmux sessions.
///
/// Falls back to the bare `$TMUX_PANE` environment variable if the tmux
/// command fails (e.g., tmux is not running, or `display-message` is
/// unavailable).
///
/// Returns `None` if neither the composite key nor `$TMUX_PANE` can be
/// determined.
#[must_use]
pub fn get_composite_tmux_pane_id() -> Option<String> {
    // Try the composite key first via tmux display-message.
    let output = std::process::Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "#{session_name}:#{window_index}:#{pane_index}",
        ])
        .output();

    if let Ok(out) = output
        && out.status.success()
    {
        let composite = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !composite.is_empty() && composite.contains(':') {
            return Some(composite);
        }
    }

    // Fallback to bare $TMUX_PANE
    tmux_pane_env()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    static TEST_CONFIG_BASE_DIR_SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct IsolatedConfigBaseDir {
        _guard: MutexGuard<'static, ()>,
        tempdir: tempfile::TempDir,
    }

    impl IsolatedConfigBaseDir {
        fn new() -> Self {
            let guard = TEST_CONFIG_BASE_DIR_SERIAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let tempdir = tempfile::tempdir().expect("temp config dir");
            set_test_config_base_dir(Some(tempdir.path().to_path_buf()));
            Self {
                _guard: guard,
                tempdir,
            }
        }

        fn project_key(&self, suffix: &str) -> String {
            self.tempdir
                .path()
                .join(suffix)
                .to_string_lossy()
                .into_owned()
        }
    }

    impl Drop for IsolatedConfigBaseDir {
        fn drop(&mut self) {
            set_test_config_base_dir(None);
        }
    }

    struct LiveTmuxPanesGuard;

    impl LiveTmuxPanesGuard {
        fn new(panes: Vec<String>) -> Self {
            set_test_live_tmux_panes(Some(panes));
            Self
        }
    }

    impl Drop for LiveTmuxPanesGuard {
        fn drop(&mut self) {
            set_test_live_tmux_panes(None);
        }
    }

    // -- project_hash --------------------------------------------------------

    #[test]
    fn project_hash_produces_expected_length() {
        let h = project_hash("/data/projects/backend");
        assert_eq!(h.len(), PROJECT_HASH_LEN);
    }

    #[test]
    fn project_hash_deterministic() {
        let a = project_hash("/data/projects/backend");
        let b = project_hash("/data/projects/backend");
        assert_eq!(a, b);
    }

    #[test]
    fn project_hash_differs_for_different_projects() {
        let a = project_hash("/data/projects/alpha");
        let b = project_hash("/data/projects/beta");
        assert_ne!(a, b);
    }

    // -- sanitize_pane_id ----------------------------------------------------

    #[test]
    fn sanitize_strips_percent() {
        assert_eq!(sanitize_pane_id("%0"), "0");
        assert_eq!(sanitize_pane_id("%123"), "123");
    }

    #[test]
    fn sanitize_preserves_plain_id() {
        assert_eq!(sanitize_pane_id("42"), "42");
    }

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize_pane_id("%foo/bar"), "foo_bar");
    }

    #[test]
    fn sanitize_empty_returns_unknown() {
        assert_eq!(sanitize_pane_id(""), "unknown");
        assert_eq!(sanitize_pane_id("%"), "unknown");
    }

    #[test]
    fn sanitize_composite_key_uses_hyphens() {
        assert_eq!(sanitize_pane_id("main:0:2"), "main-0-2");
        assert_eq!(sanitize_pane_id("my_session:1:0"), "my_session-1-0");
    }

    // -- canonical_identity_path ---------------------------------------------

    #[test]
    fn canonical_path_has_expected_structure() {
        let path = canonical_identity_path("/data/projects/backend", "%3");
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("agent-mail/identity/"),
            "missing identity dir: {path_str}"
        );
        assert!(
            path_str.ends_with("/3"),
            "expected pane id suffix: {path_str}"
        );
    }

    #[test]
    fn canonical_path_project_scoped() {
        let a = canonical_identity_path("/data/projects/alpha", "%0");
        let b = canonical_identity_path("/data/projects/beta", "%0");
        assert_ne!(a, b, "different projects should produce different paths");
    }

    #[test]
    fn canonical_path_composite_key_differs_from_bare() {
        let bare = canonical_identity_path("/data/projects/backend", "%3");
        let composite = canonical_identity_path("/data/projects/backend", "main:0:2");
        assert_ne!(
            bare, composite,
            "composite key should produce a different path than bare pane ID"
        );
        let composite_str = composite.to_string_lossy();
        assert!(
            composite_str.ends_with("/main-0-2"),
            "expected composite key filename: {composite_str}"
        );
    }

    #[test]
    fn canonical_path_different_sessions_differ() {
        let a = canonical_identity_path("/data/projects/backend", "session_a:0:2");
        let b = canonical_identity_path("/data/projects/backend", "session_b:0:2");
        assert_ne!(
            a, b,
            "different sessions with the same window/pane index should produce different paths"
        );
    }

    #[test]
    fn canonical_path_honors_virtual_xdg_config_home() {
        let _guard = TEST_CONFIG_BASE_DIR_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_test_config_base_dir(None);

        let tmp = tempfile::tempdir().expect("temp config home");
        let xdg_config_home = tmp.path().join("xdg-config");
        let xdg_config_home_text = xdg_config_home.to_string_lossy().into_owned();

        crate::config::with_process_env_overrides_for_test(
            &[("XDG_CONFIG_HOME", xdg_config_home_text.as_str())],
            || {
                let path = canonical_identity_path("/data/projects/backend", "%3");
                assert!(
                    path.starts_with(&xdg_config_home),
                    "canonical pane identity path ignored virtual XDG_CONFIG_HOME: {path:?}"
                );
            },
        );
    }

    #[test]
    fn canonical_path_honors_virtual_home_fallback() {
        let _guard = TEST_CONFIG_BASE_DIR_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_test_config_base_dir(None);

        let tmp = tempfile::tempdir().expect("temp home");
        let home = tmp.path().join("home");
        let home_text = home.to_string_lossy().into_owned();
        let expected_config_home = home.join(".config");

        crate::config::with_process_env_overrides_for_test(
            &[("XDG_CONFIG_HOME", ""), ("HOME", home_text.as_str())],
            || {
                let path = canonical_identity_path("/data/projects/backend", "%3");
                assert!(
                    path.starts_with(&expected_config_home),
                    "canonical pane identity path ignored virtual HOME fallback: {path:?}"
                );
            },
        );
    }

    #[test]
    fn legacy_claude_identity_honors_virtual_home() {
        let _guard = TEST_CONFIG_BASE_DIR_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_test_config_base_dir(None);

        let tmp = tempfile::tempdir().expect("temp home");
        let home = tmp.path().join("home");
        let home_text = home.to_string_lossy().into_owned();
        let identity_dir = home.join(".claude").join("agent-mail");
        std::fs::create_dir_all(&identity_dir).expect("create legacy identity dir");
        let identity_path = identity_dir.join("identity.18");
        std::fs::write(&identity_path, "BlueLake\n").expect("write legacy identity");

        crate::config::with_process_env_overrides_for_test(
            &[("XDG_CONFIG_HOME", ""), ("HOME", home_text.as_str())],
            || {
                let resolved =
                    resolve_identity_with_path("/data/projects/backend", "%18").expect("resolve");
                assert_eq!(resolved.0, "BlueLake");
                assert_eq!(resolved.1, identity_path);
            },
        );
    }

    // -- write / resolve roundtrip -------------------------------------------

    #[test]
    fn write_then_resolve_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Override config dir by writing directly to a temp path
        let identity_dir = tmp.path().join("agent-mail/identity");
        let hash = project_hash("/data/test-project");
        let pane_dir = identity_dir.join(&hash);
        std::fs::create_dir_all(&pane_dir).expect("create dirs");
        let file_path = pane_dir.join("5");
        std::fs::write(&file_path, "BlueLake\n").expect("write");

        let name = read_identity_file(&file_path);
        assert_eq!(name.as_deref(), Some("BlueLake"));
    }

    #[test]
    fn read_identity_file_missing_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nonexistent");
        assert!(read_identity_file(&path).is_none());
    }

    #[test]
    fn read_identity_file_empty_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("empty");
        std::fs::write(&path, "  \n  ").expect("write");
        assert!(read_identity_file(&path).is_none());
    }

    // -- list_identities (with isolated config dir) --------------------------

    #[test]
    fn write_then_resolve_roundtrip_composite_key() {
        let config = IsolatedConfigBaseDir::new();
        let unique_key = config.project_key("composite-project");
        let composite_pane = "test_session:0:1";
        write_identity(&unique_key, composite_pane, "GreenOwl").expect("write identity");

        let resolved = resolve_identity(&unique_key, composite_pane);
        assert_eq!(resolved.as_deref(), Some("GreenOwl"));
        drop(config);
    }

    #[test]
    fn composite_resolution_honors_virtual_bare_tmux_pane_fallback() {
        let config = IsolatedConfigBaseDir::new();
        let unique_key = config.project_key("bare-fallback-project");
        let bare_pane = "%23";
        let written_path =
            write_identity(&unique_key, bare_pane, "BlueLake").expect("write bare pane identity");

        crate::config::with_process_env_overrides_for_test(&[("TMUX_PANE", bare_pane)], || {
            let resolved =
                resolve_identity_with_path(&unique_key, "session:0:1").expect("resolve identity");
            assert_eq!(resolved.0, "BlueLake");
            assert_eq!(resolved.1, written_path);
        });

        drop(config);
    }

    #[test]
    fn list_identities_returns_entries() {
        let config = IsolatedConfigBaseDir::new();
        let unique_key = config.project_key("list-project");
        let pane = "%99";
        write_identity(&unique_key, pane, "RedFox").expect("write identity");

        let entries = list_identities(&unique_key);
        assert!(
            entries.iter().any(|(p, n)| p == "99" && n == "RedFox"),
            "expected RedFox entry: {entries:?}"
        );
        drop(config);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_identities_skips_symlinked_project_directories() {
        use std::os::unix::fs::symlink;

        let config = IsolatedConfigBaseDir::new();
        let tmux = LiveTmuxPanesGuard::new(vec!["live-pane".to_string()]);
        let unique_key = config.project_key("symlink-cleanup-project");
        let identity_root = config.tempdir.path().join(IDENTITY_DIR_NAME);
        let project_dir = identity_root.join(project_hash(&unique_key));
        let outside_dir = config.tempdir.path().join("outside-identities");
        let outside_stale = outside_dir.join("stale-pane");

        std::fs::create_dir_all(&identity_root).expect("create identity root");
        std::fs::create_dir_all(&outside_dir).expect("create outside dir");
        std::fs::write(&outside_stale, "OtherAgent\n").expect("write outside identity");
        symlink(&outside_dir, &project_dir).expect("symlink project identity dir");

        let scoped_removed = cleanup_stale_identities(&unique_key);
        assert!(
            scoped_removed.is_empty(),
            "scoped cleanup must not walk a symlinked project dir: {scoped_removed:?}"
        );
        assert!(
            outside_stale.exists(),
            "scoped cleanup must not remove files behind symlinked project dirs"
        );

        let global_removed = cleanup_all_stale_identities();
        assert!(
            global_removed.is_empty(),
            "global cleanup must not walk symlinked project dirs: {global_removed:?}"
        );
        assert!(
            outside_stale.exists(),
            "global cleanup must not remove files behind symlinked project dirs"
        );
        drop(tmux);
        drop(config);
    }

    // -- write_identity_current_pane -----------------------------------------

    #[test]
    fn current_pane_returns_none_when_no_tmux_pane_env() {
        assert!(resolve_identity_for_pane("/data/test", None).is_none());
        assert!(resolve_identity_for_pane("/data/test", Some("")).is_none());
        assert!(resolve_identity_for_pane("/data/test", Some("   ")).is_none());
    }

    #[test]
    fn resolve_identity_with_path_reports_legacy_ntm_path() {
        let tmp = tempfile::tempdir().expect("temp project key");
        let unique_key = tmp
            .path()
            .join("legacy-project")
            .to_string_lossy()
            .into_owned();
        let pane = "%42";
        let hash = project_hash(&unique_key);
        let sanitized = sanitize_pane_id(pane);
        let legacy_ntm = PathBuf::from(format!("/tmp/agent-mail-name.{hash}.{sanitized}"));
        std::fs::write(&legacy_ntm, "BlueLake\n").expect("write legacy identity");

        let resolved =
            resolve_identity_with_path(&unique_key, pane).expect("resolve legacy identity");
        assert_eq!(resolved.0, "BlueLake");
        assert_eq!(resolved.1, legacy_ntm);

        let _ = std::fs::remove_file(&resolved.1);
    }
}
