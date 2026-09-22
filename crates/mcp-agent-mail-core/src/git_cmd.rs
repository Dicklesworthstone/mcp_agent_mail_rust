//! `GitCmd` builder — br-8ujfs.2.4 (B4).
//!
//! One entry point for every in-process git shell-out. Combines:
//!
//! - [`resolve_git_binary`][`crate::resolve_git_binary`] (A5, `AM_GIT_BINARY`)
//! - [`GitRepoLocks`][`crate::GitRepoLocks`] (B2, per-repo mutex)
//! - [`RepoFlock`][`crate::RepoFlock`] (B3, OS flock)
//! - [`ReentrancyGuard`][`crate::ReentrancyGuard`] (B1 §6.1, panic on nested
//!   calls to same repo from same thread)
//! - SIGSEGV classification + bounded retry (E1/E2)
//! - Structured logging under target `mcp_agent_mail::git_locked`
//!
//! # Typical usage
//!
//! ```ignore
//! use mcp_agent_mail_core::git_cmd::GitCmd;
//! let out = GitCmd::new(repo_path).args(["log", "-1", "--format=%ct"]).run()?;
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
//!   the coalescer has its own CAS lock; use direct `git2::` calls there.
//! - The configured timeout is one invocation budget. Registry/repository lock
//!   waits, flock, and Unix stdin/stdout/stderr/child execution spend that same
//!   budget. Segfault retries also obey a ten-second window from its origin.
//!   A healthy first attempt is not restricted to that retry-only window.
//! - No Unix pipe or lock-wait threads are spawned or detached. Output is
//!   limited to 64 MiB combined; `AM_GIT_MAX_OUTPUT_BYTES` can override the
//!   positive byte bound. Exceeding it is an error, never truncated success.
//! - The budget is cooperative: it cannot preempt filesystem/binary-resolution
//!   syscalls, spawning, diagnostic callbacks, or child reaping. Expiry is
//!   rechecked before later work. Non-Unix blocking stdin and reader-thread
//!   behavior remain unchanged and do not establish an end-to-end pipe deadline.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::git_binary::{ResolvedGitBinary, resolve_git_binary};
use crate::git_lock::{
    GitRepoLocks, ReentrancyGuard, RepoFlock, canonicalize_repo, flock_timeout_secs,
    lock_mutex_with_timeout,
};

/// Default invocation budget, including lock admission and Unix pipe execution.
pub const DEFAULT_GIT_EXEC_TIMEOUT_SECS: u64 = 120;
const SEGFAULT_RETRY_WINDOW: Duration = Duration::from_secs(10);
const SEGFAULT_BACKOFFS_MS: [u64; 3] = [100, 400, 1600];

/// Default combined stdout/stderr capture bound for Unix git invocations.
#[cfg(unix)]
pub const DEFAULT_GIT_MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// One monotonic origin survives lock admission, child setup and retries.
/// Duration arithmetic avoids overflowing `Instant` for a `Duration::MAX` caller.
#[derive(Debug, Clone, Copy)]
struct GitBudget {
    started: Instant,
    timeout: Duration,
}

impl GitBudget {
    fn new(timeout: Duration) -> Self {
        Self {
            started: Instant::now(),
            timeout,
        }
    }

    fn remaining(self) -> Duration {
        self.timeout.saturating_sub(self.started.elapsed())
    }

    fn remaining_for(self, stage: &str) -> io::Result<Duration> {
        let remaining = self.remaining();
        if remaining.is_zero() {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("git invocation exceeded {:?} before {stage}", self.timeout),
            ))
        } else {
            Ok(remaining)
        }
    }

    fn capped(self, timeout: Duration) -> Self {
        Self {
            timeout: self.timeout.min(timeout),
            ..self
        }
    }
}

/// What the invocation did. Admission may fail before a child is spawned.
#[derive(Debug)]
pub enum GitRunOutcome {
    /// Normal exit (success OR non-zero) with captured Output.
    Finished(Output),
    /// SIGSEGV/11 or SIGBUS/7, including corresponding shell exit codes.
    SegfaultLike { signal: i32 },
    /// Another signal; not retryable.
    OtherSignal { signal: i32 },
    /// The child or its inherited pipes exceeded the invocation budget.
    Timeout { after: Duration },
    /// Admission, spawn, capture-limit or I/O error.
    Error(io::Error),
}

impl GitRunOutcome {
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
    envs: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    cwd: Option<PathBuf>,
    skip_flock: bool,
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

