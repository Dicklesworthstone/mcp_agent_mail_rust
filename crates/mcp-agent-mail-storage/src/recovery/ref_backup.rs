//! Complete ref snapshots and no-clobber backup publication for recovery.
//!
//! Callers retain their repository coordination lock across backup and repair.
//! A snapshot preserves raw symbolic targets (including unborn HEAD), not just
//! their resolved OIDs. Enumeration errors abort before publishing a backup.
//! This is not an atomic snapshot against non-cooperating Git writers; pruning
//! must still revalidate each original target under its Git reference lock.

use std::io::{Read, Write};
use std::path::Path;

use git2::{Reference, Repository};

use super::PrunableRef;
use crate::StorageError;

/// Maximum size of a single recovery backup, including metadata.
const MAX_BACKUP_BYTES: usize = 16 * 1024 * 1024;

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidPath(message.to_string())
}

fn reference_record(reference: &Reference<'_>) -> crate::Result<String> {
    let name = reference.name()?;
    if let Some(target) = reference.target() {
        return Ok(format!("ref  {name}  {target}\n"));
    }
    if let Some(target) = reference.symbolic_target()? {
        return Ok(format!("symref  {name}  {target}\n"));
    }
    Err(invalid("recovery backup reference has no target"))
}

fn append_bounded(text: &mut String, record: &str) -> crate::Result<()> {
    if text.len().saturating_add(record.len()) > MAX_BACKUP_BYTES {
        return Err(invalid("recovery ref backup exceeds its byte budget"));
    }
    text.push_str(record);
    Ok(())
}

/// Write a complete ref snapshot before an authorized repair.
///
/// Existing `ref` and `orphan` record formats are retained; `symref` records
/// preserve aliases and HEAD without resolving or losing their original names.
/// A direct ref to a missing object is still backed up exactly. The snapshot
/// does not need a healthy object database, but every enumerated ref must be
/// readable and representable without lossy UTF-8 conversion.
///
/// # Errors
///
/// Returns an error on incomplete enumeration, missing/unreadable HEAD, byte
/// limits, symlinked backup authority, existing destinations, or failed writes
/// and flushes. The caller must not mutate refs after any backup error.
pub fn write_snapshot(
    repo_path: &Path,
    destination: &Path,
    findings: &[PrunableRef],
) -> crate::Result<()> {
    let repo = Repository::open(repo_path)?;
    let mut text = String::new();
    append_bounded(&mut text, "# agent-mail ref backup v1\n")?;
    append_bounded(&mut text, &format!("# repo: {}\n", repo_path.display()))?;
    append_bounded(&mut text, "# ALL refs at backup time (including HEAD):\n")?;

    // `references()` does not normally include HEAD. Read the raw ref so an
    // unborn branch is retained rather than discarded as a resolution error.
    append_bounded(&mut text, &reference_record(&repo.find_reference("HEAD")?)?)?;
    for reference in repo.references()? {
        let reference = reference?;
        if reference.name()? != "HEAD" {
            append_bounded(&mut text, &reference_record(&reference)?)?;
        }
    }
    append_bounded(&mut text, "# ORPHAN findings:\n")?;
    for finding in findings {
        append_bounded(
            &mut text,
            &format!(
                "orphan  {}  {}  {:?}  {}\n",
                finding.ref_name, finding.target_sha, finding.category, finding.reason,
            ),
        )?;
    }
    append_bounded(&mut text, "# END agent-mail ref backup\n")?;
    publish(destination, text.as_bytes())
}

