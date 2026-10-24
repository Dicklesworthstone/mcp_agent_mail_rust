//! Bounded recovery of a stale, monolithic Git multi-pack index (GH#356).
//!
//! The MIDX is derived. Never remove a pack, pack index or temporary pack to
//! repair it. First prove the object store is readable WITHOUT the
//! suspect MIDX, preserve its exact bytes, then ask Git to rebuild through its
//! own publication lock. A failed verification is not proof of missing mail.
//! Incremental chains, unfamiliar formats and ambiguous paths are refused.
//!
//! Only Unix currently has the shared runner's bounded, thread-free pipe
//! capture. Other platforms retain the original maintenance failure.

use super::{MaintenanceObservedEffect, MaintenanceReport};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

fn is_stale_index_failure(report: &MaintenanceReport) -> bool {
    !report.success
        && report.error.as_deref().is_some_and(|error| {
            let error = error.to_ascii_lowercase();
            error.contains("could not load pack") && error.contains("multi-pack-index")
        })
}

/// One normal attempt, at most one repair, and at most one normal retry.
/// Preserve both attempts' diagnostics. A repaired index alone cannot turn a
/// failed or silently skipped maintenance task into a successful report.
pub(super) fn run_with_recovery(
    git_dir: &Path,
    stop: &AtomicBool,
    run: impl FnMut(&Path) -> MaintenanceReport,
) -> MaintenanceReport {
    run_with(git_dir, stop, run, repair)
}

fn run_with(
    git_dir: &Path,
    stop: &AtomicBool,
    mut run: impl FnMut(&Path) -> MaintenanceReport,
    repair: impl FnOnce(&Path, &AtomicBool) -> Result<Option<String>, String>,
) -> MaintenanceReport {
    let mut first = run(git_dir);
    if !first.success {
        // Partial loose-object work is not an accepted maintenance effect
        // when another task failed. Keep the raw before/after metrics.
        first.observed_effect = MaintenanceObservedEffect::Unknown;
    }
    if !is_stale_index_failure(&first) || stop.load(Ordering::Acquire) {
        return first;
    }
    match repair(git_dir, stop) {
        Ok(Some(evidence)) => {
            first.notes.push(evidence);
            if stop.load(Ordering::Acquire) {
                first
                    .notes
                    .push("MIDX repaired; maintenance retry cancelled by shutdown".into());
                return first;
            }
            let mut retried = run(git_dir);
            // Metrics and observed_effect belong to the retry, not a synthetic
            // first-to-last delta that could turn partial failure into success.
            first.notes.push(format!(
                "initial maintenance failed before MIDX repair: {}",
                first.error.as_deref().unwrap_or("unknown error")
            ));
            first.notes.append(&mut retried.notes);
            retried.notes = first.notes;
            retried.lock_reaped |= first.lock_reaped;
            if let Some(evidence) = first.lock_evidence {
                retried
                    .notes
                    .push(format!("initial maintenance lock evidence: {evidence}"));
            }
            if !retried.success {
                retried.observed_effect = MaintenanceObservedEffect::Unknown;
            }
            retried
        }
        Ok(None) => {
            first.notes.push(
                "MIDX verification did not establish a stale index; no repair attempted".into(),
            );
            first
        }
        Err(error) => {
            first.notes.push(format!("MIDX recovery refused: {error}"));
            first
        }
    }
}

#[cfg(not(unix))]
fn repair(_: &Path, _: &AtomicBool) -> Result<Option<String>, String> {
    Err("bounded MIDX recovery is currently supported only on Unix".into())
}

#[cfg(unix)]
fn repair(git_dir: &Path, stop: &AtomicBool) -> Result<Option<String>, String> {
    unix::repair(git_dir, stop).map_err(|error| error.to_string())
}

