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
    pub(crate) fn try_begin_repair(path: &Path, cancelled: impl FnMut() -> bool) -> Result<Self> {
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
    fn direct_writer_refuses_busy_fence_without_waiting_for_owner() {
        if isolated() {
            return;
        }
        let root = archive_root();
        let config = mcp_agent_mail_core::Config {
            storage_root: root.path().to_path_buf(),
            ..Default::default()
        };
        // A warm project takes the short-hold path; a cold one's first write
        // keeps the bounded window around the directories it creates (sh27).
        // Warm it before the fence is held: creating a directory takes it.
        let warm = crate::ensure_archive(&config, "fence-repair").unwrap();
        crate::ensure_dir(&warm.root.join("file_reservations")).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        // The owner must outlast both writers' admission budgets, so each
        // refusal comes from a budget, never from the owner timing out.
        let owner_patience = crate::DIRECT_FENCE_ADMISSION_BUDGET * 2 + Duration::from_secs(15);
        let owner = std::thread::spawn(move || {
            crate::with_archive_snapshot_publication_fence(|| {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(owner_patience)
            })
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let outcomes = ["fence-repair", "fence-cold"].map(|slug| {
            // A reservation with something to publish: an empty batch needs
            // no fence at all.
            let op = crate::WriteOp::FileReservation {
                project_slug: slug.to_string(),
                config: config.clone(),
                reservations: vec![serde_json::json!({
                    "id": 3,
                    "agent": "GreenCastle",
                    "path_pattern": "src/**",
                    "exclusive": true,
                })],
            };
            let started = Instant::now();
            let outcome = crate::write_op_sync_direct(&op);
            (slug, outcome, started.elapsed())
        });
        let still_held = crate::archive_publication_fence_holder().is_some();

        let _ = release_tx.send(());
        let explicitly_released = owner.join().unwrap();
        assert!(
            explicitly_released.is_ok(),
            "direct writer waited for the owner's cleanup timeout"
        );
        for (slug, outcome, elapsed) in outcomes {
            assert!(
                matches!(
                    outcome,
                    crate::DirectArchiveWrite::Failed(crate::StorageError::LockContention {
                        ref message
                    }) if message.contains("publication fence busy")
                ),
                "{slug}: busy direct writer must fail with publication-fence contention"
            );
            assert!(
                elapsed < crate::DIRECT_FENCE_ADMISSION_BUDGET + Duration::from_secs(2),
                "{slug}: busy direct writer blocked past its admission budget: {elapsed:?}"
            );
        }
        assert!(still_held, "writer returned only after the owner released");
    }

    /// sh27: a direct reservation writer that finds the project's archive lock
    /// busy gives the fence back while it waits, instead of sleeping on it,
    /// and publishes once the project frees within its budget.
    #[test]
    fn direct_reservation_writer_releases_the_fence_while_the_project_is_busy() {
        if isolated() {
            return;
        }
        let root = archive_root();
        let config = mcp_agent_mail_core::Config {
            storage_root: root.path().to_path_buf(),
            ..Default::default()
        };
        let archive = crate::ensure_archive(&config, "fence-repair").unwrap();
        // A warm project: a first write keeps the bounded window around the
        // directories it creates.
        crate::ensure_dir(&archive.root.join("file_reservations")).unwrap();
        let project = crate::archive_process_lock(&archive).unwrap();
        let held = project.lock().unwrap();
        let op = crate::WriteOp::FileReservation {
            project_slug: "fence-repair".to_string(),
            config,
            reservations: vec![serde_json::json!({
                "id": 4,
                "agent": "GreenCastle",
                "path_pattern": "src/**",
                "exclusive": true,
            })],
        };
        let writer = std::thread::spawn(move || crate::write_op_sync_direct(&op));

        std::thread::sleep(Duration::from_millis(50));
        // While the writer waits for the project, asking for the fence gets it
        // at once, and no mutation window opens: the epoch token, rewritten on
        // both edges of every window, stays put.
        let token_before = token(root.path());
        let mut slowest = Duration::ZERO;
        for _ in 0..20 {
            let asked = Instant::now();
            while crate::ARCHIVE_PUBLICATION_FENCE.try_lock().is_err() {
                assert!(
                    asked.elapsed() < Duration::from_secs(2),
                    "the fence never came free"
                );
                std::thread::sleep(Duration::from_micros(100));
            }
            slowest = slowest.max(asked.elapsed());
            std::thread::sleep(Duration::from_millis(10));
        }
        let token_after = token(root.path());
        drop(held);
        let result = writer.join().unwrap();

        assert!(
            slowest < Duration::from_millis(50),
            "the writer waiting for the project must not hold the fence: slowest acquisition {slowest:?}"
        );
        assert_eq!(
            token_after, token_before,
            "the writer must wait for the project without opening a mutation window"
        );
        assert!(
            matches!(result, crate::DirectArchiveWrite::Written),
            "{result:?}"
        );
    }

    fn busy_project_does_not_hold_publication(foreign_flock: bool) {
        use fs2::FileExt;
        use std::io::Read;

        let root = archive_root();
        let config = mcp_agent_mail_core::Config {
            storage_root: root.path().to_path_buf(),
            ..Default::default()
        };
        let archive = crate::ensure_archive(&config, "fence-repair").unwrap();
        let other = crate::ensure_archive(&config, "unrelated-writer").unwrap();
        crate::ensure_dir(&other.root.join("file_reservations")).unwrap();
        let process = crate::archive_process_lock(&archive).unwrap();
        let held_process = (!foreign_flock).then(|| process.lock().unwrap());
        let lock_path = archive.root.join(".archive.lock");
        let owner_path = archive.root.join(".archive.lock.owner.json");
        let held_file = foreign_flock.then(|| {
            std::fs::write(&lock_path, b"foreign lock evidence").unwrap();
            std::fs::write(&owner_path, b"foreign owner evidence").unwrap();
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                .unwrap();
            file.try_lock_exclusive().unwrap();
            file
        });
        let (entered_tx, entered_rx) = mpsc::channel();
        let repair = std::thread::spawn(move || {
            let mutation = try_begin_at(&Cx::for_testing(), &archive.repo_root).unwrap();
            let mut announced = false;
            // A long fixture budget makes waiting distinguishable from
            // immediate refusal without a fragile sub-250ms timing gate.
            // Production still uses its unchanged 250ms overall budget.
            let result = super::super::with_budget(
                &archive,
                Duration::from_secs(30),
                || {
                    if !announced {
                        entered_tx.send(()).unwrap();
                        announced = true;
                    }
                    false
                },
                || Ok(()),
            );
            drop(mutation);
            result
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (completed_tx, completed_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            let result = crate::write_op_sync_direct(&crate::WriteOp::FileReservation {
                project_slug: "unrelated-writer".to_string(),
                config,
                reservations: vec![serde_json::json!({
                    "id": 41,
                    "agent": "GreenCastle",
                    "path_pattern": "src/**",
                    "exclusive": true,
                })],
            });
            completed_tx.send(result).unwrap();
        });
        let completed_while_owned = completed_rx.recv_timeout(Duration::from_secs(2));
        let owner_preserved = if foreign_flock {
            // Windows denies reads of an exclusively locked range through a
            // second handle, including in this process. Inspect the evidence
            // through its still-locked owner without releasing its authority.
            let mut owner = held_file.as_ref().expect("foreign file owner");
            let mut evidence = Vec::new();
            owner.read_to_end(&mut evidence).unwrap();
            let contender = std::fs::OpenOptions::new()
                .write(true)
                .open(&lock_path)
                .unwrap();
            contender.try_lock_exclusive().is_err()
                && evidence == b"foreign lock evidence"
                && std::fs::read(&owner_path).unwrap() == b"foreign owner evidence"
        } else {
            matches!(process.try_lock(), Err(TryLockError::WouldBlock))
        };
        // Always release the real owner and join both workers before asserting,
        // including on the old implementation's expected timeout failure.
        drop(held_file);
        drop(held_process);
        let result = repair.join().unwrap();
        writer.join().unwrap();
        assert!(
            matches!(
                completed_while_owned,
                Ok(crate::DirectArchiveWrite::Written)
            ),
            "an unrelated request remained behind a repair's project-lock wait: {completed_while_owned:?}"
        );
        assert!(
            matches!(result, Err(StorageError::LockContention { ref message })
            if message.contains("project lock busy")),
            "{result:?}"
        );
        assert!(owner_preserved, "repair changed the held owner's evidence");
        let written: serde_json::Value = serde_json::from_slice(
            &std::fs::read(other.root.join("file_reservations/id-41.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["id"], 41);
        assert_eq!(written["agent"], "GreenCastle");
    }

    #[test]
    fn fenced_repair_yields_a_busy_project_mutex_to_unrelated_publication() {
        if isolated() {
            return;
        }
        busy_project_does_not_hold_publication(false);
    }

    #[test]
    fn fenced_repair_yields_a_busy_project_flock_without_changing_its_owner() {
        if isolated() {
            return;
        }
        busy_project_does_not_hold_publication(true);
    }

    #[test]
    fn fenced_project_admission_rechecks_cancellation_before_publication() {
        if isolated() {
            return;
        }
        let root = archive_root();
        let archive = crate::ensure_archive(
            &mcp_agent_mail_core::Config {
                storage_root: root.path().to_path_buf(),
                ..Default::default()
            },
            "fence-repair",
        )
        .unwrap();
        let mutation = try_begin_at(&Cx::for_testing(), root.path()).unwrap();
        for cancel_at in [1, 4] {
            let mut checks = 0;
            let result: Result<()> = archive.with_repair_lock(
                || {
                    checks += 1;
                    checks == cancel_at
                },
                || panic!("cancelled repair must not publish"),
            );
            assert!(matches!(result, Err(StorageError::Io(ref error))
                if error.kind() == std::io::ErrorKind::Interrupted));
            assert!(
                crate::archive_process_lock(&archive)
                    .unwrap()
                    .try_lock()
                    .is_ok()
            );
            archive.with_repair_lock(|| false, || Ok(())).unwrap();
        }
        drop(mutation);
        assert!(crate::archive_publication_fence_holder().is_none());
        assert!(crate::ARCHIVE_PUBLICATION_FENCE.try_lock().is_ok());
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
