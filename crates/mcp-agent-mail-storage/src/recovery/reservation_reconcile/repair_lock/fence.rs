//! Nonblocking admission to the existing archive mutation protocol.
//!
//! Terminal repair already holds a database write-activity lease. Waiting for
//! an archive writer here would retain that lease behind an unrelated stalled
//! publisher. Refuse contention before opening the archive or advancing either
//! mutation epoch. Successful admission returns the ordinary guard, including
//! its persisted epoch edges, reentrant depth, diagnostics and unwind cleanup.

use std::path::Path;
use std::sync::TryLockError;
use std::sync::atomic::Ordering;
use std::time::Instant;

use asupersync::Cx;

use crate::{ArchiveMutationGuard, FenceHolderRecord, FenceLease, Result, StorageError};

#[track_caller]
pub(in crate::recovery::reservation_reconcile) fn try_begin_at(
    cx: &Cx,
    path: &Path,
) -> Result<ArchiveMutationGuard> {
    try_begin_checked(path, || cx.checkpoint().is_err())
}

impl ArchiveMutationGuard {
    /// Share the existing repair admission protocol with other recovery paths.
    /// The predicate can include a worker stop flag as well as its Cx budget.
    /// This is still the native guard, not a parallel fence or epoch protocol.
    #[track_caller]
    pub(in crate::recovery) fn try_begin_repair(
        path: &Path,
        cancelled: impl FnMut() -> bool,
    ) -> Result<Self> {
        try_begin_checked(path, cancelled)
    }
}

fn unavailable(reason: &str) -> StorageError {
    StorageError::LockContention {
        message: format!("archive repair publication admission deferred: {reason}"),
    }
}

fn check_cancelled(cancelled: &mut impl FnMut() -> bool) -> Result<()> {
    if cancelled() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "archive repair publication admission cancelled",
        )
        .into());
    }
    Ok(())
}

