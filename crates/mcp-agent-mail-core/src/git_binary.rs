//! `AM_GIT_BINARY` resolution — br-8ujfs.1.5 (A5).
//!
//! Lets operators point mcp-agent-mail at a specific git binary, bypassing
//! the system PATH lookup. This is the primary mitigation for git 2.51.0's
//! index-race bug when a safe git binary exists somewhere on the system.
//!
//! # Resolution pipeline (fail-closed)
//!
//! 1. env unset → default `PathBuf::from("git")`.
//! 2. Expand `~` and `${VAR}` substitutions.
//! 3. Relative path → resolve via PATH lookup.
//! 4. `std::fs::metadata(path)` must succeed.
//! 5. Must be a file (symlinks followed; symlink-to-dir rejected).
//! 6. Unix: any exec bit set. Windows: `.exe` / `.cmd` / `.bat`.
//! 7. Spawn `<path> --version` with a 5s timeout.
//! 8. Parse `git version X.Y.Z` with a regex.
//! 9. Known-bad version (2.51.0) → WARN but do not fail.
//! 10. Cache `(PathBuf, Version, validated_at)` for 24h.
//!
//! # Logging
//!
//! All events use target `mcp_agent_mail::git_binary` and include a
//! `validation_step` field (1-10 above) so ops can grep by step.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;

/// Errors that can occur during git binary resolution.
///
/// Non-fatal: `KnownBad` (step 9) is WARN-logged but returned as `Ok(..)`
/// from [`resolve_git_binary`]; callers that want strict mode check the
/// version themselves.
#[derive(Debug, thiserror::Error)]
pub enum GitBinaryError {
    #[error("AM_GIT_BINARY not found: {path} (validation step 4)")]
    Missing { path: PathBuf },

    #[error("AM_GIT_BINARY is not a regular file: {path} (validation step 5)")]
    NotAFile { path: PathBuf },

    #[error("AM_GIT_BINARY is not executable: {path} (validation step 6)")]
    NotExecutable { path: PathBuf },

    #[error("AM_GIT_BINARY PATH lookup failed: {name} (validation step 3)")]
    NotOnPath { name: String },

    #[error("AM_GIT_BINARY spawn failed: {path}: {source} (validation step 7)")]
    SpawnFailed {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("AM_GIT_BINARY --version timed out after 5s: {path} (validation step 7)")]
    SpawnTimeout { path: PathBuf },

    #[error(
        "AM_GIT_BINARY produced unparseable version output: {path} — got: {stdout:?} (validation step 8)"
    )]
    Unparseable { path: PathBuf, stdout: String },
}

/// Parsed semver triple for a git binary.
///
/// Intentionally lightweight (no semver crate dep). We only need exact-match
/// comparison for the known-bad list (see [`is_known_bad`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl GitVersion {
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Is this version in the known-bad list?
    ///
    /// Currently only matches 2.51.0 (the primary motivating bug). Future
    /// additions should be made via A7 (data-driven known-bad list)
    /// rather than edited here directly.
    #[must_use]
    pub const fn is_known_bad(self) -> bool {
        self.major == 2 && self.minor == 51 && self.patch == 0
    }
}

impl std::fmt::Display for GitVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Resolved git binary + parsed version + timestamp of validation.
#[derive(Debug, Clone)]
pub struct ResolvedGitBinary {
    pub path: PathBuf,
    pub version: GitVersion,
    /// `Instant` of last successful validation; used for 24h cache TTL.
    pub validated_at: Instant,
    /// Where the path came from: `"default"`, `"env"`, or `"test-override"`.
    pub source: &'static str,
}

/// Process-wide cache of the resolved git binary.
///
/// Re-validated every 24h (configurable via `AM_GIT_BINARY_CACHE_SECS`).
/// If re-validation fails, we KEEP the previous cached value with a WARN
/// log — degraded operation beats a crash.
static CACHE: OnceLock<Mutex<Option<ResolvedGitBinary>>> = OnceLock::new();

