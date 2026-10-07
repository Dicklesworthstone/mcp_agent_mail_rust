//! Cooperative, bounded lock admission for replayable archive repair.
//!
//! Use the same process mutex and filesystem lock as ordinary archive writers,
//! but spend one short budget across both waits. Contention defers repair; it
//! never authorizes breaking a live lock or publishing without ownership.
//! Filesystem operations, registry access, and the admitted closure are not
//! subject to a hard deadline. Repair callers first try the archive mutation
//! fence with `try_begin_at`; the acquired native guard spans this lock scope.

use std::sync::TryLockError;
use std::time::{Duration, Instant};

use asupersync::Cx;

use crate::{FileLock, ProjectArchive, Result, StorageError};

mod fence;
pub(super) use fence::try_begin_at;

const REPAIR_LOCK_BUDGET: Duration = Duration::from_millis(250);
const WAIT_SLICE: Duration = Duration::from_millis(10);

pub(super) fn with_repair_lock<T>(
    cx: &Cx,
    archive: &ProjectArchive,
    repair: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_budget(
        archive,
        REPAIR_LOCK_BUDGET,
        || cx.checkpoint().is_err(),
        repair,
    )
}

impl ProjectArchive {
    /// Recovery-only admission shared with message repair. Callers retain the
    /// native publication fence before entering; both project lock layers use
    /// the same budget and ownership protocol as terminal reservation repair.
    pub(in crate::recovery) fn with_repair_lock<T>(
        &self,
        cancelled: impl FnMut() -> bool,
        repair: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        with_budget(self, REPAIR_LOCK_BUDGET, cancelled, repair)
    }
}

fn deferred() -> StorageError {
    StorageError::LockTimeout("archive repair lock budget exhausted; repair deferred".to_string())
}

fn remaining(
    started: Instant,
    budget: Duration,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Duration> {
    if cancelled() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "archive repair lock acquisition cancelled",
        )
        .into());
    }
    let remaining = budget.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(deferred());
    }
    Ok(remaining)
}

