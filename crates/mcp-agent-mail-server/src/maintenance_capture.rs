//! Unix output capture for the archive maintenance child.
//!
//! Waiting for exit before draining pipes deadlocks when Git fills a pipe.
//! Waiting for EOF after exit is also unbounded when a descendant inherits a
//! writer. Drain both parent endpoints fairly, and require exit AND both EOFs
//! within the same budget. No reader threads, temporary output files or unsafe
//! code are needed. Child endpoints keep their ordinary blocking semantics.
//!
//! The caller retains its existing graceful termination and stale-lock evidence
//! protocol on errors. Owning Drop is the last-resort direct-child reaper,
//! including during unwind. This does not terminate arbitrary descendants or
//! preempt blocking OS spawn/reap/filesystem calls.

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use std::io::{self, Read};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// Match the shared Git runner's default combined capture bound. An over-limit
// result is an error, never truncated success or an incomplete MIDX diagnosis.
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_IDLE_PAUSE: Duration = Duration::from_millis(20);

fn output_pipe() -> io::Result<(io::PipeReader, io::PipeWriter)> {
    let (reader, writer) = io::pipe()?;
    let flags = OFlag::from_bits_retain(fcntl(&reader, FcntlArg::F_GETFL)?);
    fcntl(&reader, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok((reader, writer))
}

pub(super) struct CapturedChild {
    child: Child,
    stdout: Option<io::PipeReader>,
    stderr: Option<io::PipeReader>,
    started: Instant,
}

impl CapturedChild {
    pub(super) fn spawn(mut command: Command, stop: &AtomicBool) -> io::Result<Self> {
        if stop.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "interrupted by shutdown",
            ));
        }
        let started = Instant::now();
        let (stdout, stdout_writer) = output_pipe()?;
        let (stderr, stderr_writer) = output_pipe()?;
        // The maintenance and MIDX recovery paths classify Git diagnostics.
        // Keep those diagnostics stable even under a localized server locale.
        command.env("LC_ALL", "C");
        command
            .stdin(Stdio::null())
            .stdout(stdout_writer)
            .stderr(stderr_writer);
        if stop.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "interrupted by shutdown",
            ));
        }
        let child = command.spawn()?;
        // Command retains explicit Stdio handles after spawn. Its copies must
        // close now, otherwise even a child without descendants never gets EOF.
        drop(command);
        Ok(Self {
            child,
            stdout: Some(stdout),
            stderr: Some(stderr),
            started,
        })
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    /// Borrow the exact owned child for the existing evidence-aware shutdown.
    pub(super) fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    pub(super) fn wait_output(
        &mut self,
        stop: &AtomicBool,
        timeout: Duration,
    ) -> io::Result<Output> {
        self.capture(stop, timeout, MAX_OUTPUT_BYTES)
    }

    fn capture(
        &mut self,
        stop: &AtomicBool,
        timeout: Duration,
        output_limit: usize,
    ) -> io::Result<Output> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut remaining = output_limit;
        let mut status = None;
        let mut pause = Duration::from_millis(1);
        loop {
            if stop.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "interrupted by shutdown",
                ));
            }
            if self.started.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "maintenance child or inherited output pipes timed out after {timeout:?}"
                    ),
                ));
            }
            // One chunk from EACH pipe per turn. A continuous stdout producer
            // cannot starve stderr, process reaping, shutdown or deadline checks.
            let out_progress = drain(&mut self.stdout, &mut stdout, &mut remaining)?;
            let err_progress = drain(&mut self.stderr, &mut stderr, &mut remaining)?;
            if status.is_none() {
                status = self.child.try_wait()?;
            }
            if let Some(status) = status
                && self.stdout.is_none()
                && self.stderr.is_none()
            {
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            if out_progress || err_progress {
                pause = Duration::from_millis(1);
            } else {
                std::thread::sleep(pause.min(timeout.saturating_sub(self.started.elapsed())));
                pause = pause.saturating_mul(2).min(MAX_IDLE_PAUSE);
            }
        }
    }
}