    /// Set the invocation budget, including admission and Unix child/pipe work.
    /// Zero refuses execution without spawning a child.
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
    /// provably read-only probes whose contract forbids creating the sentinel.
    #[must_use]
    pub const fn skip_flock(mut self) -> Self {
        self.skip_flock = true;
        self
    }

    /// Skip the in-process mutex. Almost never correct; retained for callers
    /// whose surrounding protocol already supplies the required exclusion.
    #[must_use]
    pub const fn skip_mutex(mut self) -> Self {
        self.skip_mutex = true;
        self
    }

    fn run_once_inner(&self, budget: GitBudget) -> GitRunOutcome {
        let execute = || -> io::Result<GitRunOutcome> {
            budget.remaining_for("repository resolution")?;
            let canonical = canonicalize_repo(self.repo);
            budget.remaining_for("binary resolution")?;
            let binary = resolve_git_binary()
                .map_err(|error| io::Error::other(format!("cannot resolve git binary: {error}")))?;
            budget.remaining_for("lock admission")?;
            let _reent = canonical.as_ref().map(|path| ReentrancyGuard::enter(path));

            // Include the registry wait, not just the per-repository mutex.
            let mutex =
                if self.skip_mutex {
                    None
                } else if let Some(path) = canonical.as_ref() {
                    Some(GitRepoLocks::global().lock_for_with_timeout(
                        path,
                        budget.remaining_for("repository lock lookup")?,
                    )?)
                } else {
                    None
                };
            let _mutex_guard = match mutex.as_ref() {
                Some(mutex) => Some(lock_mutex_with_timeout(
                    mutex,
                    budget.remaining_for("repository mutex")?,
                )?),
                None => None,
            };

            // Preserve the separate configured flock cap, but never let it
            // grant a fresh wait beyond the invocation's remaining budget.
            let _flock = if self.skip_flock {
                None
            } else if let Some(path) = canonical.as_ref() {
                let remaining = budget.remaining_for("repository flock")?;
                Some(RepoFlock::acquire_with_timeout(
                    path,
                    remaining.min(Duration::from_secs(flock_timeout_secs())),
                )?)
            } else {
                None
            };
            budget.remaining_for("git child execution")?;
            Ok(run_child(
                &binary,
                self.repo,
                self.cwd.as_deref(),
                &self.args,
                self.stdin.as_deref(),
                &self.envs,
                budget,
            ))
        };
        match execute() {
            Ok(outcome) => outcome,
            Err(error) => GitRunOutcome::Error(error),
        }
    }

    /// Run once, including bounded lock admission, returning classified outcome.
    #[must_use]
    pub fn run_once(self) -> GitRunOutcome {
        self.run_once_inner(GitBudget::new(self.timeout))
    }

    /// Run with at most three segfault retries. Backoff and all later attempts
    /// share the initial budget and its additional ten-second retry window.
    pub fn run(self) -> io::Result<Output> {
        run_with_retry(self.repo, GitBudget::new(self.timeout), |budget| {
            self.run_once_inner(budget)
        })
    }
}

fn pause_before_retry(
    budget: GitBudget,
    desired: Duration,
    pause: impl FnOnce(Duration),
) -> io::Result<()> {
    pause(desired.min(budget.remaining_for("retry backoff")?));
    // Scheduler delay or a slow callback may exceed the requested sleep.
    budget.remaining_for("retry admission")?;
    Ok(())
}