fn with_budget<T>(
    archive: &ProjectArchive,
    budget: Duration,
    mut cancelled: impl FnMut() -> bool,
    repair: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let started = Instant::now();
    remaining(started, budget, &mut cancelled)?;
    let process_lock = crate::archive_process_lock(archive)?;
    let project_root = crate::archive_project_root_checked(archive)?;
    let _process = loop {
        let left = remaining(started, budget, &mut cancelled)?;
        match process_lock.try_lock() {
            Ok(guard) => break guard,
            Err(TryLockError::Poisoned(error)) => break error.into_inner(),
            Err(TryLockError::WouldBlock) => std::thread::sleep(left.min(WAIT_SLICE)),
        }
    };
    let mut lock = loop {
        let left = remaining(started, budget, &mut cancelled)?;
        // FileLock's default minimum-attempt floor is intentionally disabled:
        // it must not extend a short recovery budget. Short slices also let
        // caller cancellation interrupt contention on a foreign process lock.
        let mut candidate = FileLock::new(project_root.join(".archive.lock"))
            .with_timeout(left.min(WAIT_SLICE))
            .with_max_retries(0);
        match candidate.acquire() {
            Ok(()) => break candidate,
            Err(StorageError::LockTimeout(_)) => {}
            Err(error) => return Err(error),
        }
    };
    // A late acquisition or cancellation never authorizes a repair. Dropping
    // the owned FileLock releases it on this path as well as during unwinding.
    remaining(started, budget, &mut cancelled)?;
    mcp_agent_mail_core::global_metrics()
        .storage
        .archive_lock_wait_us
        .record(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
    let result = repair();
    lock.release()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};

    fn archive(root: &std::path::Path) -> ProjectArchive {
        crate::ensure_archive(
            &mcp_agent_mail_core::Config {
                storage_root: root.to_path_buf(),
                ..Default::default()
            },
            "repair-lock",
        )
        .unwrap()
    }

    #[test]
    fn an_in_process_owner_defers_repair_without_releasing_the_owner() {
        let temp = tempfile::tempdir().unwrap();
        let archive = archive(temp.path());
        let lock = crate::archive_process_lock(&archive).unwrap();
        let held = lock.lock().unwrap();
        let called = AtomicBool::new(false);
        let started = Instant::now();
        let result = with_budget(
            &archive,
            Duration::from_millis(20),
            || false,
            || {
                called.store(true, Ordering::Release);
                Ok(())
            },
        );
        assert!(matches!(result, Err(StorageError::LockTimeout(_))));
        assert!(started.elapsed() >= Duration::from_millis(20));
        assert!(!called.load(Ordering::Acquire));
        assert!(matches!(lock.try_lock(), Err(TryLockError::WouldBlock)));
        drop(held);
        assert_eq!(
            with_repair_lock(&Cx::for_testing(), &archive, || Ok(42)).unwrap(),
            42
        );
    }

    #[test]
    fn a_foreign_flock_defers_without_changing_its_file_or_metadata() {
        use fs2::FileExt;
        let temp = tempfile::tempdir().unwrap();
        let archive = archive(temp.path());
        let path = archive.root.join(".archive.lock");
        let metadata = archive.root.join(".archive.lock.owner.json");
        std::fs::write(&path, b"held lock evidence").unwrap();
        std::fs::write(&metadata, b"operator evidence").unwrap();
        let held = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        held.try_lock_exclusive().unwrap();
        let result: Result<()> = with_budget(
            &archive,
            Duration::from_millis(20),
            || false,
            || panic!("contended flock must not invoke repair"),
        );
        assert!(matches!(result, Err(StorageError::LockTimeout(_))));
        assert_eq!(std::fs::read(&path).unwrap(), b"held lock evidence");
        assert_eq!(std::fs::read(&metadata).unwrap(), b"operator evidence");
        let contender = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert!(contender.try_lock_exclusive().is_err());
        fs2::FileExt::unlock(&held).unwrap();
        assert!(with_repair_lock(&Cx::for_testing(), &archive, || Ok(())).is_ok());
    }

    #[test]
    fn cancellation_interrupts_process_lock_contention_before_its_long_budget() {
        let temp = tempfile::tempdir().unwrap();
        let archive = archive(temp.path());
        let lock = crate::archive_process_lock(&archive).unwrap();
        let held = lock.lock().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&cancelled);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = with_budget(
                &archive,
                Duration::from_secs(30),
                || {
                    let _ = entered_tx.send(());
                    observed.load(Ordering::Acquire)
                },
                || Ok(()),
            );
            let _ = done_tx.send(matches!(result, Err(StorageError::Io(ref error))
                if error.kind() == std::io::ErrorKind::Interrupted));
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        cancelled.store(true, Ordering::Release);
        let interrupted_while_held = done_rx.recv_timeout(Duration::from_secs(5));
        drop(held);
        worker.join().unwrap();
        assert!(interrupted_while_held.unwrap());
    }

    #[test]
    fn cancellation_after_acquisition_prevents_publication_and_releases_both_locks() {
        let temp = tempfile::tempdir().unwrap();
        let archive = archive(temp.path());
        let mut checks = 0;
        let result = with_budget(
            &archive,
            Duration::from_secs(5),
            || {
                checks += 1;
                checks == 4
            },
            || Ok("must not run"),
        );
        assert!(matches!(result, Err(StorageError::Io(ref error))
            if error.kind() == std::io::ErrorKind::Interrupted));
        assert!(with_repair_lock(&Cx::for_testing(), &archive, || Ok(())).is_ok());
    }

    #[test]
    fn repair_error_and_panic_release_both_locks() {
        let temp = tempfile::tempdir().unwrap();
        let archive = archive(temp.path());
        let result: Result<()> = with_repair_lock(&Cx::for_testing(), &archive, || {
            Err(StorageError::InvalidPath("injected repair error".into()))
        });
        assert!(matches!(result, Err(StorageError::InvalidPath(_))));
        assert!(
            std::panic::catch_unwind(|| {
                let _: Result<()> = with_repair_lock(&Cx::for_testing(), &archive, || {
                    panic!("injected repair panic");
                });
            })
            .is_err()
        );
        assert!(with_repair_lock(&Cx::for_testing(), &archive, || Ok(())).is_ok());
    }

    #[test]
    fn admission_holds_both_existing_locks_and_zero_budget_never_calls_repair() {
        use fs2::FileExt;
        let temp = tempfile::tempdir().unwrap();
        let archive = archive(temp.path());
        let process = crate::archive_process_lock(&archive).unwrap();
        let path = archive.root.join(".archive.lock");
        let zero: Result<()> = with_budget(
            &archive,
            Duration::ZERO,
            || false,
            || {
                panic!("an exhausted budget cannot publish");
            },
        );
        assert!(matches!(zero, Err(StorageError::LockTimeout(_))));
        assert!(!path.exists());
        with_repair_lock(&Cx::for_testing(), &archive, || {
            assert!(matches!(process.try_lock(), Err(TryLockError::WouldBlock)));
            let contender = std::fs::OpenOptions::new().write(true).open(&path)?;
            assert!(contender.try_lock_exclusive().is_err());
            Ok(())
        })
        .unwrap();
        assert!(process.try_lock().is_ok());
    }
}