#[track_caller]
fn try_begin_checked(
    path: &Path,
    mut cancelled: impl FnMut() -> bool,
) -> Result<ArchiveMutationGuard> {
    check_cancelled(&mut cancelled)?;
    let depth = crate::ARCHIVE_MUTATION_DEPTH
        .try_with(std::cell::Cell::get)
        .map_err(|_| unavailable("mutation tracking is unavailable"))?;
    let next_depth = depth
        .checked_add(1)
        .ok_or_else(|| unavailable("mutation nesting exhausted"))?;
    // Allocate before changing the tracking state. Nested windows retain the
    // enclosing window's persisted anchor, exactly as the ordinary entrypoint.
    let epoch_anchor = (depth == 0).then(|| path.to_path_buf());
    let fence = if depth == 0 {
        let guard = match crate::ARCHIVE_PUBLICATION_FENCE.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(unavailable("publication fence busy")),
        };
        // Do not trade the publication wait for an unbounded diagnostic wait.
        // On refusal the raw fence guard drops without a mutation window.
        let mut holder = match crate::FENCE_HOLDER.try_lock() {
            Ok(holder) => holder,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(unavailable("holder metadata busy")),
        };
        let thread = std::thread::current();
        *holder = Some(FenceHolderRecord {
            thread: format!(
                "{} ({:?})",
                thread.name().unwrap_or("<unnamed>"),
                thread.id()
            ),
            site: std::panic::Location::caller(),
            since: Instant::now(),
        });
        drop(holder);
        Some(FenceLease { guard: Some(guard) })
    } else {
        None
    };
    // A cancellation racing acquisition must not stamp an epoch or publish.
    // FenceLease performs the ordinary holder-clear-before-unlock cleanup.
    check_cancelled(&mut cancelled)?;
    crate::ARCHIVE_MUTATION_DEPTH
        .try_with(|depth| depth.set(next_depth))
        .map_err(|_| unavailable("mutation tracking disappeared"))?;
    crate::ARCHIVE_MUTATIONS_ACTIVE.fetch_add(1, Ordering::AcqRel);
    crate::ARCHIVE_MUTATION_EPOCH.fetch_add(1, Ordering::AcqRel);
    // Construct before the filesystem work: an unwinding entry must run the
    // same exit protocol as every other admitted physical mutation window.
    let window = ArchiveMutationGuard {
        fence,
        epoch_anchor,
    };
    if let Some(anchor) = window.epoch_anchor.as_deref()
        && let Some(git_dir) = crate::resolve_archive_epoch_git_dir(anchor)
    {
        crate::rewrite_archive_epoch_file(&git_dir);
    }
    Ok(window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    // These assertions concern the real process-global fence and counters.
    // Other libtests must not mutate them between observations.
    fn isolated() -> bool {
        const CHILD: &str = "AM_TEST_REPAIR_FENCE_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named libtest thread");
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .output()
            .expect("run isolated archive admission test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "isolated {name} failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        true
    }

    fn archive_root() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        crate::ensure_archive(
            &mcp_agent_mail_core::Config {
                storage_root: temp.path().to_path_buf(),
                ..Default::default()
            },
            "fence-repair",
        )
        .unwrap();
        temp
    }

    fn token(root: &Path) -> Vec<u8> {
        std::fs::read(root.join(".git").join(crate::ARCHIVE_EPOCH_FILE_NAME)).unwrap()
    }

    fn tracking() -> (u32, usize, u64) {
        (
            crate::ARCHIVE_MUTATION_DEPTH.with(std::cell::Cell::get),
            crate::archive_mutations_active(),
            crate::archive_mutation_epoch(),
        )
    }

    #[test]
    fn a_busy_publication_fence_preserves_tracking_and_the_owners_evidence() {
        if isolated() {
            return;
        }
        let root = archive_root();
        let before = tracking();
        let stamp = token(root.path());
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            crate::with_archive_snapshot_publication_fence(|| {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5))
            })
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let held = crate::archive_publication_fence_holder().unwrap();
        let refused = try_begin_at(&Cx::for_testing(), root.path()).err();
        let after = tracking();
        let after_stamp = token(root.path());
        let still_held = crate::archive_publication_fence_holder().unwrap();
        let _ = release_tx.send(());
        let explicitly_released = owner.join().unwrap();
        assert!(
            explicitly_released.is_ok(),
            "repair waited for the owner's cleanup timeout"
        );
        assert!(
            refused
                .unwrap()
                .to_string()
                .contains("publication fence busy")
        );
        assert_eq!(after, before);
        assert_eq!(after_stamp, stamp);
        assert_eq!(still_held.thread, held.thread);
        assert_eq!(still_held.site, held.site);
        let window = try_begin_at(&Cx::for_testing(), root.path()).unwrap();
        assert_eq!(tracking().0, 1, "refusal did not leak a reentrant depth");
        drop(window);
        assert_eq!(tracking().0, 0);
    }

    #[test]
    fn admitted_and_nested_windows_use_the_native_epoch_and_holder_protocol() {
        if isolated() {
            return;
        }
        let root = archive_root();
        let before = tracking();
        assert_eq!((before.0, before.1), (0, 0));
        let old_stamp = token(root.path());
        let outer = try_begin_at(&Cx::for_testing(), root.path()).unwrap();
        assert_eq!(tracking(), (1, 1, before.2 + 1));
        let entry_stamp = token(root.path());
        assert_ne!(entry_stamp, old_stamp);
        let holder = crate::archive_publication_fence_holder().unwrap();
        assert!(matches!(
            crate::ARCHIVE_PUBLICATION_FENCE.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        let nested = crate::ArchiveMutationGuard::begin_at(root.path());
        assert_eq!(tracking(), (2, 2, before.2 + 2));
        assert_eq!(token(root.path()), entry_stamp);
        let repair_nested = try_begin_at(&Cx::for_testing(), root.path()).unwrap();
        assert_eq!(tracking(), (3, 3, before.2 + 3));
        assert_eq!(
            crate::archive_publication_fence_holder().unwrap().site,
            holder.site
        );
        drop(repair_nested);
        drop(nested);
        assert_eq!(tracking(), (1, 1, before.2 + 5));
        assert_eq!(token(root.path()), entry_stamp);
        drop(outer);
        assert_eq!(tracking(), (0, 0, before.2 + 6));
        assert_ne!(token(root.path()), entry_stamp);
        assert!(crate::archive_publication_fence_holder().is_none());
        assert!(crate::ARCHIVE_PUBLICATION_FENCE.try_lock().is_ok());
    }

    #[test]
    fn cancellation_before_or_after_acquisition_never_opens_a_mutation_window() {
        if isolated() {
            return;
        }
        let root = archive_root();
        let before = tracking();
        let stamp = token(root.path());
        for cancel_at in [1, 2] {
            let mut checks = 0;
            let result = try_begin_checked(root.path(), || {
                checks += 1;
                checks == cancel_at
            });
            assert!(matches!(result, Err(StorageError::Io(ref error))
                if error.kind() == std::io::ErrorKind::Interrupted));
            assert_eq!(tracking(), before);
            assert_eq!(token(root.path()), stamp);
            assert!(crate::archive_publication_fence_holder().is_none());
            assert!(crate::ARCHIVE_PUBLICATION_FENCE.try_lock().is_ok());
        }
    }

    #[test]
    fn busy_diagnostic_metadata_defers_without_retaining_the_publication_lock() {
        if isolated() {
            return;
        }
        let root = archive_root();
        let before = tracking();
        let stamp = token(root.path());
        let holder = crate::FENCE_HOLDER.lock().unwrap();
        let refused = try_begin_at(&Cx::for_testing(), root.path()).err().unwrap();
        assert!(refused.to_string().contains("holder metadata busy"));
        assert_eq!(tracking(), before);
        assert_eq!(token(root.path()), stamp);
        assert!(crate::ARCHIVE_PUBLICATION_FENCE.try_lock().is_ok());
        assert!(holder.is_none());
        drop(holder);
        drop(try_begin_at(&Cx::for_testing(), root.path()).unwrap());
    }

    #[test]
    fn unwinding_an_admitted_window_cleans_up_and_allows_poison_recovery() {
        if isolated() {
            return;
        }
        let root = archive_root();
        let before = tracking();
        assert!(
            std::panic::catch_unwind(|| {
                let _window = try_begin_at(&Cx::for_testing(), root.path()).unwrap();
                panic!("injected admitted repair panic");
            })
            .is_err()
        );
        assert_eq!(tracking(), (before.0, before.1, before.2 + 2));
        assert!(crate::archive_publication_fence_holder().is_none());
        let window = try_begin_at(&Cx::for_testing(), root.path()).unwrap();
        assert_eq!(tracking().0, 1);
        drop(window);
        assert_eq!(tracking().0, 0);
        assert_eq!(tracking().1, 0);
    }

    #[test]
    fn exhausted_depth_refuses_without_changing_any_epoch() {
        if isolated() {
            return;
        }
        let root = archive_root();
        let before = tracking();
        let stamp = token(root.path());
        crate::ARCHIVE_MUTATION_DEPTH.with(|depth| depth.set(u32::MAX));
        let refused = try_begin_at(&Cx::for_testing(), root.path()).err();
        let observed = tracking();
        crate::ARCHIVE_MUTATION_DEPTH.with(|depth| depth.set(before.0));
        assert!(
            refused
                .unwrap()
                .to_string()
                .contains("mutation nesting exhausted")
        );
        assert_eq!(observed, (u32::MAX, before.1, before.2));
        assert_eq!(token(root.path()), stamp);
        assert!(crate::archive_publication_fence_holder().is_none());
    }
}