#[cfg(unix)]
mod unix {
    use mcp_agent_mail_core::git_cmd::GitCmd;
    use nix::fcntl::OFlag;
    use sha2::{Digest, Sha256};
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Component, Path, PathBuf};
    use std::process::Output;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;
    const MAX_RETAINED_INDEXES: usize = 4;
    const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
    const REPAIR_BUDGET: Duration = Duration::from_secs(20);

    fn refused(message: &str) -> io::Error {
        io::Error::other(message)
    }

    fn live(stop: &AtomicBool) -> io::Result<()> {
        if stop.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "shutdown requested",
            ));
        }
        if mcp_agent_mail_core::global_metrics()
            .system
            .disk_pressure_level
            .load()
            >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64()
        {
            return Err(refused(
                "critical disk pressure; no index publication admitted",
            ));
        }
        Ok(())
    }

    /// Do not follow an operator's symlink into a different evidence namespace.
    fn directory(path: &Path) -> io::Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut checked = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::ParentDir => return Err(refused("parent traversal in archive path")),
                Component::CurDir => continue,
                _ => checked.push(component.as_os_str()),
            }
            if !fs::symlink_metadata(&checked)?.file_type().is_dir() {
                return Err(refused("archive/evidence path is not a real directory"));
            }
        }
        Ok(checked)
    }

    fn absent(path: &Path) -> io::Result<()> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
            Ok(_) => Err(refused(&format!(
                "occupied control path: {}",
                path.display()
            ))),
        }
    }

    fn controls(git_dir: &Path) -> io::Result<()> {
        for relative in [
            "objects/maintenance.lock",
            "objects/pack/multi-pack-index.lock",
            "objects/pack/multi-pack-index.d",
        ] {
            absent(&git_dir.join(relative))?;
        }
        Ok(())
    }

    #[derive(PartialEq, Eq)]
    struct Index {
        bytes: Vec<u8>,
        device: u64,
        inode: u64,
    }

    fn read_index(path: &Path) -> io::Result<Index> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
            .open(path)?;
        let before = file.metadata()?;
        if !before.is_file() || before.nlink() != 1 || before.len() > MAX_INDEX_BYTES {
            return Err(refused(
                "MIDX must be a single-link regular file within the byte bound",
            ));
        }
        let mut bytes = Vec::new();
        (&file).take(MAX_INDEX_BYTES + 1).read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        let named = fs::symlink_metadata(path)?;
        if bytes.len() as u64 != before.len()
            || before.len() != after.len()
            || before.modified()? != after.modified()?
            || !named.is_file()
            || named.dev() != before.dev()
            || named.ino() != before.ino()
        {
            return Err(refused("MIDX changed during observation"));
        }
        // Only the monolithic v1 format, for SHA-1 or SHA-256 repositories.
        // A base-layer count is never permission to rewrite an incremental chain.
        if bytes.len() < 12
            || &bytes[..4] != b"MIDX"
            || bytes[4] != 1
            || !matches!(bytes[5], 1 | 2)
            || bytes[7] != 0
        {
            return Err(refused("unrecognized or incremental MIDX format"));
        }
        Ok(Index {
            bytes,
            device: before.dev(),
            inode: before.ino(),
        })
    }

    fn git(
        git_dir: &Path,
        stop: &AtomicBool,
        started: Instant,
        args: &[&str],
    ) -> io::Result<Output> {
        live(stop)?;
        let remaining = REPAIR_BUDGET.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "MIDX recovery budget exhausted",
            ));
        }
        let output = GitCmd::new(git_dir)
            .arg("--git-dir")
            .arg(git_dir.as_os_str())
            .args(args.iter().copied())
            .env("LC_ALL", "C")
            .timeout(remaining.min(COMMAND_TIMEOUT))
            .run()?;
        // The shared runner owns pipe draining and child reaping. Shutdown and
        // pressure are checked again before any later phase, including writes.
        live(stop)?;
        Ok(output)
    }

    fn require_success(output: Output, phase: &str) -> io::Result<()> {
        if !output.status.success() {
            return Err(refused(&format!(
                "{phase} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(2048)
                    .collect::<String>()
            )));
        }
        Ok(())
    }

    fn child_directory(parent: &Path, name: &str) -> io::Result<PathBuf> {
        let path = parent.join(name);
        match fs::create_dir(&path) {
            Ok(()) => {
                File::open(parent)?.sync_all()?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        directory(&path)
    }

    /// Content-addressed, create-new evidence avoids copying the same broken
    /// index every cycle. A hard cap stops distinct failures growing without bound.
    fn preserve(git_dir: &Path, original: &Index) -> io::Result<PathBuf> {
        let root = child_directory(git_dir, "agent-mail-maintenance")?;
        let root = child_directory(&root, "midx-recovery")?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags((OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW).bits())
            .open(&root)?;
        // Serialize cooperating evidence writers without creating another
        // control file. The directory lock drops before any Git invocation.
        fs2::FileExt::try_lock_exclusive(&directory)?;
        // The pinned SHA-2 output type does not implement LowerHex. Encode
        // every byte explicitly, retaining leading zeros in its stable name.
        let mut digest = String::with_capacity(64);
        for byte in Sha256::digest(&original.bytes) {
            use std::fmt::Write as _;
            write!(&mut digest, "{byte:02x}")
                .map_err(|_| refused("cannot encode MIDX evidence identity"))?;
        }
        let path = root.join(format!("multi-pack-index-{digest}.original"));
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                if read_index(&path)?.bytes != original.bytes {
                    return Err(refused(
                        "retained MIDX evidence does not match; preserving it",
                    ));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut count = 0;
        for entry in fs::read_dir(&root)? {
            entry?;
            count += 1;
            if count >= MAX_RETAINED_INDEXES {
                return Err(refused(
                    "MIDX recovery evidence budget full; no automatic cleanup",
                ));
            }
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(&original.bytes)?;
        file.sync_all()?;
        directory.sync_all()?;
        Ok(path)
    }

    pub(super) fn repair(git_dir: &Path, stop: &AtomicBool) -> io::Result<Option<String>> {
        live(stop)?;
        let started = Instant::now();
        let git_dir = directory(git_dir)?;
        let pack_dir = directory(&git_dir.join("objects/pack"))?;
        controls(&git_dir)?;
        let path = pack_dir.join("multi-pack-index");
        let original = read_index(&path)?;
        let verify = git(&git_dir, stop, started, &["multi-pack-index", "verify"])?;
        if verify.status.success() {
            return Ok(None);
        }
        // Signals, invocation errors and unsupported commands are not a stale-index verdict.
        if verify.status.code() != Some(1) {
            return Err(refused(
                "MIDX verification did not return a content-failure verdict",
            ));
        }
        require_success(
            git(
                &git_dir,
                stop,
                started,
                &[
                    "-c",
                    "core.multiPackIndex=false",
                    "fsck",
                    "--full",
                    "--no-dangling",
                ],
            )?,
            "independent object-store verification",
        )?;
        controls(&git_dir)?;
        if read_index(&path)? != original {
            return Err(refused(
                "MIDX changed after verification; no replacement admitted",
            ));
        }
        live(stop)?;
        let evidence = preserve(&git_dir, &original)?;
        controls(&git_dir)?;
        if read_index(&path)? != original {
            return Err(refused(
                "MIDX changed while preserving evidence; no replacement admitted",
            ));
        }
        // Git takes multi-pack-index.lock and publishes atomically. Disabling
        // reads of the suspect derived index lets the writer enumerate actual
        // per-pack indexes, without unlinking MIDX or any object/pack ourselves.
        require_success(
            git(
                &git_dir,
                stop,
                started,
                &[
                    "-c",
                    "core.multiPackIndex=false",
                    "multi-pack-index",
                    "write",
                ],
            )?,
            "MIDX rebuild",
        )?;
        require_success(
            git(&git_dir, stop, started, &["multi-pack-index", "verify"])?,
            "rebuilt MIDX verification",
        )?;
        Ok(Some(format!(
            "rebuilt and verified stale MIDX; retained original at {}",
            evidence.display()
        )))
    }

    #[cfg(test)]
    mod evidence_tests {
        use super::*;

        fn index(ordinal: u8) -> Index {
            let mut bytes = b"MIDX\x01\x01\x04\x00\x00\x00\x00\x01".to_vec();
            bytes.extend_from_slice(&[ordinal; 40]);
            Index {
                bytes,
                device: 0,
                inode: 0,
            }
        }

        #[test]
        fn evidence_filename_uses_the_complete_lowercase_sha256() {
            let scratch = tempfile::tempdir().unwrap();
            let root = scratch.path().canonicalize().unwrap();
            let original = index(0);
            let path = preserve(&root, &original).unwrap();
            assert_eq!(
                path.file_name().unwrap(),
                "multi-pack-index-42811216b5546b624c307f2cd91156c9d95e12b17631ff8f6d8310d50f1ae475.original"
            );
            assert_eq!(std::fs::read(&path).unwrap(), original.bytes);
            assert_eq!(preserve(&root, &original).unwrap(), path);
        }

        #[test]
        fn evidence_is_idempotent_capped_and_never_overwrites_a_conflict() {
            let scratch = tempfile::tempdir().unwrap();
            let root = scratch.path().canonicalize().unwrap();
            for ordinal in 0..u8::try_from(MAX_RETAINED_INDEXES).unwrap() {
                let original = index(ordinal);
                let path = preserve(&root, &original).unwrap();
                assert_eq!(std::fs::read(&path).unwrap(), original.bytes);
                assert_eq!(preserve(&root, &original).unwrap(), path);
            }
            assert!(preserve(&root, &index(99)).is_err());
            let dir = root.join("agent-mail-maintenance/midx-recovery");
            assert_eq!(
                std::fs::read_dir(&dir).unwrap().count(),
                MAX_RETAINED_INDEXES
            );
            let original = index(0);
            let path = preserve(&root, &original).unwrap();
            // Only a test-owned evidence file is changed. Publication must
            // refuse that occupied name, not overwrite it or allocate a twin.
            std::fs::write(&path, b"retain operator evidence").unwrap();
            assert!(preserve(&root, &original).is_err());
            assert_eq!(std::fs::read(path).unwrap(), b"retain operator evidence");
            assert_eq!(
                std::fs::read_dir(dir).unwrap().count(),
                MAX_RETAINED_INDEXES
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[cfg(unix)]
    use std::path::PathBuf;

    fn failed(message: &str) -> MaintenanceReport {
        MaintenanceReport {
            error: Some(message.into()),
            ..Default::default()
        }
    }

    fn stale() -> MaintenanceReport {
        failed("exit 1: could not load pack; failed to write multi-pack-index")
    }

    #[test]
    fn unrelated_failures_and_success_never_enter_index_recovery() {
        for report in [
            failed("permission denied"),
            failed("could not load pack"),
            failed("failed to write multi-pack-index"),
            failed("timed out"),
            MaintenanceReport {
                success: true,
                ..stale()
            },
        ] {
            let expected = report.error.clone();
            let result = run_with(
                Path::new("unused"),
                &AtomicBool::new(false),
                |_| report.clone(),
                |_, _| panic!("unrelated result cannot authorize repair"),
            );
            assert_eq!(result.error, expected);
        }
    }

    #[test]
    fn a_verified_repair_retries_once_and_keeps_both_attempts_diagnostics() {
        let calls = Cell::new(0);
        let result = run_with(
            Path::new("unused"),
            &AtomicBool::new(false),
            |_| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    stale()
                } else {
                    MaintenanceReport {
                        success: true,
                        loose_before: Some(17),
                        loose_after: Some(3),
                        observed_effect: MaintenanceObservedEffect::ProgressObserved,
                        notes: vec!["retry note".into()],
                        ..Default::default()
                    }
                }
            },
            |_, _| Ok(Some("original retained".into())),
        );
        assert_eq!(calls.get(), 2);
        assert!(result.success);
        assert_eq!(
            (result.loose_before, result.loose_after),
            (Some(17), Some(3))
        );
        assert!(result.notes.iter().any(|note| note == "original retained"));
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("initial maintenance failed"))
        );
        assert!(result.notes.iter().any(|note| note == "retry note"));
    }

    #[test]
    fn repair_refusal_or_a_clean_index_never_retries_maintenance() {
        for repair in [Err("source objects unavailable".into()), Ok(None)] {
            let calls = Cell::new(0);
            let result = run_with(
                Path::new("unused"),
                &AtomicBool::new(false),
                |_| {
                    calls.set(calls.get() + 1);
                    stale()
                },
                |_, _| repair,
            );
            assert_eq!(calls.get(), 1);
            assert!(!result.success);
            assert_eq!(result.error, stale().error);
            assert_eq!(result.notes.len(), 1);
        }
    }

    #[test]
    fn a_second_failure_is_terminal_even_when_it_reports_partial_progress() {
        let calls = Cell::new(0);
        let result = run_with(
            Path::new("unused"),
            &AtomicBool::new(false),
            |_| {
                calls.set(calls.get() + 1);
                MaintenanceReport {
                    observed_effect: MaintenanceObservedEffect::ProgressObserved,
                    ..stale()
                }
            },
            |_, _| Ok(Some("original retained".into())),
        );
        assert_eq!(calls.get(), 2);
        assert!(!result.success);
        assert_eq!(result.observed_effect, MaintenanceObservedEffect::Unknown);
        assert_eq!(result.error, stale().error);
    }

    #[test]
    fn shutdown_prevents_repair_or_the_post_repair_retry() {
        for already_stopped in [false, true] {
            let stop = AtomicBool::new(already_stopped);
            let calls = Cell::new(0);
            let repairs = Cell::new(0);
            let result = run_with(
                Path::new("unused"),
                &stop,
                |_| {
                    calls.set(calls.get() + 1);
                    stale()
                },
                |_, stop| {
                    repairs.set(repairs.get() + 1);
                    stop.store(true, Ordering::Release);
                    Ok(Some("original retained".into()))
                },
            );
            assert_eq!(calls.get(), 1);
            assert_eq!(repairs.get(), i32::from(!already_stopped));
            assert!(!result.success);
        }
    }

    #[cfg(unix)]
    fn isolated() -> bool {
        // Disk-pressure metrics and the core Git locks are process-global.
        // Execute real-repository tests in a child, not beside mutating tests.
        const CHILD: &str = "AM_TEST_MIDX_RECOVERY_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named test");
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let mut log = tempfile::tempfile().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .stdout(log.try_clone().unwrap())
            .stderr(log.try_clone().unwrap())
            .spawn()
            .unwrap();
        let start = std::time::Instant::now();
        while child.try_wait().unwrap().is_none() {
            if start.elapsed() >= std::time::Duration::from_secs(90) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("isolated MIDX recovery test exceeded its deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let status = child.wait().unwrap();
        use std::io::{Read, Seek, SeekFrom};
        log.seek(SeekFrom::Start(0)).unwrap();
        let mut output = String::new();
        log.read_to_string(&mut output).unwrap();
        assert!(
            status.success() && output.contains("1 passed; 0 failed"),
            "{output}"
        );
        true
    }

    #[cfg(unix)]
    struct Fixture {
        root: tempfile::TempDir,
        git_dir: PathBuf,
        blobs: Vec<String>,
        packs: Vec<String>,
    }

    #[cfg(unix)]
    impl Fixture {
        fn git(&self, args: &[&str], input: Option<String>) -> std::process::Output {
            let mut command = mcp_agent_mail_core::git_cmd::GitCmd::new(self.root.path())
                .arg("--git-dir")
                .arg(self.git_dir.as_os_str())
                .args(args.iter().copied())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .timeout(std::time::Duration::from_secs(5));
            if let Some(input) = input {
                command = command.stdin(input.into_bytes());
            }
            command.run().expect("real Git subprocess")
        }

        fn ok(&self, args: &[&str], input: Option<String>) -> String {
            let output = self.git(args, input);
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().into()
        }

        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let mut fixture = Self {
                git_dir: root.path().canonicalize().unwrap().join("archive.git"),
                root,
                blobs: Vec::new(),
                packs: Vec::new(),
            };
            fixture.ok(&["init", "--bare"], None);
            fixture.ok(&["config", "gc.auto", "0"], None);
            for ordinal in 0..3 {
                let blob = fixture.ok(
                    &["hash-object", "-w", "--stdin"],
                    Some(format!("mail {ordinal}\n")),
                );
                fixture.ok(
                    &["update-ref", &format!("refs/tags/mail-{ordinal}"), &blob],
                    None,
                );
                fixture.blobs.push(blob);
                let prefix = fixture.git_dir.join("objects/pack/pack");
                let pack = fixture.ok(
                    &["pack-objects", prefix.to_str().unwrap()],
                    Some(fixture.blobs.join("\n") + "\n"),
                );
                fixture.packs.push(pack);
            }
            fixture.ok(&["multi-pack-index", "write"], None);
            fixture.ok(&["multi-pack-index", "verify"], None);
            fixture
        }

        fn index(&self) -> PathBuf {
            self.git_dir.join("objects/pack/multi-pack-index")
        }

        fn make_stale(&self) {
            let retained = self.root.path().join("retained-packs");
            std::fs::create_dir(&retained).unwrap();
            for pack in &self.packs[..2] {
                for extension in ["pack", "idx"] {
                    let name = format!("pack-{pack}.{extension}");
                    std::fs::rename(
                        self.git_dir.join("objects/pack").join(&name),
                        retained.join(name),
                    )
                    .unwrap();
                }
            }
            assert_eq!(
                self.git(&["multi-pack-index", "verify"], None)
                    .status
                    .code(),
                Some(1)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_stale_index_rebuild_preserves_original_and_every_mail_object() {
        if isolated() {
            return;
        }
        let fixture = Fixture::new();
        fixture.make_stale();
        let before = std::fs::read(fixture.index()).unwrap();
        let refs = fixture.ok(&["show-ref"], None);
        assert!(
            repair(&fixture.git_dir, &AtomicBool::new(false))
                .unwrap()
                .is_some()
        );
        fixture.ok(&["multi-pack-index", "verify"], None);
        fixture.ok(&["fsck", "--full", "--no-dangling"], None);
        assert_eq!(fixture.ok(&["show-ref"], None), refs);
        for (ordinal, blob) in fixture.blobs.iter().enumerate() {
            assert_eq!(
                fixture.ok(&["cat-file", "-p", blob], None),
                format!("mail {ordinal}")
            );
        }
        let evidence = fixture.git_dir.join("agent-mail-maintenance/midx-recovery");
        let entries = std::fs::read_dir(&evidence)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(std::fs::read(entries[0].path()).unwrap(), before);
        // Restoring the same stale bytes models a repeated interrupted cycle;
        // preserving its evidence is idempotent, not another copy per attempt.
        std::fs::write(fixture.index(), &before).unwrap();
        assert!(
            repair(&fixture.git_dir, &AtomicBool::new(false))
                .unwrap()
                .is_some()
        );
        assert_eq!(std::fs::read_dir(evidence).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn real_missing_object_refuses_rebuild_instead_of_hiding_data_loss() {
        if isolated() {
            return;
        }
        let fixture = Fixture::new();
        fixture.make_stale();
        let retained = fixture.root.path().join("retained-packs");
        for extension in ["pack", "idx"] {
            let name = format!("pack-{}.{extension}", fixture.packs[2]);
            std::fs::rename(
                fixture.git_dir.join("objects/pack").join(&name),
                retained.join(name),
            )
            .unwrap();
        }
        let blob = &fixture.blobs[2];
        std::fs::rename(
            fixture
                .git_dir
                .join("objects")
                .join(&blob[..2])
                .join(&blob[2..]),
            retained.join("missing-mail-blob"),
        )
        .unwrap();
        let before = std::fs::read(fixture.index()).unwrap();
        assert!(repair(&fixture.git_dir, &AtomicBool::new(false)).is_err());
        assert_eq!(std::fs::read(fixture.index()).unwrap(), before);
        assert!(
            !fixture
                .git_dir
                .join("agent-mail-maintenance/midx-recovery")
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_healthy_index_and_occupied_controls_are_not_rewritten() {
        if isolated() {
            return;
        }
        let fixture = Fixture::new();
        let before = std::fs::read(fixture.index()).unwrap();
        assert!(
            repair(&fixture.git_dir, &AtomicBool::new(false))
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(fixture.index()).unwrap(), before);
        for name in [
            "objects/maintenance.lock",
            "objects/pack/multi-pack-index.lock",
            "objects/pack/multi-pack-index.d",
        ] {
            let fixture = Fixture::new();
            fixture.make_stale();
            let before = std::fs::read(fixture.index()).unwrap();
            std::fs::write(fixture.git_dir.join(name), b"foreign control evidence").unwrap();
            assert!(repair(&fixture.git_dir, &AtomicBool::new(false)).is_err());
            assert_eq!(std::fs::read(fixture.index()).unwrap(), before);
            assert_eq!(
                std::fs::read(fixture.git_dir.join(name)).unwrap(),
                b"foreign control evidence"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_linked_and_unrecognized_indexes_are_preserved() {
        if isolated() {
            return;
        }
        for mode in ["symlink", "hardlink", "foreign"] {
            let fixture = Fixture::new();
            let original = std::fs::read(fixture.index()).unwrap();
            let witness = fixture.root.path().join("original-midx");
            match mode {
                "symlink" => {
                    std::fs::rename(fixture.index(), &witness).unwrap();
                    std::os::unix::fs::symlink(&witness, fixture.index()).unwrap();
                }
                "hardlink" => std::fs::hard_link(fixture.index(), &witness).unwrap(),
                _ => {
                    std::fs::rename(fixture.index(), &witness).unwrap();
                    std::fs::write(fixture.index(), b"operator evidence, not a MIDX").unwrap();
                }
            }
            assert!(repair(&fixture.git_dir, &AtomicBool::new(false)).is_err());
            assert_eq!(std::fs::read(witness).unwrap(), original);
            assert!(
                !fixture
                    .git_dir
                    .join("agent-mail-maintenance/midx-recovery")
                    .exists()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_and_critical_pressure_refuse_before_creating_evidence() {
        if isolated() {
            return;
        }
        let fixture = Fixture::new();
        fixture.make_stale();
        let before = std::fs::read(fixture.index()).unwrap();
        assert!(repair(&fixture.git_dir, &AtomicBool::new(true)).is_err());
        struct RestorePressure(u64);
        impl Drop for RestorePressure {
            fn drop(&mut self) {
                mcp_agent_mail_core::global_metrics()
                    .system
                    .disk_pressure_level
                    .set(self.0);
            }
        }
        let pressure = &mcp_agent_mail_core::global_metrics()
            .system
            .disk_pressure_level;
        let _restore = RestorePressure(pressure.load());
        pressure.set(mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64());
        assert!(repair(&fixture.git_dir, &AtomicBool::new(false)).is_err());
        assert_eq!(std::fs::read(fixture.index()).unwrap(), before);
        assert!(
            !fixture
                .git_dir
                .join("agent-mail-maintenance/midx-recovery")
                .exists()
        );
    }
}