/// Back up exact bytes of a regular file, such as packed-refs, without clobbering.
///
/// # Errors
///
/// Refuses symlinks, nonregular sources, excessive/growing input, existing
/// destinations, and read/write/flush failures. A missing source is an error;
/// callers distinguish absence from other metadata failures before calling.
pub fn copy_file(source: &Path, destination: &Path) -> crate::Result<()> {
    if crate::path_existing_prefix_has_symlink(source)? {
        return Err(invalid("recovery backup source has a symlinked authority"));
    }
    let file = mcp_agent_mail_core::disk::open_regular_file_no_follow(source)?;
    if file.metadata()?.len() > MAX_BACKUP_BYTES as u64 {
        return Err(invalid("recovery backup source exceeds its byte budget"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BACKUP_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    publish(destination, &bytes)
}

/// Publish fully written bytes. Unix flushes the containing directory and
/// newly created directory ancestry; other platforms flush file data without
/// claiming Unix directory-fsync guarantees.
fn publish(destination: &Path, bytes: &[u8]) -> crate::Result<()> {
    if bytes.len() > MAX_BACKUP_BYTES {
        return Err(invalid("recovery backup exceeds its byte budget"));
    }
    if destination.file_name().is_none() {
        return Err(invalid("recovery backup destination has no filename"));
    }
    if crate::path_existing_prefix_has_symlink(destination)? {
        return Err(invalid("recovery backup destination has a symlinked authority"));
    }
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    // Record the missing directory ancestry before creating it. Flushing only
    // the leaf directory would leave newly created parents uncommitted on Unix.
    #[cfg(unix)]
    let mut sync_dirs = Vec::new();
    let mut ancestor = parent.to_path_buf();
    loop {
        #[cfg(unix)]
        sync_dirs.push(ancestor.clone());
        match std::fs::symlink_metadata(&ancestor) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => return Err(invalid("recovery backup parent is not a regular directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
            }
            Err(error) => return Err(error.into()),
        }
    }
    std::fs::create_dir_all(parent)?;
    if crate::path_existing_prefix_has_symlink(destination)? {
        return Err(invalid("recovery backup authority changed before publication"));
    }

    let mut staged = tempfile::Builder::new()
        .prefix(".ref-backup-")
        .tempfile_in(parent)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(destination) {
        Ok(file) => {
            file.sync_all()?;
            #[cfg(unix)]
            for directory in sync_dirs {
                std::fs::File::open(directory)?.sync_all()?;
            }
            Ok(())
        }
        Err(error) => {
            let kind = error.error.kind();
            let (file, path) = error.file.keep().map_err(|error| error.error)?;
            drop(file);
            Err(std::io::Error::new(
                kind,
                format!("backup publication refused; candidate retained at {}", path.display()),
            )
            .into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture(temp: &TempDir) -> (Repository, PrunableRef) {
        let repo = Repository::init(temp.path().join("repo")).unwrap();
        let path = repo.path().join("refs/temp/orphan");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string();
        std::fs::write(path, format!("{target_sha}\n")).unwrap();
        let finding = PrunableRef {
            ref_name: "refs/temp/orphan".to_string(),
            target_sha,
            reason: "missing object".to_string(),
            category: super::super::RefCategory::SafeToPrune,
        };
        (repo, finding)
    }

    #[test]
    fn snapshot_preserves_unborn_head_missing_oids_and_raw_symbolic_targets() {
        let temp = TempDir::new().unwrap();
        let (repo, finding) = fixture(&temp);
        std::fs::write(
            repo.path().join("refs/temp/alias"),
            b"ref: refs/heads/not-created\n",
        )
        .unwrap();
        let destination = temp.path().join("backups/nested/refs.txt");
        write_snapshot(repo.workdir().unwrap(), &destination, &[finding]).unwrap();
        let text = std::fs::read_to_string(destination).unwrap();
        assert!(text.contains("symref  HEAD  refs/heads/"));
        assert!(text.contains("symref  refs/temp/alias  refs/heads/not-created\n"));
        assert!(text.contains("ref  refs/temp/orphan  deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n"));
        assert!(text.contains("orphan  refs/temp/orphan"));
        assert!(text.ends_with("# END agent-mail ref backup\n"));
        assert!(repo.find_reference("refs/temp/orphan").is_ok());
    }

    #[test]
    fn snapshot_failure_does_not_publish_or_create_backup_directories() {
        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("backups/refs.txt");
        assert!(write_snapshot(&temp.path().join("missing-repo"), &destination, &[]).is_err());
        assert!(!temp.path().join("backups").exists());
    }

    #[test]
    fn snapshot_never_replaces_existing_backup_evidence() {
        let temp = TempDir::new().unwrap();
        let (repo, finding) = fixture(&temp);
        let destination = temp.path().join("refs.txt");
        std::fs::write(&destination, b"previous evidence").unwrap();
        assert!(write_snapshot(repo.workdir().unwrap(), &destination, &[finding]).is_err());
        assert_eq!(std::fs::read(destination).unwrap(), b"previous evidence");
    }

    #[test]
    fn concurrent_publication_has_exactly_one_winner() {
        let temp = TempDir::new().unwrap();
        let (repo, _) = fixture(&temp);
        let source = repo.workdir().unwrap().to_path_buf();
        let destination = temp.path().join("refs.txt");
        let barrier = std::sync::Barrier::new(2);
        let outcomes = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                write_snapshot(&source, &destination, &[]).is_ok()
            });
            let second = scope.spawn(|| {
                barrier.wait();
                write_snapshot(&source, &destination, &[]).is_ok()
            });
            (first.join().unwrap(), second.join().unwrap())
        });
        assert_ne!(outcomes.0, outcomes.1);
        assert!(
            std::fs::read_to_string(destination)
                .unwrap()
                .ends_with("# END agent-mail ref backup\n")
        );
    }

    #[test]
    fn packed_ref_copy_preserves_bytes_and_refuses_overwrite() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("packed-refs");
        let destination = temp.path().join("backups/packed.txt");
        let bytes = b"# pack-refs with: peeled fully-peeled sorted\n0123456789012345678901234567890123456789 refs/tags/test\n";
        std::fs::write(&source, bytes).unwrap();
        copy_file(&source, &destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
        assert!(copy_file(&source, &destination).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
        assert_eq!(std::fs::read(source).unwrap(), bytes);
    }

    #[test]
    fn excessive_or_nonregular_backup_input_is_refused() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("oversized");
        let file = std::fs::File::create(&source).unwrap();
        file.set_len(MAX_BACKUP_BYTES as u64 + 1).unwrap();
        let destination = temp.path().join("backups/ref.txt");
        assert!(copy_file(&source, &destination).is_err());
        assert!(copy_file(temp.path(), &destination).is_err());
        assert!(!temp.path().join("backups").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_backup_parent_is_refused_without_touching_outside_data() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let (repo, finding) = fixture(&temp);
        symlink(outside.path(), temp.path().join("backups")).unwrap();
        let destination = temp.path().join("backups/nested/refs.txt");
        assert!(write_snapshot(repo.workdir().unwrap(), &destination, &[finding]).is_err());
        assert!(!outside.path().join("nested").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_packed_ref_source_is_not_followed() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let actual = temp.path().join("actual");
        std::fs::write(&actual, b"original").unwrap();
        let source = temp.path().join("packed-refs");
        symlink(&actual, &source).unwrap();
        let destination = temp.path().join("backup.txt");
        assert!(copy_file(&source, &destination).is_err());
        assert!(!destination.exists());
        assert_eq!(std::fs::read(actual).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_ref_name_snapshot_is_never_published() {
        use std::os::unix::ffi::OsStringExt;
        let temp = TempDir::new().unwrap();
        let (repo, finding) = fixture(&temp);
        let name = std::ffi::OsString::from_vec(b"bad-\xff".to_vec());
        std::fs::write(
            repo.path().join("refs/temp").join(name),
            format!("{}\n", finding.target_sha),
        )
        .unwrap();
        let destination = temp.path().join("backups/refs.txt");
        assert!(write_snapshot(repo.workdir().unwrap(), &destination, &[finding]).is_err());
        assert!(!temp.path().join("backups").exists());
    }
}