fn run_with_retry(
    repo: &Path,
    budget: GitBudget,
    mut run_attempt: impl FnMut(GitBudget) -> GitRunOutcome,
) -> io::Result<Output> {
    for attempt in 0..=SEGFAULT_BACKOFFS_MS.len() {
        let attempt_budget = if attempt == 0 {
            budget
        } else {
            budget.capped(SEGFAULT_RETRY_WINDOW)
        };
        attempt_budget.remaining_for("attempt admission")?;
        match run_attempt(attempt_budget) {
            GitRunOutcome::Finished(output) => {
                if attempt > 0 {
                    tracing::info!(
                        target: "mcp_agent_mail::git_locked",
                        attempt,
                        repo = %repo.display(),
                        "git_segfault_retry_succeeded"
                    );
                }
                // Preserve known terminal results, including nonzero status.
                // A later clock check must not repeat completed side effects.
                return Ok(output);
            }
            GitRunOutcome::SegfaultLike { signal } => {
                tracing::warn!(
                    target: "mcp_agent_mail::git_locked",
                    attempt,
                    signal,
                    repo = %repo.display(),
                    "git_segfault_retry_attempt"
                );
                let Some(&base_ms) = SEGFAULT_BACKOFFS_MS.get(attempt) else {
                    return Err(io::Error::other(format!(
                        "git segfaulted {} times in a row; set AM_GIT_BINARY to a supported binary",
                        attempt + 1,
                    )));
                };
                pause_before_retry(
                    budget.capped(SEGFAULT_RETRY_WINDOW),
                    Duration::from_millis(jitter_ms(base_ms)),
                    std::thread::sleep,
                )?;
            }
            GitRunOutcome::OtherSignal { signal } => {
                return Err(io::Error::other(format!(
                    "git child killed by signal {signal} (not segfault-like, not retrying)"
                )));
            }
            GitRunOutcome::Timeout { after } => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("git child or output pipes exceeded {after:?} invocation budget"),
                ));
            }
            GitRunOutcome::Error(error) => return Err(error),
        }
    }
    Err(io::Error::other("git retry attempts exhausted"))
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
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    let span = base / 2;
    let low = base - span / 2;
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
        if code == 139 {
            return GitRunOutcome::SegfaultLike { signal: 11 };
        }
        if code == 135 {
            return GitRunOutcome::SegfaultLike { signal: 7 };
        }
    }
    GitRunOutcome::Finished(Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

#[cfg(not(unix))]
fn classify_exit(status: std::process::ExitStatus) -> GitRunOutcome {
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
    budget: GitBudget,
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
        budget,
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
        if let Err(error) = budget.remaining_for("git child spawn") {
            return GitRunOutcome::Error(error);
        }
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
        wait_with_timeout(&mut child, budget)
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
        GitRunOutcome::Error(error) => {
            tracing::error!(
                target: "mcp_agent_mail::git_locked",
                err = %error,
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
    budget: GitBudget,
    output_limit: usize,
) -> GitRunOutcome {
    if let Err(error) = budget.remaining_for("git pipe setup") {
        return GitRunOutcome::Error(error);
    }
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
    if let Err(error) = budget.remaining_for("git child spawn") {
        return GitRunOutcome::Error(error);
    }
    let mut child = match command.spawn() {
        Ok(child) => ReapedGitChild(child),
        Err(error) => return GitRunOutcome::Error(error),
    };
    // Command retains explicit Stdio handles after spawn. Drop its writer
    // copies so EOF depends only on the actual child and any descendants.
    drop(command);
    capture_pipes(
        &mut child.0,
        stdin,
        stdin_bytes.unwrap_or_default(),
        stdout,
        stderr,
        budget,
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
            output.try_reserve(length).map_err(|error| {
                io::Error::other(format!("git capture allocation failed: {error}"))
            })?;
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
    budget: GitBudget,
    output_limit: usize,
) -> GitRunOutcome {
    use std::io::Write;

    let mut stdout = Some(stdout);
    let mut stderr = Some(stderr);
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut remaining = output_limit;
    let mut status = None;
    let mut pause = Duration::from_millis(1);
    loop {
        if input.is_empty() {
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
                GitRunOutcome::Finished(_) if !input.is_empty() => {
                    GitRunOutcome::Error(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "git exited before all supplied stdin could be written",
                    ))
                }
                GitRunOutcome::Finished(_) => GitRunOutcome::Finished(Output {
                    status: exited,
                    stdout: stdout_bytes,
                    stderr: stderr_bytes,
                }),
                other => other,
            };
        }
        if budget.remaining().is_zero() {
            return GitRunOutcome::Timeout {
                after: budget.timeout,
            };
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
            std::thread::sleep(pause.min(budget.remaining()));
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
fn wait_with_timeout(child: &mut Child, budget: GitBudget) -> GitRunOutcome {
    use std::io::Read;

    let mut stdout_handle = child.stdout.take().map(|mut output| {
        std::thread::spawn(move || {
            let mut buf = Vec::with_capacity(4096);
            let _ = output.read_to_end(&mut buf);
            buf
        })
    });
    let mut stderr_handle = child.stderr.take().map(|mut output| {
        std::thread::spawn(move || {
            let mut buf = Vec::with_capacity(4096);
            let _ = output.read_to_end(&mut buf);
            buf
        })
    });

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let remaining = budget.remaining();
                if remaining.is_zero() {
                    let _ = child.kill();
                    let _ = child.wait();
                    join_readers_bounded(stdout_handle.take(), stderr_handle.take());
                    return GitRunOutcome::Timeout {
                        after: budget.timeout,
                    };
                }
                std::thread::sleep(Duration::from_millis(25).min(remaining));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                join_readers_bounded(stdout_handle.take(), stderr_handle.take());
                return GitRunOutcome::Error(error);
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
        let output = out.unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("git version"));
    }

    #[test]
    fn run_returns_nonzero_output_not_error() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let result = GitCmd::new(&repo).arg("nonexistent-subcommand-xyz").run();
        assert!(result.is_ok(), "nonzero exit should NOT be Err: {result:?}");
        assert!(!result.unwrap().status.success());
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
                 dd if=/dev/zero bs=65536 count=16 >&2 2>/dev/null; cat",
            ),
            Some(&input),
            GitBudget::new(Duration::from_secs(10)),
            4 * 1024 * 1024,
        ));
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 3 * 1024 * 1024);
        assert!(output.stdout[..1024 * 1024].iter().all(|byte| *byte == 0));
        assert_eq!(&output.stdout[1024 * 1024..], input.as_slice());
        assert_eq!(output.stderr, vec![0; 1024 * 1024]);
    }

    #[cfg(unix)]
    #[test]
    fn blocked_stdin_is_cut_off_by_the_execution_deadline() {
        let started = Instant::now();
        let outcome = run_piped_command(
            shell("exec sleep 30"),
            Some(&vec![b'x'; 1024 * 1024]),
            GitBudget::new(Duration::from_millis(100)),
            1024,
        );
        assert!(matches!(outcome, GitRunOutcome::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn exited_child_with_retained_output_writer_still_has_a_deadline() {
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
            GitBudget::new(Duration::from_millis(100)),
            1024,
        );
        assert!(matches!(outcome, GitRunOutcome::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(child.0.try_wait().unwrap().unwrap().success());
        drop(retained_writer);
    }

    #[cfg(unix)]
    #[test]
    fn successful_exit_cannot_hide_unwritten_stdin() {
        let (retained_reader, stdin) = nonblocking_parent_pipe(true).unwrap();
        let (stdout, stdout_writer) = nonblocking_parent_pipe(false).unwrap();
        let (stderr, stderr_writer) = nonblocking_parent_pipe(false).unwrap();
        drop(stdout_writer);
        drop(stderr_writer);
        let mut child = ReapedGitChild(
            shell("exit 0")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        assert!(child.0.wait().unwrap().success());
        let outcome = capture_pipes(
            &mut child.0,
            Some(stdin),
            &vec![b'x'; 1024 * 1024],
            stdout,
            stderr,
            GitBudget::new(Duration::from_secs(5)),
            1024,
        );
        assert!(matches!(
            outcome,
            GitRunOutcome::Error(error) if error.kind() == io::ErrorKind::BrokenPipe
        ));
        drop(retained_reader);
    }

    #[cfg(unix)]
    #[test]
    fn combined_capture_limit_is_exact_and_never_returns_partial_success() {
        let output = finished(run_piped_command(
            shell("printf abc; printf def >&2"),
            None,
            GitBudget::new(Duration::from_secs(5)),
            6,
        ));
        assert_eq!(output.stdout, b"abc");
        assert_eq!(output.stderr, b"def");
        let limited = run_piped_command(
            shell("printf abc; printf def >&2"),
            None,
            GitBudget::new(Duration::from_secs(5)),
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
            GitBudget::new(Duration::from_secs(5)),
            1024,
        ));
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"");
        assert_eq!(output.stderr, b"problem");
    }

    #[cfg(unix)]
    #[test]
    fn continuous_output_cannot_bypass_capture_limit() {
        let outcome = run_piped_command(
            shell("while :; do printf '0123456789abcdef'; done"),
            None,
            GitBudget::new(Duration::from_secs(5)),
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
            GitBudget::new(Duration::MAX),
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

    #[test]
    fn mutex_timeout_prevents_git_write_and_reacquisition_succeeds() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();
        let mutex = GitRepoLocks::global().lock_for(&canonical);
        let held = mutex.lock().unwrap();
        let started = Instant::now();
        let error = GitCmd::new(&repo)
            .args(["config", "--local", "budget.probe", "admitted"])
            .timeout(Duration::from_millis(50))
            .run()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            !repo.join(".git/config").exists(),
            "timed-out work must not execute"
        );
        drop(held);
        let output = GitCmd::new(&repo)
            .args(["config", "--local", "budget.probe", "admitted"])
            .run()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(
            std::fs::read_to_string(repo.join(".git/config"))
                .unwrap()
                .contains("admitted")
        );
    }

    #[test]
    fn flock_timeout_releases_the_process_mutex_without_executing_git() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let canonical = canonicalize_repo(&repo).unwrap();
        let held = RepoFlock::acquire(&canonical).unwrap();
        assert!(held.is_real());
        let error = GitCmd::new(&repo)
            .args(["config", "--local", "budget.probe", "admitted"])
            .timeout(Duration::from_millis(50))
            .run()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(!repo.join(".git/config").exists());
        let mutex = GitRepoLocks::global().lock_for(&canonical);
        let guard = mutex
            .try_lock()
            .expect("failed flock must release the mutex");
        drop(guard);
        drop(held);
        assert!(
            GitCmd::new(&repo)
                .arg("--version")
                .run()
                .unwrap()
                .status
                .success()
        );
    }

    #[test]
    fn a_contended_repository_does_not_block_an_unrelated_git_invocation() {
        let tmp = TempDir::new().unwrap();
        let first = init_repo(&tmp.path().join("first"));
        let second = init_repo(&tmp.path().join("second"));
        let mutex = GitRepoLocks::global().lock_for(&canonicalize_repo(&first).unwrap());
        let _held = mutex.lock().unwrap();
        assert!(
            GitCmd::new(&second)
                .arg("--version")
                .run()
                .unwrap()
                .status
                .success()
        );
    }

    #[test]
    fn zero_invocation_budget_refuses_without_creating_a_sentinel_or_config() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let error = GitCmd::new(&repo)
            .args(["config", "--local", "budget.probe", "admitted"])
            .timeout(Duration::ZERO)
            .run()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(!repo.join(".git/config").exists());
        assert!(!repo.join(".git/am.git-serialize.lock").exists());
    }

    #[test]
    fn retry_window_keeps_original_origin_and_does_not_cap_the_first_attempt() {
        let budget = GitBudget {
            started: Instant::now().checked_sub(Duration::from_secs(11)).unwrap(),
            timeout: Duration::from_secs(120),
        };
        let mut calls = 0;
        let error = run_with_retry(Path::new("repo"), budget, |attempt| {
            calls += 1;
            assert_eq!(attempt.started, budget.started);
            assert_eq!(attempt.timeout, Duration::from_secs(120));
            assert!(attempt.remaining() > Duration::from_secs(100));
            GitRunOutcome::SegfaultLike { signal: 11 }
        })
        .unwrap_err();
        assert_eq!(
            calls, 1,
            "an expired retry window must not launch attempt two"
        );
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let short = GitBudget::new(Duration::from_secs(2));
        assert_eq!(short.capped(SEGFAULT_RETRY_WINDOW).timeout, short.timeout);
        assert_eq!(short.capped(SEGFAULT_RETRY_WINDOW).started, short.started);
    }

    #[test]
    fn retry_backoff_clips_sleep_and_rechecks_after_oversleep() {
        let budget = GitBudget::new(Duration::from_millis(100));
        let mut sleeps = 0;
        let result = pause_before_retry(budget, Duration::from_secs(2), |requested| {
            sleeps += 1;
            assert!(requested <= Duration::from_millis(100));
            std::thread::sleep(Duration::from_millis(150));
        });
        assert_eq!(sleeps, 1);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn non_segfault_signals_never_consume_another_attempt() {
        let mut calls = 0;
        let error = run_with_retry(
            Path::new("repo"),
            GitBudget::new(Duration::from_secs(10)),
            |_| {
                calls += 1;
                GitRunOutcome::OtherSignal { signal: 15 }
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(error.to_string().contains("not retrying"));
    }

    #[cfg(unix)]
    #[test]
    fn expired_shared_budget_never_spawns_the_prepared_child() {
        let tmp = TempDir::new().unwrap();
        let mut command = shell("printf dispatched > marker");
        command.current_dir(tmp.path());
        let budget = GitBudget {
            started: Instant::now().checked_sub(Duration::from_secs(2)).unwrap(),
            timeout: Duration::from_secs(1),
        };
        assert!(matches!(
            run_piped_command(command, None, budget, 1024),
            GitRunOutcome::Error(error) if error.kind() == io::ErrorKind::TimedOut
        ));
        assert!(!tmp.path().join("marker").exists());
    }

    #[cfg(unix)]
    #[test]
    fn segfault_then_slow_child_share_one_budget_and_stop_before_attempt_three() {
        let budget = GitBudget::new(Duration::from_secs(2));
        let mut calls = 0;
        let error = run_with_retry(Path::new("repo"), budget, |attempt_budget| {
            calls += 1;
            assert_eq!(attempt_budget.started, budget.started);
            assert_eq!(attempt_budget.timeout, budget.timeout);
            let command = if calls == 1 {
                // Shell exit classification, not a real crash/core dump.
                shell("exit 139")
            } else {
                assert_eq!(calls, 2);
                shell("exec sleep 30")
            };
            run_piped_command(command, None, attempt_budget, 1024)
        })
        .unwrap_err();
        assert_eq!(calls, 2);
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(budget.started.elapsed() < Duration::from_secs(5));
    }
}
