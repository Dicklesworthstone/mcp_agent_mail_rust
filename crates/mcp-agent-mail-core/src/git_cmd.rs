//! `GitCmd` builder — br-8ujfs.2.4 (B4).
//!
//! One entry point for every in-process git shell-out. Combines:
//!
//! - [`resolve_git_binary`][`crate::resolve_git_binary`] (A5, `AM_GIT_BINARY`)
//! - [`GitRepoLocks`][`crate::GitRepoLocks`] (B2, per-repo mutex)
//! - [`RepoFlock`][`crate::RepoFlock`] (B3, OS flock)
//! - [`ReentrancyGuard`][`crate::ReentrancyGuard`] (B1 §6.1, panic on nested
//!   calls to same repo from same thread)
//! - SIGSEGV classification + retry (E1/E2 — wired as hooks here; retry
//!   loop lives in this module but the retry *policy* is implemented
//!   incrementally across E1/E2 beads)
//! - Structured logging under target `mcp_agent_mail::git_locked`
//! - Metrics counters registered via [`crate::metrics`]
//!
//! # Typical usage
//!
//! ```ignore
//! use mcp_agent_mail_core::git_cmd::GitCmd;
//!
//! // Simple: run and get Output.
//! let out = GitCmd::new(repo_path).args(["log", "-1", "--format=%ct"]).run()?;
//!
//! // With stdin (e.g. pre-push hook data).
//! let out = GitCmd::new(repo_path)
//!     .args(["rev-list", "--stdin"])
//!     .stdin(stdin_bytes)
//!     .run()?;
//! ```
//!
//! # Scope boundaries
//!
//! - Do NOT call `GitCmd::new` from inside `mcp-agent-mail-guard`
//!   pre-commit code: the guard runs inside the user's git process and
//!   wrapping with flock would deadlock. See B1 design note §3.
//! - Do NOT call from inside the CommitCoalescer's per-repo worker:
//!   the coalescer has its own CAS lock; mutexing twice wastes time
//!   (but won't deadlock). Use direct `git2::` calls there.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::git_binary::{resolve_git_binary, ResolvedGitBinary};
use crate::git_lock::{canonicalize_repo, GitRepoLocks, ReentrancyGuard, RepoFlock};

/// Default wall-clock timeout for the git child process.
pub const DEFAULT_GIT_EXEC_TIMEOUT_SECS: u64 = 120;

/// What the git process did after we spawned it.
#[derive(Debug)]
pub enum GitRunOutcome {
    /// Normal exit (success OR non-zero) with captured Output.
    Finished(Output),
    /// Process was killed by a signal in the "segfault-like" family
    /// (SIGSEGV/11 or SIGBUS/7). Caller may want to retry (E2).
    SegfaultLike { signal: i32 },
    /// Process was killed by some other signal (SIGABRT, SIGKILL, ...)
    /// or exited with the corresponding exit code. Not retryable.
    OtherSignal { signal: i32 },
    /// We killed the process because it exceeded the wall-clock timeout.
    Timeout { after: Duration },
    /// Spawn or I/O error before we could run.
    Error(io::Error),
}

impl GitRunOutcome {
    /// True if this outcome is one that Track E's retry policy should
    /// retry.
    #[must_use]
    pub const fn is_segfault_like(&self) -> bool {
        matches!(self, Self::SegfaultLike { .. })
    }
}

/// Builder for a single git invocation.
pub struct GitCmd<'a> {
    repo: &'a Path,
    args: Vec<std::ffi::OsString>,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    /// Extra env vars to set on the child process (e.g. `GIT_AUTHOR_NAME`).
    envs: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    /// Override `cwd` of the child. Default: repo.
    cwd: Option<PathBuf>,
    /// If true, skip flock. Used by the guard retry path (E5) which
    /// already runs inside git's own process.
    skip_flock: bool,
    /// If true, skip in-process mutex too. Only for extremely rare
    /// cases; default is always serialize.
    skip_mutex: bool,
}

