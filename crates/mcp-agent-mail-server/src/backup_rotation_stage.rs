//! Unix automatic-backup staging through the shared recovery namespace mover.
//!
//! Rotation selects its own keep set. This adapter only publishes that set
//! into private quarantine: it does not apply a second retention policy, copy
//! or delete evidence, or interpret an incomplete move as a file kept in place.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use mcp_agent_mail_db::recovery_retention::{
    DebrisArtifact, DebrisCategory, ReclaimOutcome, ReclaimPlan, consolidate_debris,
};

const MAX_BATCH_DIRECTORY_ATTEMPTS: u32 = 128;

/// Stage an already-selected set of regular backup files.
///
/// `Err` means the shared mover did not begin its artifact loop; none of this
/// batch's sources was moved. `Ok` can contain per-artifact failures, including
/// a completed rename whose directory sync or namespace revalidation failed.
/// Those failures preserve their destination details and MUST NOT be counted
/// as either durably staged or necessarily still present at the source.
///
/// Each successful directory claim remains held throughout the shared batch.
/// Only a pre-batch `AlreadyExists` permits another name attempt. The shared
/// mover returns post-move failures inside `ReclaimOutcome`, never here, so
/// they cannot accidentally cause the entire batch to run a second time.
pub(super) fn stage_backup_paths(
    paths: &[PathBuf],
    quarantine_parent: &Path,
    stem: &str,
) -> io::Result<ReclaimOutcome> {
    if paths.is_empty() {
        return Ok(ReclaimOutcome::default());
    }
    let mut components = Path::new(stem).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "rotation quarantine stem must be one ordinary path component",
        ));
    }

    let mut preflight = ReclaimOutcome::default();
    let mut plan = ReclaimPlan::default();
    for path in paths {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                plan.total_bytes = plan.total_bytes.saturating_add(metadata.len());
                plan.prune.push(DebrisArtifact {
                    path: path.clone(),
                    bytes: metadata.len(),
                    modified_us: 0,
                    // The move-only API consumes paths and byte counts from
                    // this already-selected plan, not its category. Rotation's
                    // real BackupKind stays with the caller and its report;
                    // never feed this adapter plan to the retention selector.
                    category: DebrisCategory::CorruptQuarantine,
                });
            }
            Ok(_) => preflight.failures.push((
                path.clone(),
                "rotation source is no longer a regular backup file; source not moved".to_string(),
            )),
            Err(error) => preflight.failures.push((path.clone(), error.to_string())),
        }
    }
    if plan.prune.is_empty() {
        return Ok(preflight);
    }
    plan.total_count = plan.prune.len();
    plan.reclaimable_bytes = plan.total_bytes;

    for suffix in 0..MAX_BATCH_DIRECTORY_ATTEMPTS {
        let name = if suffix == 0 {
            stem.to_string()
        } else {
            format!("{stem}-{suffix}")
        };
        let destination = quarantine_parent.join(name);
        match consolidate_debris(&plan, &destination) {
            Ok(mut outcome) => {
                outcome.failures.extend(preflight.failures);
                return Ok(outcome);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "rotation quarantine directory collision budget exhausted under {}; no backup moved",
            quarantine_parent.display()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        tempfile::tempdir().unwrap().keep().canonicalize().unwrap()
    }

    #[test]
    fn empty_rotation_batch_creates_no_quarantine() {
        let root = fixture();
        let parent = root.join("doctor/reclaimable");
        let outcome = stage_backup_paths(&[], &parent, "rotation-test").unwrap();
        assert_eq!(outcome.moved, 0);
        assert!(outcome.failures.is_empty());
        assert!(!root.join("doctor").exists());
    }

    #[test]
    fn rotation_batch_refuses_symlinked_quarantine_ancestor() {
        let root = fixture();
        let outside = fixture();
        let source = root.join("backup");
        fs::write(&source, b"owned backup").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("doctor")).unwrap();
        assert!(stage_backup_paths(
            std::slice::from_ref(&source),
            &root.join("doctor/reclaimable"),
            "rotation-test",
        ).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"owned backup");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(fs::read_link(root.join("doctor")).unwrap(), outside);
    }

    #[test]
    fn rotation_batch_preserves_occupied_and_dangling_directory_names() {
        let root = fixture();
        let parent = root.join("doctor/reclaimable");
        fs::create_dir_all(parent.join("rotation-test")).unwrap();
        fs::write(parent.join("rotation-test/sentinel"), b"earlier evidence").unwrap();
        std::os::unix::fs::symlink("absent", parent.join("rotation-test-1")).unwrap();
        let source = root.join("backup");
        fs::write(&source, b"new backup").unwrap();
        let outcome = stage_backup_paths(std::slice::from_ref(&source), &parent, "rotation-test")
            .unwrap();
        assert_eq!(outcome.moved, 1);
        assert_eq!(outcome.moved_bytes, 10);
        assert!(outcome.failures.is_empty());
        assert_eq!(fs::read(parent.join("rotation-test/sentinel")).unwrap(), b"earlier evidence");
        assert_eq!(fs::read_link(parent.join("rotation-test-1")).unwrap(), Path::new("absent"));
        assert_eq!(fs::read(parent.join("rotation-test-2/backup")).unwrap(), b"new backup");
        assert!(!source.exists());
    }

    #[test]
    fn rotation_batch_collision_exhaustion_leaves_every_source_untouched() {
        let root = fixture();
        let parent = root.join("doctor/reclaimable");
        fs::create_dir_all(&parent).unwrap();
        for suffix in 0..MAX_BATCH_DIRECTORY_ATTEMPTS {
            let name = if suffix == 0 { "rotation-test".to_string() } else { format!("rotation-test-{suffix}") };
            let occupied = parent.join(name);
            fs::create_dir(&occupied).unwrap();
            fs::write(occupied.join("sentinel"), b"retained evidence").unwrap();
        }
        let source = root.join("backup");
        fs::write(&source, b"owned backup").unwrap();
        let error = stage_backup_paths(std::slice::from_ref(&source), &parent, "rotation-test")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("collision budget exhausted"));
        assert_eq!(fs::read(source).unwrap(), b"owned backup");
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 128);
        for entry in fs::read_dir(parent).unwrap() {
            assert_eq!(fs::read(entry.unwrap().path().join("sentinel")).unwrap(), b"retained evidence");
        }
    }

    #[test]
    fn rotation_batch_preserves_invalid_sources_and_stages_independent_files() {
        let root = fixture();
        let good = root.join("backup");
        let directory = root.join("not-a-backup-file");
        let linked = root.join("linked-backup");
        let missing = root.join("missing-backup");
        fs::write(&good, b"owned").unwrap();
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("sentinel"), b"unrelated").unwrap();
        std::os::unix::fs::symlink(&directory, &linked).unwrap();
        let outcome = stage_backup_paths(
            &[good, directory.clone(), linked.clone(), missing],
            &root.join("doctor/reclaimable"),
            "rotation-test",
        ).unwrap();
        assert_eq!(outcome.moved, 1);
        assert_eq!(outcome.moved_bytes, 5);
        assert_eq!(outcome.failures.len(), 3);
        assert_eq!(fs::read(directory.join("sentinel")).unwrap(), b"unrelated");
        assert_eq!(fs::read_link(linked).unwrap(), directory);
        assert_eq!(fs::read(root.join("doctor/reclaimable/rotation-test/backup")).unwrap(), b"owned");
    }

    #[test]
    fn rotation_batch_refuses_a_symlinked_source_parent() {
        let root = fixture();
        let outside = fixture();
        let source = outside.join("backup");
        fs::write(&source, b"outside backup").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("database")).unwrap();
        let aliased = root.join("database/backup");
        let outcome = stage_backup_paths(
            std::slice::from_ref(&aliased),
            &root.join("doctor/reclaimable"),
            "rotation-test",
        ).unwrap();
        assert_eq!(outcome.moved, 0);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].0, aliased);
        assert_eq!(fs::read(source).unwrap(), b"outside backup");
        assert_eq!(fs::read_dir(root.join("doctor/reclaimable/rotation-test")).unwrap().count(), 0);
    }

    #[test]
    fn rotation_batch_creates_private_quarantine_ancestors() {
        use std::os::unix::fs::PermissionsExt;
        let root = fixture();
        let source = root.join("backup");
        fs::write(&source, b"owned").unwrap();
        let outcome = stage_backup_paths(
            &[source],
            &root.join("doctor/reclaimable"),
            "rotation-test",
        ).unwrap();
        assert_eq!(outcome.moved, 1);
        assert!(outcome.failures.is_empty());
        for suffix in ["doctor", "doctor/reclaimable", "doctor/reclaimable/rotation-test"] {
            assert_eq!(fs::metadata(root.join(suffix)).unwrap().permissions().mode() & 0o077, 0);
        }
    }
}