/// False means no progress. Reads and EINTR handling are bounded per turn.
fn drain(
    pipe: &mut Option<io::PipeReader>,
    bytes: &mut Vec<u8>,
    remaining: &mut usize,
) -> io::Result<bool> {
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
                    "MAINTENANCE_OUTPUT_LIMIT: combined stdout/stderr exceeded the capture bound",
                ));
            }
            bytes.try_reserve(length).map_err(io::Error::other)?;
            bytes.extend_from_slice(&buffer[..length]);
            *remaining -= length;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(true),
        Err(error) => Err(error),
    }
}

impl Drop for CapturedChild {
    fn drop(&mut self) {
        // No capture endpoint or direct child may survive an early return or
        // unwind. Descendants lose these parent readers but are not signalled.
        self.stdout = None;
        self.stderr = None;
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn only_parent_read_endpoints_are_nonblocking() {
        let (reader, writer) = output_pipe().unwrap();
        let read_flags = OFlag::from_bits_retain(fcntl(&reader, FcntlArg::F_GETFL).unwrap());
        let write_flags = OFlag::from_bits_retain(fcntl(&writer, FcntlArg::F_GETFL).unwrap());
        assert!(read_flags.contains(OFlag::O_NONBLOCK));
        assert!(!write_flags.contains(OFlag::O_NONBLOCK));
    }

    #[test]
    fn real_large_stdout_and_stderr_complete_without_pipe_deadlock() {
        let stop = AtomicBool::new(false);
        // POSIX shell builtins; each stream exceeds pipe capacity many times.
        let mut child = CapturedChild::spawn(
            shell(
                "i=0; while [ \"$i\" -lt 128 ]; do printf '%4096s' X; \
             printf '%4096s' Y >&2; i=$((i + 1)); done; \
             printf stdout-end; printf stderr-end >&2",
            ),
            &stop,
        )
        .unwrap();
        let output = child.wait_output(&stop, Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 128 * 4096 + 10);
        assert_eq!(output.stderr.len(), 128 * 4096 + 10);
        assert_eq!(
            output.stdout.iter().filter(|&&byte| byte == b'X').count(),
            128
        );
        assert_eq!(
            output.stderr.iter().filter(|&&byte| byte == b'Y').count(),
            128
        );
        assert!(output.stdout.ends_with(b"stdout-end"));
        assert!(output.stderr.ends_with(b"stderr-end"));
        assert!(child.child_mut().try_wait().unwrap().is_some());
    }

    #[test]
    fn nonzero_exit_and_complete_midx_diagnostics_remain_available() {
        let stop = AtomicBool::new(false);
        let mut child = CapturedChild::spawn(shell(
            "printf 'partial packing'; printf 'could not load pack; failed to write multi-pack-index' >&2; exit 7",
        ), &stop).unwrap();
        let output = child.wait_output(&stop, Duration::from_secs(5)).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"partial packing");
        assert_eq!(
            output.stderr,
            b"could not load pack; failed to write multi-pack-index"
        );
    }

    #[test]
    fn diagnostic_locale_is_fixed_for_machine_classification() {
        let stop = AtomicBool::new(false);
        let mut command = shell("printf '%s' \"$LC_ALL\"");
        command.env("LC_ALL", "POSIX");
        let mut child = CapturedChild::spawn(command, &stop).unwrap();
        let output = child.wait_output(&stop, Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"C");
    }