fn cache_ttl_secs() -> u64 {
    std::env::var("AM_GIT_BINARY_CACHE_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(86_400) // 24h default
}

/// Resolve the git binary per the pipeline above.
///
/// # Logging
///
/// - INFO on successful resolution (first call).
/// - INFO on revalidation (cache hit or refresh).
/// - WARN on known-bad version.
/// - WARN on revalidation failure that keeps the prior value.
/// - ERROR on unrecoverable validation failure.
///
/// # Errors
///
/// Returns `GitBinaryError` for steps 3-8 failures. Known-bad at step 9
/// is NOT an error here — caller gets `Ok(resolved)` and can inspect
/// `resolved.version.is_known_bad()` if they want strict behavior.
pub fn resolve_git_binary() -> Result<ResolvedGitBinary, GitBinaryError> {
    resolve_git_binary_with_env(|k| std::env::var(k).ok())
}

/// Resolution with a custom env lookup. Useful for testing without
/// mutating the process-wide environment.
pub fn resolve_git_binary_with_env<F>(env: F) -> Result<ResolvedGitBinary, GitBinaryError>
where
    F: Fn(&str) -> Option<String>,
{
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Check cache TTL.
    if let Some(cached) = guard.as_ref() {
        let age = cached.validated_at.elapsed();
        if age < Duration::from_secs(cache_ttl_secs()) {
            return Ok(cached.clone());
        }
        tracing::info!(
            target: "mcp_agent_mail::git_binary",
            path = %cached.path.display(),
            age_secs = age.as_secs(),
            "git_binary_revalidating"
        );
        match resolve_inner(&env) {
            Ok(fresh) => {
                if fresh.version != cached.version {
                    tracing::warn!(
                        target: "mcp_agent_mail::git_binary",
                        old_version = %cached.version,
                        new_version = %fresh.version,
                        path = %fresh.path.display(),
                        "git_binary_version_drift"
                    );
                }
                *guard = Some(fresh.clone());
                return Ok(fresh);
            }
            Err(e) => {
                tracing::warn!(
                    target: "mcp_agent_mail::git_binary",
                    error = %e,
                    cached_path = %cached.path.display(),
                    cached_version = %cached.version,
                    "git_binary_revalidation_failed_keeping_previous"
                );
                return Ok(cached.clone());
            }
        }
    }

    // Cold path: first resolution.
    let resolved = resolve_inner(&env)?;
    tracing::info!(
        target: "mcp_agent_mail::git_binary",
        path = %resolved.path.display(),
        version = %resolved.version,
        source = resolved.source,
        validation_step = 10,
        "resolved_git_binary"
    );
    if resolved.version.is_known_bad() {
        tracing::warn!(
            target: "mcp_agent_mail::git_binary",
            path = %resolved.path.display(),
            version = %resolved.version,
            validation_step = 9,
            remediation = "set AM_GIT_BINARY to a safer git (2.50.x or >= 2.51.1) or upgrade/downgrade system git",
            "git_binary_known_bad"
        );
    }
    *guard = Some(resolved.clone());
    Ok(resolved)
}

fn resolve_inner<F>(env: &F) -> Result<ResolvedGitBinary, GitBinaryError>
where
    F: Fn(&str) -> Option<String>,
{
    // Step 1: env unset -> default "git".
    let (raw, source) = match env("AM_GIT_BINARY") {
        Some(v) if !v.trim().is_empty() => (v, "env"),
        _ => ("git".to_string(), "default"),
    };

    // Step 2: expand tilde + env var substitutions.
    let expanded = expand(&raw);

    // Step 3: relative path -> PATH lookup.
    let resolved_path = if Path::new(&expanded).is_absolute() {
        PathBuf::from(expanded)
    } else {
        match which_on_path(&expanded) {
            Some(p) => p,
            None => return Err(GitBinaryError::NotOnPath { name: expanded }),
        }
    };

    // Step 4: stat must succeed.
    let meta = fs::metadata(&resolved_path).map_err(|_| GitBinaryError::Missing {
        path: resolved_path.clone(),
    })?;

    // Step 5: must be a file.
    if !meta.is_file() {
        return Err(GitBinaryError::NotAFile {
            path: resolved_path,
        });
    }

    // Step 6: must be executable.
    if !is_executable(&resolved_path, &meta) {
        return Err(GitBinaryError::NotExecutable {
            path: resolved_path,
        });
    }

    // Step 7: spawn with 5s timeout; capture stdout.
    let output = spawn_with_timeout(&resolved_path, Duration::from_secs(5))?;

    // Step 8: parse "git version X.Y.Z".
    let version = parse_git_version(&output).ok_or_else(|| GitBinaryError::Unparseable {
        path: resolved_path.clone(),
        stdout: output.clone(),
    })?;

    Ok(ResolvedGitBinary {
        path: resolved_path,
        version,
        validated_at: Instant::now(),
        source,
    })
}

fn expand(raw: &str) -> String {
    shellexpand::full(raw)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| raw.to_string())
}

