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
//! - Do NOT call from inside the `CommitCoalescer`'s per-repo worker:
//!   the coalescer has its own CAS lock; mutexing twice wastes time
//!   (but won't deadlock). Use direct `git2::` calls there.
//! - On Unix, stdin, stdout, stderr and child exit share one execution
//!   deadline. No pipe reader/writer threads are spawned or detached. Output
//!   is limited to 64 MiB combined; `AM_GIT_MAX_OUTPUT_BYTES` can override
//!   that bound with a positive byte count. Exceeding it is an error, never
//!   successful truncated output. Lock acquisition is outside this deadline.
//!   Other platforms retain their existing subprocess implementation.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::git_binary::{ResolvedGitBinary, resolve_git_binary};
use crate::git_lock::{GitRepoLocks, ReentrancyGuard, RepoFlock, canonicalize_repo};

/// Default wall-clock timeout for the git child process.
pub const DEFAULT_GIT_EXEC_TIMEOUT_SECS: u64 = 120;

/// Default combined stdout/stderr capture bound for Unix git invocations.
#[cfg(unix)]
pub const DEFAULT_GIT_MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

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
    /// The child or its inherited pipes exceeded the execution deadline.
    Timeout { after: Duration },
    /// Spawn, capture-limit or I/O error.
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
    pub const fn timeout(mut self, t: Duration) -> Self {
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

    /// Skip the OS flock acquisition. Use only for guard-hook callers or
    /// provably read-only probes whose contract forbids creating the flock
    /// sentinel itself (for example a dry-run safety preflight).
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
    #[allow(clippy::too_many_arguments)]
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
        let mtx = if skip_mutex {
            None
        } else {
            canonical
                .as_ref()
                .map(|c| GitRepoLocks::global().lock_for(c))
        };
        let _mtx_guard = mtx.as_ref().map(|arc| {
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
    #[must_use]
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
        const MAX_RETRIES: u32 = 3;
        const BACKOFFS_MS: [u64; 3] = [100, 400, 1600];

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
                        format!("git child or output pipes exceeded {after:?} wall-clock timeout"),
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

#[cfg(unix)]
fn parse_git_output_limit(raw: Option<&str>) -> usize {
    raw.and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_GIT_MAX_OUTPUT_BYTES)
}

fn jitter_ms(base: u64) -> u64 {
    // Deterministic-ish jitter in [0.75x, 1.25x] using process nanos.
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    let span = base / 2; // 0.5 * base
    let low = base - span / 2; // 0.75x
    let offset = n % span.max(1);
    low + offset
}

fn duration_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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
        && code.cast_unsigned() == 0xC000_0005
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

    #[cfg(unix)]
    let outcome = run_piped_command(
        cmd,
        stdin_bytes,
        timeout,
        parse_git_output_limit(std::env::var("AM_GIT_MAX_OUTPUT_BYTES").ok().as_deref()),
    );
    #[cfg(not(unix))]
    let outcome = {
        cmd.stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => return GitRunOutcome::Error(error),
        };
        if let Some(bytes) = stdin_bytes
            && let Some(mut stdin) = child.stdin.take()
        {
            use std::io::Write;
            if let Err(error) = stdin.write_all(bytes) {
                drop(stdin);
                let _ = child.kill();
                let _ = child.wait();
                return GitRunOutcome::Error(error);
            }
        }
        wait_with_timeout(&mut child, timeout)
    };

    let duration = start.elapsed();
    match &outcome {
        GitRunOutcome::Finished(_) => {
            tracing::debug!(
                target: "mcp_agent_mail::git_locked",
                duration_ms = duration_ms_u64(duration),
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

/// Only the parent endpoint is nonblocking. Git and its hooks keep ordinary
/// blocking stdio semantics; no flags are changed on their pipe endpoints.
#[cfg(unix)]
fn nonblocking_parent_pipe(input: bool) -> io::Result<(io::PipeReader, io::PipeWriter)> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

    let (reader, writer) = io::pipe()?;
    if input {
        let flags = fcntl_getfl(&writer)?;
        fcntl_setfl(&writer, flags | OFlags::NONBLOCK)?;
    } else {
        let flags = fcntl_getfl(&reader)?;
        fcntl_setfl(&reader, flags | OFlags::NONBLOCK)?;
    }
    Ok((reader, writer))
}

/// Owns just the direct child, never a caller's process group. Drop also
/// covers an unwinding capture path; no child is abandoned on an I/O error.
#[cfg(unix)]
struct ReapedGitChild(Child);

#[cfg(unix)]
impl Drop for ReapedGitChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn run_piped_command(
    mut command: Command,
    stdin_bytes: Option<&[u8]>,
    timeout: Duration,
    output_limit: usize,
) -> GitRunOutcome {
    let prepared = (|| -> io::Result<_> {
        let (stdout, stdout_writer) = nonblocking_parent_pipe(false)?;
        let (stderr, stderr_writer) = nonblocking_parent_pipe(false)?;
        command.stdout(stdout_writer).stderr(stderr_writer);
        let stdin = if stdin_bytes.is_some() {
            let (reader, writer) = nonblocking_parent_pipe(true)?;
            command.stdin(reader);
            Some(writer)
        } else {
            command.stdin(Stdio::null());
            None
        };
        Ok((stdin, stdout, stderr))
    })();
    let (stdin, stdout, stderr) = match prepared {
        Ok(pipes) => pipes,
        Err(error) => return GitRunOutcome::Error(error),
    };
    let mut child = match command.spawn() {
        Ok(child) => ReapedGitChild(child),
        Err(error) => return GitRunOutcome::Error(error),
    };
    // Explicit Stdio handles remain owned by Command after spawn. In
    // particular its writer copies would keep EOF unreachable forever.
    drop(command);
    capture_pipes(
        &mut child.0,
        stdin,
        stdin_bytes.unwrap_or_default(),
        stdout,
        stderr,
        timeout,
        output_limit,
    )
}

/// Read at most one chunk per turn, so continuous stdout cannot starve
/// stderr, stdin, child reaping or the deadline. False means no progress.
#[cfg(unix)]
fn drain_pipe(
    pipe: &mut Option<io::PipeReader>,
    output: &mut Vec<u8>,
    remaining: &mut usize,
) -> io::Result<bool> {
    use std::io::Read;

    let Some(reader) = pipe else {
        return Ok(false);
    };
    let mut buffer = [0_u8; 8192];
    match reader.read(&mut buffer) {
        Ok(0) => {
            *pipe = None;
            Ok(true)
        }
        Ok(length) => {
            if length > *remaining {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "GIT_OUTPUT_LIMIT: combined stdout/stderr exceeded AM_GIT_MAX_OUTPUT_BYTES",
                ));
            }
            output
                .try_reserve_exact(length)
                .map_err(|error| io::Error::other(format!("git capture allocation failed: {error}")))?;
            output.extend_from_slice(&buffer[..length]);
            *remaining -= length;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn capture_pipes(
    child: &mut Child,
    mut stdin: Option<io::PipeWriter>,
    mut input: &[u8],
    stdout: io::PipeReader,
    stderr: io::PipeReader,
    timeout: Duration,
    output_limit: usize,
) -> GitRunOutcome {
    use std::io::Write;

    let started = Instant::now();
    let mut stdout = Some(stdout);
    let mut stderr = Some(stderr);
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut remaining = output_limit;
    let mut status = None;
    let mut pause = Duration::from_millis(1);
    loop {
        if input.is_empty() {
            // Closing the actual parent writer is what gives Git stdin EOF.
            stdin = None;
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(observed) => status = observed,
                Err(error) => return GitRunOutcome::Error(error),
            }
        }
        if let Some(exited) = status
            && stdout.is_none()
            && stderr.is_none()
        {
            return match classify_exit(exited) {
                GitRunOutcome::Finished(_) => GitRunOutcome::Finished(Output {
                    status: exited,
                    stdout: stdout_bytes,
                    stderr: stderr_bytes,
                }),
                other => other,
            };
        }
        // Compare elapsed durations instead of adding to Instant: even a
        // caller-supplied Duration::MAX cannot overflow the deadline.
        let time_left = timeout.saturating_sub(started.elapsed());
        if time_left.is_zero() {
            return GitRunOutcome::Timeout { after: timeout };
        }
        let mut progressed = false;
        if let Some(writer) = stdin.as_mut() {
            let length = input.len().min(8192);
            match writer.write(&input[..length]) {
                Ok(0) => {
                    return GitRunOutcome::Error(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "git stdin accepted zero bytes before input was complete",
                    ));
                }
                Ok(written) => {
                    input = &input[written..];
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => progressed = true,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return GitRunOutcome::Error(error),
            }
        }
        for (pipe, bytes) in [
            (&mut stdout, &mut stdout_bytes),
            (&mut stderr, &mut stderr_bytes),
        ] {
            match drain_pipe(pipe, bytes, &mut remaining) {
                Ok(progress) => progressed |= progress,
                Err(error) => return GitRunOutcome::Error(error),
            }
        }
        if progressed {
            pause = Duration::from_millis(1);
        } else {
            std::thread::sleep(pause.min(timeout.saturating_sub(started.elapsed())));
            pause = (pause * 2).min(Duration::from_millis(20));
        }
    }
}

/// Legacy non-Unix give-up path. Unix capture above has no reader threads.
#[cfg(not(unix))]
fn join_readers_bounded(
    stdout: Option<std::thread::JoinHandle<Vec<u8>>>,
    stderr: Option<std::thread::JoinHandle<Vec<u8>>>,
) {
    for handle in [stdout, stderr].into_iter().flatten() {
        let deadline = Instant::now() + READER_EOF_GRACE;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(not(unix))]
const READER_EOF_GRACE: Duration = Duration::from_millis(250);

#[cfg(not(unix))]
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> GitRunOutcome {
    use std::io::Read;

    let mut stdout_handle = child.stdout.take().map(|mut o| {
        std::thread::spawn(move || {
            let mut buf = Vec::with_capacity(4096);
            let _ = o.read_to_end(&mut buf);
            buf
        })
    });
    let mut stderr_handle = child.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut buf = Vec::with_capacity(4096);
            let _ = e.read_to_end(&mut buf);
            buf
        })
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    join_readers_bounded(stdout_handle.take(), stderr_handle.take());
                    return GitRunOutcome::Timeout { after: timeout };
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                join_readers_bounded(stdout_handle.take(), stderr_handle.take());
                return GitRunOutcome::Error(e);
            }
        }
    };

    let stdout_bytes = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr_bytes = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    match classify_exit(status) {
        GitRunOutcome::Finished(_) => GitRunOutcome::Finished(Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        }),
        other => other,
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
        let res = GitCmd::new(&repo).arg("nonexistent-subcommand-xyz").run();
        assert!(res.is_ok(), "nonzero exit should NOT be Err: {res:?}");
        let o = res.unwrap();
        assert!(
            !o.status.success(),
            "expected nonzero exit from unknown subcmd"
        );
    }

    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[cfg(unix)]
    fn finished(outcome: GitRunOutcome) -> Output {
        match outcome {
            GitRunOutcome::Finished(output) => output,
            other => panic!("expected complete output, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn large_stdin_and_both_output_pipes_make_progress_together() {
        let input = vec![b'x'; 2 * 1024 * 1024];
        let output = finished(run_piped_command(
            shell(
                "dd if=/dev/zero bs=65536 count=16 2>/dev/null; \
                 dd if=/dev/zero bs=65536 count=16 2>/dev/null >&2; cat",
            ),
            Some(&input),
            Duration::from_secs(10),
            4 * 1024 * 1024,
        ));
        assert!(output.status.success());
        assert_eq!(&output.stdout[..1024 * 1024], vec![0; 1024 * 1024]);
        assert_eq!(&output.stdout[1024 * 1024..], input);
        assert_eq!(output.stderr, vec![0; 1024 * 1024]);
    }

    #[cfg(unix)]
    #[test]
    fn blocked_stdin_is_cut_off_by_the_execution_deadline() {
        let started = Instant::now();
        let outcome = run_piped_command(
            shell("exec sleep 30"),
            Some(&vec![b'x'; 1024 * 1024]),
            Duration::from_millis(100),
            1024,
        );
        assert!(matches!(outcome, GitRunOutcome::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn exited_child_with_retained_output_writer_still_has_a_deadline() {
        // Model a hook retaining inherited stdout with an owned writer in
        // this test, rather than leaking a real orphan process to init.
        let (reader, retained_writer) = nonblocking_parent_pipe(false).unwrap();
        let (stderr, stderr_writer) = nonblocking_parent_pipe(false).unwrap();
        let mut command = shell("exit 0");
        command
            .stdin(Stdio::null())
            .stdout(retained_writer.try_clone().unwrap())
            .stderr(stderr_writer);
        let mut child = ReapedGitChild(command.spawn().unwrap());
        drop(command);
        let started = Instant::now();
        let outcome = capture_pipes(
            &mut child.0,
            None,
            &[],
            reader,
            stderr,
            Duration::from_millis(100),
            1024,
        );
        assert!(matches!(outcome, GitRunOutcome::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(child.0.try_wait().unwrap().unwrap().success());
        drop(retained_writer);
    }

    #[cfg(unix)]
    #[test]
    fn combined_capture_limit_is_exact_and_never_returns_partial_success() {
        let output = finished(run_piped_command(
            shell("printf abc; printf def >&2"),
            None,
            Duration::from_secs(5),
            6,
        ));
        assert_eq!(output.stdout, b"abc");
        assert_eq!(output.stderr, b"def");
        let limited = run_piped_command(
            shell("printf abc; printf def >&2"),
            None,
            Duration::from_secs(5),
            5,
        );
        match limited {
            GitRunOutcome::Error(error) => {
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert!(error.to_string().contains("GIT_OUTPUT_LIMIT"));
            }
            other => panic!("over-limit capture must fail, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn empty_stdin_closes_and_nonzero_exit_keeps_stderr() {
        let output = finished(run_piped_command(
            shell("cat; printf problem >&2; exit 7"),
            Some(&[]),
            Duration::from_secs(5),
            1024,
        ));
        assert_eq!(output.status.code(), Some(7));
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"problem");
    }

    #[cfg(unix)]
    #[test]
    fn continuous_output_cannot_bypass_capture_limit() {
        let outcome = run_piped_command(
            shell("while :; do printf '0123456789abcdef'; done"),
            None,
            Duration::from_secs(5),
            64 * 1024,
        );
        assert!(matches!(
            outcome,
            GitRunOutcome::Error(error) if error.kind() == io::ErrorKind::InvalidData
        ));
    }

    #[cfg(unix)]
    #[test]
    fn maximum_timeout_does_not_overflow_instant() {
        let output = finished(run_piped_command(
            shell("printf complete"),
            None,
            Duration::MAX,
            1024,
        ));
        assert_eq!(output.stdout, b"complete");
    }

    #[cfg(unix)]
    #[test]
    fn output_limit_overrides_require_a_positive_byte_count() {
        for raw in [None, Some(""), Some("0"), Some("-1"), Some("garbage")] {
            assert_eq!(parse_git_output_limit(raw), DEFAULT_GIT_MAX_OUTPUT_BYTES);
        }
        assert_eq!(parse_git_output_limit(Some(" 4096 ")), 4096);
    }
}