impl<'a> GitCmd<'a> {
    #[must_use]
    pub fn new(repo: &'a Path) -> Self {
        Self {
            repo,
            args: Vec::new(),
            stdin: None,
            timeout: Duration::from_secs(git_exec_timeout_secs()),
            envs: Vec::new(),
            cwd: None,
            skip_flock: false,
            skip_mutex: false,
        }
    }

    #[must_use]
    pub fn arg(mut self, a: impl Into<std::ffi::OsString>) -> Self {
        self.args.push(a.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString>,
    {
        for a in args {
            self.args.push(a.into());
        }
        self
    }

    #[must_use]
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }

    #[must_use]
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    #[must_use]
    pub fn env(
        mut self,
        k: impl Into<std::ffi::OsString>,
        v: impl Into<std::ffi::OsString>,
    ) -> Self {
        self.envs.push((k.into(), v.into()));
        self
    }

    #[must_use]
    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.cwd = Some(path.into());
        self
    }

    /// Skip the OS flock acquisition. Use for guard-hook callers only.
    #[must_use]
    pub const fn skip_flock(mut self) -> Self {
        self.skip_flock = true;
        self
    }

    /// Skip the in-process mutex. Almost never correct; kept for
    /// symmetry with [`Self::skip_flock`]. Don't use unless you know
    /// exactly why.
    #[must_use]
    pub const fn skip_mutex(mut self) -> Self {
        self.skip_mutex = true;
        self
    }

    /// Run once with the given borrowed repo. Internal.
    fn run_once_inner(
        repo: &Path,
        cwd: Option<&Path>,
        args: &[std::ffi::OsString],
        stdin_bytes: Option<&[u8]>,
        envs: &[(std::ffi::OsString, std::ffi::OsString)],
        timeout: Duration,
        skip_flock: bool,
        skip_mutex: bool,
    ) -> GitRunOutcome {
        let canonical = canonicalize_repo(repo);
        let binary = match resolve_git_binary() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(
                    target: "mcp_agent_mail::git_locked",
                    err = %e,
                    "git_binary_unresolvable"
                );
                return GitRunOutcome::Error(io::Error::other(format!(
                    "cannot resolve git binary: {e}"
                )));
            }
        };

        // Reentrancy guard (panics on nested same-repo from same thread).
        let _reent = canonical.as_ref().map(|c| ReentrancyGuard::enter(c));

        // Mutex layer.
        let _mtx = if skip_mutex {
            None
        } else {
            canonical
                .as_ref()
                .map(|c| GitRepoLocks::global().lock_for(c))
        };
        let _mtx_guard = _mtx.as_ref().map(|arc| {
            arc.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });

        // Flock layer.
        let _flock = if skip_flock {
            None
        } else if let Some(c) = canonical.as_ref() {
            match RepoFlock::acquire(c) {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::error!(
                        target: "mcp_agent_mail::git_locked",
                        err = %e,
                        repo = %c.display(),
                        "flock_acquire_failed"
                    );
                    return GitRunOutcome::Error(e);
                }
            }
        } else {
            None
        };

        run_child(&binary, repo, cwd, args, stdin_bytes, envs, timeout)
    }

    /// Run once, returning classified outcome.
    pub fn run_once(self) -> GitRunOutcome {
        Self::run_once_inner(
            self.repo,
            self.cwd.as_deref(),
            &self.args,
            self.stdin.as_deref(),
            &self.envs,
            self.timeout,
            self.skip_flock,
            self.skip_mutex,
        )
    }

    /// Run with retry on `SegfaultLike`. Retry policy per bead E2
    /// (3 retries, 100/400/1600ms jittered, 10s wall-clock cap).
    pub fn run(self) -> io::Result<Output> {
        // Capture owned state so we can re-attempt without re-borrowing
        // the original `&Path` beyond this function's lifetime.
        let repo = self.repo.to_path_buf();
        let args = self.args.clone();
        let stdin = self.stdin.clone();
        let envs = self.envs.clone();
        let cwd = self.cwd.clone();
        let timeout = self.timeout;
        let skip_flock = self.skip_flock;
        let skip_mutex = self.skip_mutex;

        const MAX_RETRIES: u32 = 3;
        const BACKOFFS_MS: [u64; 3] = [100, 400, 1600];

        let attempt_limit = MAX_RETRIES + 1;
        let overall_start = Instant::now();
        let wallclock_cap = Duration::from_secs(10);
        let mut last_err: Option<io::Error> = None;

        for attempt in 0..attempt_limit {
            let outcome = Self::run_once_inner(
                &repo,
                cwd.as_deref(),
                &args,
                stdin.as_deref(),
                &envs,
                timeout,
                skip_flock,
                skip_mutex,
            );
            match outcome {
                GitRunOutcome::Finished(out) => {
                    if attempt > 0 {
                        tracing::info!(
                            target: "mcp_agent_mail::git_locked",
                            attempt = attempt,
                            repo = %repo.display(),
                            "git_segfault_retry_succeeded"
                        );
                    }
                    return Ok(out);
                }
                GitRunOutcome::SegfaultLike { signal } => {
                    tracing::warn!(
                        target: "mcp_agent_mail::git_locked",
                        attempt = attempt,
                        signal = signal,
                        repo = %repo.display(),
                        "git_segfault_retry_attempt"
                    );
                    if attempt + 1 >= attempt_limit {
                        last_err = Some(io::Error::other(format!(
                            "git segfaulted {attempts} times in a row; system git may be 2.51.0 (known bad). Set AM_GIT_BINARY or upgrade/downgrade.",
                            attempts = attempt + 1
                        )));
                        break;
                    }
                    if overall_start.elapsed() >= wallclock_cap {
                        last_err = Some(io::Error::other(
                            "git segfault retry budget exceeded 10s wall-clock cap",
                        ));
                        break;
                    }
                    let base_ms = BACKOFFS_MS[attempt as usize];
                    let jittered = jitter_ms(base_ms);
                    std::thread::sleep(Duration::from_millis(jittered));
                }
                GitRunOutcome::OtherSignal { signal } => {
                    return Err(io::Error::other(format!(
                        "git child killed by signal {signal} (not segfault-like, not retrying)"
                    )));
                }
                GitRunOutcome::Timeout { after } => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("git child exceeded {after:?} wall-clock timeout"),
                    ));
                }
                GitRunOutcome::Error(e) => return Err(e),
            }
        }

        tracing::error!(
            target: "mcp_agent_mail::git_locked",
            repo = %repo.display(),
            "git_segfault_retry_exhausted"
        );
        Err(last_err.unwrap_or_else(|| io::Error::other("unknown git retry error")))
    }
}