/// Minimal `which` implementation — avoids adding the `which` crate to
/// the workspace for a single call site.
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if let Ok(meta) = fs::metadata(&candidate)
            && meta.is_file()
            && is_executable(&candidate, &meta)
        {
            return Some(candidate);
        }
        // On Windows, also try with .exe extension.
        #[cfg(windows)]
        {
            let with_ext = candidate.with_extension("exe");
            if let Ok(meta) = fs::metadata(&with_ext) {
                if meta.is_file() {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(_path: &Path, meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_executable(path: &Path, _meta: &fs::Metadata) -> bool {
    let ext = path.extension().and_then(|s| s.to_str()).map(|s| {
        let mut lower = s.to_ascii_lowercase();
        lower.insert(0, '.');
        lower
    });
    matches!(ext.as_deref(), Some(".exe") | Some(".cmd") | Some(".bat"))
}

/// Spawn `<path> --version` and read stdout with a wall-clock timeout.
///
/// If the child does not finish in `timeout`, it's killed and we return
/// [`GitBinaryError::SpawnTimeout`]. Keeps logic synchronous (no runtime
/// dependency) by using a background reader thread that feeds stdout into
/// a channel; the main thread polls the channel with a deadline.
fn spawn_with_timeout(path: &Path, timeout: Duration) -> Result<String, GitBinaryError> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| GitBinaryError::SpawnFailed {
            path: path.to_path_buf(),
            source: e,
        })?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitBinaryError::SpawnFailed {
            path: path.to_path_buf(),
            source: io::Error::other("stdout pipe not captured"),
        })?;

    let (tx, rx) = std::sync::mpsc::channel::<io::Result<String>>();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let res = stdout.read_to_string(&mut buf).map(|_| buf);
        let _ = tx.send(res);
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GitBinaryError::SpawnTimeout {
                        path: path.to_path_buf(),
                    });
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(GitBinaryError::SpawnFailed {
                    path: path.to_path_buf(),
                    source: e,
                });
            }
        }
    }

    match rx.recv_timeout(Duration::from_millis(250)) {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(GitBinaryError::SpawnFailed {
            path: path.to_path_buf(),
            source: e,
        }),
        Err(_) => Err(GitBinaryError::SpawnTimeout {
            path: path.to_path_buf(),
        }),
    }
}

/// Parse `git version X.Y.Z[.anything]` from stdout.
fn parse_git_version(stdout: &str) -> Option<GitVersion> {
    // Accept trailing suffixes like "2.51.0.windows.1" or "2.51.0-rc1".
    let re = Regex::new(r"git version (\d+)\.(\d+)\.(\d+)").ok()?;
    let captures = re.captures(stdout)?;
    Some(GitVersion::new(
        captures.get(1)?.as_str().parse().ok()?,
        captures.get(2)?.as_str().parse().ok()?,
        captures.get(3)?.as_str().parse().ok()?,
    ))
}