    #[test]
    fn combined_output_limit_refuses_instead_of_returning_truncated_success() {
        let stop = AtomicBool::new(false);
        let mut child = CapturedChild::spawn(
            shell("while :; do printf '%4096s' X; printf '%4096s' Y >&2; done"),
            &stop,
        )
        .unwrap();
        let error = child
            .capture(&stop, Duration::from_secs(5), 32 * 1024)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("MAINTENANCE_OUTPUT_LIMIT"));
        // Owning Drop kills/reaps the direct child, with no reader thread join.
        drop(child);
    }

    #[test]
    fn exact_capture_limit_is_accepted_after_both_eofs() {
        let stop = AtomicBool::new(false);
        let mut child = CapturedChild::spawn(shell("printf 1234; printf 5678 >&2"), &stop).unwrap();
        let output = child.capture(&stop, Duration::from_secs(5), 8).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"1234");
        assert_eq!(output.stderr, b"5678");
    }

    #[test]
    fn inherited_writer_cannot_extend_capture_after_direct_child_exit() {
        let stop = AtomicBool::new(false);
        let (reader, writer) = output_pipe().unwrap();
        let mut child = CapturedChild::spawn(shell("printf direct-child-exited"), &stop).unwrap();
        // An independently owned writer has exactly the EOF semantics of an
        // inherited descendant descriptor, without leaving an orphan process.
        // The direct child's real stdout still drains; hold the stderr writer.
        child.stderr = Some(reader);
        let error = child
            .capture(&stop, Duration::from_millis(250), 1024)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(child.child_mut().try_wait().unwrap().is_some());
        drop(writer);
    }

    #[test]
    fn shutdown_before_spawn_and_during_idle_capture_stops_work() {
        assert!(CapturedChild::spawn(shell("exit 0"), &AtomicBool::new(true)).is_err());
        let stop = AtomicBool::new(false);
        let mut child = CapturedChild::spawn(shell("exec sleep 5"), &stop).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                stop.store(true, Ordering::Release);
            });
            let error = child
                .wait_output(&stop, Duration::from_secs(3))
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        });
        drop(child);
    }

    #[test]
    fn zero_deadline_refuses_even_when_capture_has_no_output() {
        let stop = AtomicBool::new(false);
        let mut child = CapturedChild::spawn(shell("exec sleep 5"), &stop).unwrap();
        let error = child.wait_output(&stop, Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(child);
    }

    #[test]
    fn timeout_preserves_stale_lock_evidence_for_the_existing_recovery_path() {
        let scratch = tempfile::tempdir().unwrap();
        let git_dir = scratch.path().join("archive.git");
        let lock = git_dir.join("objects/maintenance.lock");
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        let stop = AtomicBool::new(false);
        let mut command =
            shell("printf 'owned maintenance lock' > \"$AM_TEST_LOCK\"; exec sleep 5");
        command.env("AM_TEST_LOCK", &lock);
        let mut child = CapturedChild::spawn(command, &stop).unwrap();
        let active = super::super::record_active_maintenance_evidence(
            &git_dir,
            child.id(),
            super::super::process_start_ticks(child.id()),
        );
        assert!(active.is_some());
        let error = child
            .wait_output(&stop, Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(std::fs::read(&lock).unwrap(), b"owned maintenance lock");
        let evidence = super::super::terminate_maintenance_child(
            child.child_mut(),
            &git_dir,
            &error.to_string(),
        );
        assert!(evidence.is_some());
        assert!(child.child_mut().try_wait().unwrap().is_some());
        drop(child);
        let preflight = super::super::preflight_maintenance_lock(&git_dir);
        assert!(preflight.reaped, "{preflight:?}");
        let retained = std::fs::read_dir(git_dir.join("objects"))
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("maintenance.lock.agent-mail-quarantine-")
            })
            .expect("owned stale lock retained by move-only recovery");
        assert_eq!(
            std::fs::read(retained.path()).unwrap(),
            b"owned maintenance lock"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unwinding_capture_reaps_its_direct_child() {
        let pid = std::cell::Cell::new(0);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let child =
                CapturedChild::spawn(shell("exec sleep 30"), &AtomicBool::new(false)).unwrap();
            pid.set(child.id());
            panic!("exercise owned child cleanup during unwind");
        }));
        assert!(outcome.is_err());
        assert_ne!(pid.get(), 0);
        assert!(!std::path::Path::new(&format!("/proc/{}", pid.get())).exists());
    }

    #[test]
    fn already_reaped_child_cannot_authorize_termination_or_new_stale_lock_evidence() {
        let scratch = tempfile::tempdir().unwrap();
        let git_dir = scratch.path().join("archive.git");
        let stop = AtomicBool::new(false);
        let mut child = CapturedChild::spawn(shell("exit 0"), &stop).unwrap();
        assert!(
            child
                .wait_output(&stop, Duration::from_secs(5))
                .unwrap()
                .status
                .success()
        );
        let lock = git_dir.join("objects/maintenance.lock");
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(&lock, b"created after that child exited").unwrap();
        assert!(
            super::super::terminate_maintenance_child(
                child.child_mut(),
                &git_dir,
                "inherited pipe timeout",
            )
            .is_none()
        );
        assert_eq!(
            std::fs::read(lock).unwrap(),
            b"created after that child exited"
        );
        assert!(!git_dir.join("agent-mail-maintenance").exists());
    }
}