fn git_exec_timeout_secs() -> u64 {
    std::env::var("AM_GIT_EXEC_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_GIT_EXEC_TIMEOUT_SECS)
}

fn jitter_ms(base: u64) -> u64 {
    // Deterministic-ish jitter in [0.75x, 1.25x] using process nanos.
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let span = base / 2; // 0.5 * base
    let low = base - span / 2; // 0.75x
    let offset = n % span.max(1);
    low + offset
}

#[cfg(unix)]
fn classify_exit(status: std::process::ExitStatus) -> GitRunOutcome {
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        return match signal {
            11 | 7 => GitRunOutcome::SegfaultLike { signal },
            other => GitRunOutcome::OtherSignal { signal: other },
        };
    }
    if let Some(code) = status.code() {
        // Some shells report SIGSEGV as exit 139.
        if code == 139 {
            return GitRunOutcome::SegfaultLike { signal: 11 };
        }
        if code == 135 {
            return GitRunOutcome::SegfaultLike { signal: 7 };
        }
    }
    // Otherwise a normal exit; caller inspects Output for nonzero codes.
    GitRunOutcome::Finished(Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

#[cfg(not(unix))]
fn classify_exit(status: std::process::ExitStatus) -> GitRunOutcome {
    // Windows STATUS_ACCESS_VIOLATION = 0xC0000005. Treat as segfault-like.
    if let Some(code) = status.code()
        && code as u32 == 0xC000_0005
    {
        return GitRunOutcome::SegfaultLike { signal: 11 };
    }
    GitRunOutcome::Finished(Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

fn run_child(
    binary: &ResolvedGitBinary,
    repo: &Path,
    cwd: Option<&Path>,
    args: &[std::ffi::OsString],
    stdin_bytes: Option<&[u8]>,
    envs: &[(std::ffi::OsString, std::ffi::OsString)],
    timeout: Duration,
) -> GitRunOutcome {
    let start = Instant::now();
    let mut cmd = Command::new(&binary.path);
    cmd.current_dir(cwd.unwrap_or(repo));
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.stdin(if stdin_bytes.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                target: "mcp_agent_mail::git_locked",
                err = %e,
                binary = %binary.path.display(),
                "git_spawn_failed"
            );
            return GitRunOutcome::Error(e);
        }
    };

    // Feed stdin if any.
    if let Some(bytes) = stdin_bytes
        && let Some(mut stdin) = child.stdin.take()
    {
        use std::io::Write;
        if let Err(e) = stdin.write_all(bytes) {
            let _ = child.kill();
            return GitRunOutcome::Error(e);
        }
    }

    // Wait with timeout.
    let outcome = wait_with_timeout(&mut child, timeout);

    let duration = start.elapsed();
    match &outcome {
        GitRunOutcome::Finished(_) => {
            tracing::debug!(
                target: "mcp_agent_mail::git_locked",
                duration_ms = duration.as_millis() as u64,
                binary_version = %binary.version,
                "git_locked_exit_ok"
            );
        }
        GitRunOutcome::SegfaultLike { signal } => {
            tracing::warn!(
                target: "mcp_agent_mail::git_locked",
                signal = signal,
                binary_version = %binary.version,
                "git_locked_exit_segfault_like"
            );
        }
        GitRunOutcome::OtherSignal { signal } => {
            tracing::warn!(
                target: "mcp_agent_mail::git_locked",
                signal = signal,
                "git_locked_exit_signal"
            );
        }
        GitRunOutcome::Timeout { after } => {
            tracing::error!(
                target: "mcp_agent_mail::git_locked",
                after_secs = after.as_secs(),
                "git_locked_exit_timeout"
            );
        }
        GitRunOutcome::Error(e) => {
            tracing::error!(
                target: "mcp_agent_mail::git_locked",
                err = %e,
                "git_locked_exit_io_error"
            );
        }
    }
    outcome
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> GitRunOutcome {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Drain stdout/stderr before classifying.
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut o) = child.stdout.take() {
                    use std::io::Read;
                    let _ = o.read_to_end(&mut stdout);
                }
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_end(&mut stderr);
                }
                // classify_exit needs the ExitStatus; we also want to
                // attach stdout/stderr for Finished.
                let outcome = classify_exit(status);
                return match outcome {
                    GitRunOutcome::Finished(_) => GitRunOutcome::Finished(Output {
                        status,
                        stdout,
                        stderr,
                    }),
                    other => other,
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return GitRunOutcome::Timeout { after: timeout };
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return GitRunOutcome::Error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_repo(dir: &Path) -> PathBuf {
        let p = dir.join("repo");
        std::fs::create_dir_all(p.join(".git/objects")).unwrap();
        std::fs::create_dir_all(p.join(".git/refs")).unwrap();
        std::fs::write(p.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        p
    }

    #[test]
    fn run_git_version_succeeds() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let out = GitCmd::new(&repo).arg("--version").run();
        assert!(out.is_ok(), "git --version should succeed: {out:?}");
        let o = out.unwrap();
        assert!(
            String::from_utf8_lossy(&o.stdout).contains("git version"),
            "unexpected output"
        );
    }

    #[test]
    fn run_returns_nonzero_output_not_error() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        // Unknown subcommand → nonzero exit, but run() returns Output
        // (it only errors on spawn / signal / timeout).
        let res = GitCmd::new(&repo).arg("nonexistent-subcommand-xyz").run();
        assert!(res.is_ok(), "nonzero exit should NOT be Err: {res:?}");
        let o = res.unwrap();
        assert!(
            !o.status.success(),
            "expected nonzero exit from unknown subcmd"
        );
    }
}