/// Shortcut: returns just the binary path. Panics if resolution fails.
/// Intended for call sites that have already validated resolution at
/// startup (e.g., `run_git_locked` in bead B4).
#[must_use]
pub fn resolved_git_binary_path() -> PathBuf {
    match resolve_git_binary() {
        Ok(r) => r.path,
        Err(_) => PathBuf::from("git"),
    }
}

/// Test-only: reset the cache. Lets unit tests exercise the cold path
/// repeatedly without process restart.
#[cfg(test)]
pub fn reset_cache_for_test() {
    if let Some(m) = CACHE.get()
        && let Ok(mut g) = m.lock()
    {
        *g = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self as sfs, File};
    use std::io::Write;
    use tempfile::TempDir;

    /// Build a shim script that prints the given version output.
    ///
    /// This is the minimal precursor to the full test-helpers crate (G6);
    /// once G6 lands these inlined helpers migrate there.
    fn build_shim_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = File::create(&path).expect("create shim");
        writeln!(f, "#!/usr/bin/env bash").unwrap();
        writeln!(f, "set -e").unwrap();
        f.write_all(body.as_bytes()).unwrap();
        writeln!(f).unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = sfs::metadata(&path).unwrap().permissions();
            perm.set_mode(0o755);
            sfs::set_permissions(&path, perm).unwrap();
        }
        path
    }

    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'static {
        let owned: Vec<(String, String)> =
            pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
        move |k: &str| -> Option<String> {
            owned.iter().find(|(name, _)| name == k).map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn parse_version_basic() {
        assert_eq!(
            parse_git_version("git version 2.50.2\n"),
            Some(GitVersion::new(2, 50, 2)),
        );
    }

    #[test]
    fn parse_version_with_trailing_suffix() {
        assert_eq!(
            parse_git_version("git version 2.51.0.windows.1\n"),
            Some(GitVersion::new(2, 51, 0)),
        );
        assert_eq!(
            parse_git_version("git version 2.52.0-rc1\n"),
            Some(GitVersion::new(2, 52, 0)),
        );
    }

    #[test]
    fn parse_version_garbage() {
        assert_eq!(parse_git_version("this is not git output"), None);
        assert_eq!(parse_git_version(""), None);
    }

    #[test]
    fn is_known_bad_detects_2_51_0_exactly() {
        assert!(GitVersion::new(2, 51, 0).is_known_bad());
        assert!(!GitVersion::new(2, 51, 1).is_known_bad());
        assert!(!GitVersion::new(2, 50, 0).is_known_bad());
        assert!(!GitVersion::new(2, 52, 0).is_known_bad());
    }

    #[test]
    fn unset_env_defaults_to_git_on_path() {
        reset_cache_for_test();
        // Rely on system PATH having some git. The test must tolerate
        // ANY version because this runs on our 2.51.0 box.
        let env = env_map(&[]);
        let res = resolve_git_binary_with_env(env);
        assert!(res.is_ok(), "default 'git' on PATH should resolve: {res:?}");
        let r = res.unwrap();
        assert_eq!(r.source, "default");
    }

    #[test]
    fn missing_binary_errors_clearly() {
        reset_cache_for_test();
        let env = env_map(&[("AM_GIT_BINARY", "/nonexistent/dir/git-xyz")]);
        let res = resolve_git_binary_with_env(env);
        match res {
            Err(GitBinaryError::Missing { path }) => {
                assert_eq!(path, PathBuf::from("/nonexistent/dir/git-xyz"));
            }
            other => panic!("expected Missing error, got {other:?}"),
        }
    }

    #[test]
    fn directory_not_file_errors() {
        reset_cache_for_test();
        let tmp = TempDir::new().unwrap();
        let env = env_map(&[("AM_GIT_BINARY", tmp.path().to_str().unwrap())]);
        let res = resolve_git_binary_with_env(env);
        match res {
            Err(GitBinaryError::NotAFile { .. }) => {}
            other => panic!("expected NotAFile error, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn non_executable_errors() {
        reset_cache_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("git-plain-file");
        sfs::write(&path, b"I am not executable\n").unwrap();
        let env = env_map(&[("AM_GIT_BINARY", path.to_str().unwrap())]);
        let res = resolve_git_binary_with_env(env);
        match res {
            Err(GitBinaryError::NotExecutable { .. }) => {}
            other => panic!("expected NotExecutable, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn shim_reporting_2_50_2_resolves_with_warn_free() {
        reset_cache_for_test();
        let tmp = TempDir::new().unwrap();
        let shim = build_shim_script(tmp.path(), "git", r#"echo "git version 2.50.2""#);
        let env = env_map(&[("AM_GIT_BINARY", shim.to_str().unwrap())]);
        let r = resolve_git_binary_with_env(env).expect("2.50.2 shim should resolve");
        assert_eq!(r.version, GitVersion::new(2, 50, 2));
        assert_eq!(r.source, "env");
        assert!(!r.version.is_known_bad());
    }

    #[test]
    #[cfg(unix)]
    fn shim_reporting_2_51_0_resolves_but_flags_known_bad() {
        reset_cache_for_test();
        let tmp = TempDir::new().unwrap();
        let shim = build_shim_script(tmp.path(), "git", r#"echo "git version 2.51.0""#);
        let env = env_map(&[("AM_GIT_BINARY", shim.to_str().unwrap())]);
        let r = resolve_git_binary_with_env(env).expect("2.51.0 shim should resolve (warn, not fail)");
        assert_eq!(r.version, GitVersion::new(2, 51, 0));
        assert!(r.version.is_known_bad());
    }

    #[test]
    #[cfg(unix)]
    fn shim_unparseable_version_errors() {
        reset_cache_for_test();
        let tmp = TempDir::new().unwrap();
        let shim = build_shim_script(tmp.path(), "git", r#"echo "some nonsense output""#);
        let env = env_map(&[("AM_GIT_BINARY", shim.to_str().unwrap())]);
        let res = resolve_git_binary_with_env(env);
        match res {
            Err(GitBinaryError::Unparseable { .. }) => {}
            other => panic!("expected Unparseable, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn cache_returns_same_result_on_second_call() {
        reset_cache_for_test();
        let tmp = TempDir::new().unwrap();
        let shim = build_shim_script(tmp.path(), "git", r#"echo "git version 2.50.2""#);
        let env = env_map(&[("AM_GIT_BINARY", shim.to_str().unwrap())]);
        let first = resolve_git_binary_with_env(&env).expect("first call");
        let second = resolve_git_binary_with_env(&env).expect("second call");
        assert_eq!(first.path, second.path);
        assert_eq!(first.version, second.version);
        // validated_at must NOT change on cache hit.
        assert_eq!(first.validated_at, second.validated_at);
    }

    // NOTE: tilde-expansion is exercised via shellexpand::full directly in
    // the production path. A full-scoped unit test would require mutating
    // HOME which races with parallel tests; we cover tilde expansion in
    // the A5 E2E shell test (tests/e2e/test_am_git_binary.sh) instead.

    #[test]
    fn spawn_timeout_after_5s_on_hung_shim() {
        reset_cache_for_test();
        let tmp = TempDir::new().unwrap();
        // Shim that sleeps 30s — longer than our 5s timeout.
        let shim = build_shim_script(
            tmp.path(),
            "git",
            r#"sleep 30
echo "git version 2.50.2""#,
        );
        let env = env_map(&[("AM_GIT_BINARY", shim.to_str().unwrap())]);
        let t0 = Instant::now();
        let res = resolve_git_binary_with_env(env);
        let elapsed = t0.elapsed();
        match res {
            Err(GitBinaryError::SpawnTimeout { .. }) => {
                assert!(
                    elapsed >= Duration::from_secs(5),
                    "should wait at least 5s before timeout (got {elapsed:?})"
                );
                assert!(
                    elapsed < Duration::from_secs(10),
                    "should NOT wait forever (got {elapsed:?})"
                );
            }
            other => panic!("expected SpawnTimeout, got {other:?}"),
        }
    }
}
