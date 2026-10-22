//! Admission for automatic backup staging (GH#326).
//!
//! Retry timing is process-local; retained staging artifacts are not. Inspect
//! the actual filesystem under a nonblocking, cross-process lease before each
//! automatic export. Both proactive and verified backups use this boundary.
//! Nothing here removes, relocates, opens with SQLite, or repairs old evidence.
//!
//! Staging names currently contain no mailbox identifier, so the budget and
//! lease deliberately cover the entire database parent, including other
//! mailboxes sharing it. This is admission control, not a filesystem quota:
//! estimates cannot constrain a single export's actual expansion, explicit
//! operator calls, older binaries, or unrelated processes that ignore the lease.

#[cfg(windows)]
use std::fs::Metadata;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

use mcp_agent_mail_db::recovery_retention::staging::{
    STAGING_REMEDIATION, assess_automatic_backup_staging,
    inspect_staging_for_admission as inspect_staging, regular_file_bytes,
};

/// Keeping this value alive serializes automatic exporters sharing a parent.
/// Dropping it releases the OS lease, including unwinding and process exit;
/// there is no stale PID/timestamp record to clear after a crash.
struct Admission {
    _lease: File,
}

/// Run a producer only after successful admission. The lease spans the entire
/// producer, including its validation, publication, and failed-stage retention.
/// Refusal is an incomplete backup attempt, never a clean integrity verdict.
pub(super) fn with_admission<T>(
    primary: &Path,
    produce: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    if primary.as_os_str() == ":memory:" {
        return produce();
    }
    let _admission = Admission::acquire(primary).map_err(|error| {
        format!(
            "automatic backup staging admission refused for {}: {error}; no export was started and existing evidence is unchanged",
            primary.display()
        )
    })?;
    produce()
}

impl Admission {
    fn acquire(primary: &Path) -> io::Result<Self> {
        // Freeze relative spelling once, without changing the caller's CWD.
        let primary = if primary.is_absolute() {
            primary.to_path_buf()
        } else {
            std::env::current_dir()?.join(primary)
        };
        let parent = primary
            .parent()
            .ok_or_else(|| invalid("database has no parent"))?;
        // Do not create a control file for an absent/non-regular primary.
        regular_file_bytes(&primary, false)?;
        let lease = acquire_parent_lease(parent)?;
        // Health and this producer share both the inventory rules and the
        // count/projected-byte policy. Only this producer holds admission.
        let assessment = assess_automatic_backup_staging(&primary, inspect_staging(parent)?)?;
        assessment.check_budget().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{}: {error}; {STAGING_REMEDIATION}", parent.display()),
            )
        })?;
        validate_parent_lease(parent, &lease)?;
        Ok(Self { _lease: lease })
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn try_lock(file: &File) -> io::Result<()> {
    fs2::FileExt::try_lock_exclusive(file).map_err(|error| {
        let contended = fs2::lock_contended_error().raw_os_error();
        if error.kind() == io::ErrorKind::WouldBlock
            || (contended.is_some() && error.raw_os_error() == contended)
        {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "another automatic exporter holds this parent's staging lease",
            )
        } else {
            error
        }
    })
}

#[cfg(unix)]
fn acquire_parent_lease(parent: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(
            nix::fcntl::OFlag::O_DIRECTORY.bits()
                | nix::fcntl::OFlag::O_NOFOLLOW.bits()
                | nix::fcntl::OFlag::O_NONBLOCK.bits(),
        )
        .open(parent)?;
    // flock on the directory inode creates no control file and naturally
    // serializes different mailbox names and aliases of the same parent.
    try_lock(&file)?;
    validate_parent_lease(parent, &file)?;
    Ok(file)
}

#[cfg(unix)]
fn validate_parent_lease(parent: &Path, retained: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let current = fs::symlink_metadata(parent)?;
    let opened = retained.metadata()?;
    if !current.file_type().is_dir()
        || current.dev() != opened.dev()
        || current.ino() != opened.ino()
    {
        return Err(invalid(
            "backup parent changed while acquiring staging admission",
        ));
    }
    Ok(())
}

#[cfg(windows)]
const WINDOWS_LEASE_NAME: &str = ".mcp-agent-mail-backup-admission.lock";

#[cfg(windows)]
fn acquire_parent_lease(parent: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    // Directories cannot use the Unix flock protocol on Windows. Open a
    // persistent empty control file without replacement/truncation, without
    // following a reparse point, and without FILE_SHARE_DELETE. Never remove it.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ_WRITE: u32 = 0x0000_0003;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ_WRITE)
        .open(parent.join(WINDOWS_LEASE_NAME))?;
    validate_windows_lease_file(&file.metadata()?)?;
    if mcp_agent_mail_core::disk::windows_file_link_count(&file)? != 1 {
        return Err(invalid("backup admission control file must have one link"));
    }
    try_lock(&file)?;
    validate_parent_lease(parent, &file)?;
    Ok(file)
}

#[cfg(windows)]
fn validate_windows_lease_file(metadata: &Metadata) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    if !metadata.file_type().is_file()
        || metadata.len() != 0
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(invalid(
            "backup admission control path is occupied by non-control data",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_parent_lease(parent: &Path, retained: &File) -> io::Result<()> {
    validate_windows_lease_file(&retained.metadata()?)?;
    validate_windows_lease_file(&fs::symlink_metadata(parent.join(WINDOWS_LEASE_NAME))?)
}

#[cfg(not(any(unix, windows)))]
fn acquire_parent_lease(_parent: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cross-process backup staging admission is unsupported",
    ))
}

#[cfg(not(any(unix, windows)))]
fn validate_parent_lease(_parent: &Path, _retained: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cross-process backup staging admission is unsupported",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_db::recovery_retention::staging::{
        AutomaticBackupStagingAssessment, MAX_RETAINED_STAGES,
        ProactiveBackupStagingInventory as Inventory, STAGING_BYTES_FLOOR,
    };
    use std::cell::Cell;
    use std::path::PathBuf;

    fn check_budget(
        inventory: &Inventory,
        projected_bytes: u64,
        byte_limit: u64,
    ) -> io::Result<()> {
        AutomaticBackupStagingAssessment {
            inventory: *inventory,
            projected_bytes,
            byte_limit,
        }
        .check_budget()
    }

    fn fixture() -> PathBuf {
        tempfile::tempdir()
            .expect("retained backup-admission fixture")
            .keep()
            .canonicalize()
            .unwrap()
    }

    fn primary(root: &Path) -> PathBuf {
        let primary = root.join("mail.sqlite3");
        fs::write(&primary, b"mailbox fixture").unwrap();
        primary
    }

    fn stage(root: &Path, ordinal: usize, bytes: &[u8]) -> PathBuf {
        let path = root.join(format!(".mcp-agent-mail-proactive-backup-{ordinal:06}"));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("live-export.sqlite3"), bytes).unwrap();
        fs::write(path.join("snapshot.sqlite3"), bytes).unwrap();
        path
    }

    #[test]
    fn budget_counts_the_next_attempt_and_rejects_overflow() {
        let inventory = Inventory {
            directories: 2,
            legacy_files: 0,
            bytes: 60,
        };
        assert!(check_budget(&inventory, 40, 100).is_ok());
        assert!(check_budget(&inventory, 41, 100).is_err());
        assert!(
            check_budget(
                &Inventory {
                    directories: 3,
                    legacy_files: 0,
                    bytes: 0
                },
                0,
                u64::MAX
            )
            .is_err()
        );
        assert!(
            check_budget(
                &Inventory {
                    directories: 0,
                    legacy_files: 0,
                    bytes: u64::MAX
                },
                1,
                u64::MAX
            )
            .is_err()
        );
    }

    #[test]
    fn inventory_counts_nested_copies_without_touching_live_or_backup_files() {
        let root = fixture();
        let primary = primary(&root);
        let backup = root.join("mail.sqlite3.bak");
        fs::write(&backup, b"verified backup").unwrap();
        let kept = stage(&root, 0, b"12345");
        fs::create_dir(kept.join("nested")).unwrap();
        fs::write(kept.join("nested/sidecar"), b"123").unwrap();
        assert_eq!(
            inspect_staging(&root).unwrap(),
            Inventory {
                directories: 1,
                legacy_files: 0,
                bytes: 13
            }
        );
        assert_eq!(fs::read(primary).unwrap(), b"mailbox fixture");
        assert_eq!(fs::read(backup).unwrap(), b"verified backup");
    }

    #[test]
    fn restarting_admission_cannot_repeat_failed_exports_forever() {
        let root = fixture();
        let primary = primary(&root);
        let calls = Cell::new(0);
        for _ in 0..20 {
            let _ = with_admission(&primary, || -> Result<(), String> {
                let ordinal = calls.get();
                stage(&root, ordinal, b"preserved failed export");
                calls.set(ordinal + 1);
                Err("strict canonical validation refused the candidate".into())
            });
        }
        assert_eq!(
            calls.get(),
            MAX_RETAINED_STAGES,
            "fresh process-local state cannot bypass retained inventory"
        );
        assert_eq!(
            inspect_staging(&root).unwrap().artifacts(),
            MAX_RETAINED_STAGES
        );
        for ordinal in 0..MAX_RETAINED_STAGES {
            let path = root.join(format!(
                ".mcp-agent-mail-proactive-backup-{ordinal:06}/snapshot.sqlite3"
            ));
            assert_eq!(fs::read(path).unwrap(), b"preserved failed export");
        }
    }

    #[test]
    fn legacy_inspection_images_share_the_stage_count_and_byte_budget() {
        let root = fixture();
        let primary = primary(&root);
        let current = stage(&root, 0, b"current");
        let legacy = root.join(".snapshot-inspect-legacy.sqlite3");
        fs::write(&legacy, b"legacy").unwrap();
        assert_eq!(
            inspect_staging(&root).unwrap(),
            Inventory {
                directories: 1,
                legacy_files: 1,
                bytes: 20,
            }
        );
        assert_eq!(with_admission(&primary, || Ok(17)).unwrap(), 17);

        let interrupted = root.join(".snapshot-inspect-interrupted.sqlite3");
        fs::write(&interrupted, b"partial inspection").unwrap();
        let error = with_admission(&primary, || -> Result<(), String> {
            panic!("mixed retained stages must refuse before the producer")
        })
        .unwrap_err();
        assert!(error.contains("3 retained staging artifacts"));
        assert!(error.contains(".snapshot-inspect-"));
        assert_eq!(fs::read(legacy).unwrap(), b"legacy");
        assert_eq!(fs::read(interrupted).unwrap(), b"partial inspection");
        assert_eq!(
            fs::read(current.join("snapshot.sqlite3")).unwrap(),
            b"current"
        );
        assert_eq!(fs::read(primary).unwrap(), b"mailbox fixture");
    }

    #[test]
    fn legacy_inspection_bytes_refuse_before_a_new_export() {
        let root = fixture();
        let primary = primary(&root);
        let legacy = root.join(".snapshot-inspect-large.sqlite3");
        fs::write(&legacy, b"partial database image").unwrap();
        OpenOptions::new()
            .write(true)
            .open(&legacy)
            .unwrap()
            .set_len(STAGING_BYTES_FLOOR)
            .unwrap();
        let error = with_admission(&primary, || -> Result<(), String> {
            panic!("legacy bytes must spend the automatic staging budget")
        })
        .unwrap_err();
        assert!(error.contains("retained bytes"));
        assert!(error.contains("exceed"));
        assert_eq!(fs::metadata(legacy).unwrap().len(), STAGING_BYTES_FLOOR);
        assert_eq!(fs::read_dir(root).unwrap().count(), 2);
    }

    #[test]
    fn repeated_interrupted_legacy_inspections_cannot_bypass_admission() {
        let root = fixture();
        let primary = primary(&root);
        let calls = Cell::new(0);
        for _ in 0..20 {
            let _ = with_admission(&primary, || -> Result<(), String> {
                let ordinal = calls.get();
                fs::write(
                    root.join(format!(".snapshot-inspect-{ordinal:06}.sqlite3")),
                    b"unverified interrupted inspection",
                )
                .unwrap();
                calls.set(ordinal + 1);
                Err("metadata inspection was interrupted".into())
            });
        }
        assert_eq!(calls.get(), MAX_RETAINED_STAGES);
        for ordinal in 0..MAX_RETAINED_STAGES {
            assert_eq!(
                fs::read(root.join(format!(".snapshot-inspect-{ordinal:06}.sqlite3"))).unwrap(),
                b"unverified interrupted inspection"
            );
        }
    }

    #[test]
    fn malformed_legacy_inspection_entries_refuse_without_mutation() {
        for name in [".snapshot-inspect-.sqlite3", ".snapshot-inspect-unknown"] {
            let root = fixture();
            let primary = primary(&root);
            let evidence = root.join(name);
            fs::write(&evidence, b"unclassified evidence").unwrap();
            let error = with_admission(&primary, || -> Result<(), String> {
                panic!("unclassified inspection evidence must refuse admission")
            })
            .unwrap_err();
            assert!(error.contains("footprint is unavailable"));
            assert_eq!(fs::read(evidence).unwrap(), b"unclassified evidence");
        }
        let root = fixture();
        let primary = primary(&root);
        let unexpected = root.join(".snapshot-inspect-directory.sqlite3");
        fs::create_dir(&unexpected).unwrap();
        fs::write(unexpected.join("evidence"), b"unknown directory").unwrap();
        assert!(with_admission(&primary, || Ok(())).is_err());
        assert_eq!(
            fs::read(unexpected.join("evidence")).unwrap(),
            b"unknown directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_inspection_symlinks_are_preserved_and_never_followed() {
        let root = fixture();
        let outside = fixture();
        let primary = primary(&root);
        let evidence = outside.join("database.sqlite3");
        fs::write(&evidence, b"external database").unwrap();
        let alias = root.join(".snapshot-inspect-link.sqlite3");
        std::os::unix::fs::symlink(&evidence, &alias).unwrap();
        let error = with_admission(&primary, || -> Result<(), String> {
            panic!("legacy aliases must refuse admission")
        })
        .unwrap_err();
        assert!(error.contains("not a recognized regular staging file"));
        assert!(
            fs::symlink_metadata(alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(evidence).unwrap(), b"external database");
    }

    #[test]
    fn shared_parent_mailboxes_share_one_nonblocking_lease() {
        let root = fixture();
        let first = primary(&root);
        let second = root.join("other.sqlite3");
        fs::write(&second, b"other mailbox").unwrap();
        let held = Admission::acquire(&first).unwrap();
        let refused = Admission::acquire(&second)
            .err()
            .expect("another exporter must not enter");
        assert_eq!(refused.kind(), io::ErrorKind::WouldBlock);
        drop(held);
        let again = Admission::acquire(&second).unwrap();
        drop(again);
        assert_eq!(fs::read(first).unwrap(), b"mailbox fixture");
        assert_eq!(fs::read(second).unwrap(), b"other mailbox");
    }

    #[test]
    fn independent_database_parents_do_not_block_each_other() {
        let first_root = fixture();
        let second_root = fixture();
        let first = Admission::acquire(&primary(&first_root)).unwrap();
        let second = Admission::acquire(&primary(&second_root)).unwrap();
        drop((first, second));
    }

    #[test]
    fn retained_byte_budget_refuses_before_the_producer_runs() {
        let root = fixture();
        let primary = primary(&root);
        let kept = stage(&root, 0, b"preserved");
        // Sparse apparent bytes exercise the real metadata path without
        // allocating half a gigabyte in a unit test.
        OpenOptions::new()
            .write(true)
            .open(kept.join("snapshot.sqlite3"))
            .unwrap()
            .set_len(STAGING_BYTES_FLOOR)
            .unwrap();
        let error = with_admission(&primary, || -> Result<(), String> {
            panic!("a full staging budget must not call the producer")
        })
        .unwrap_err();
        assert!(error.contains("exceed"));
        assert_eq!(
            fs::metadata(kept.join("snapshot.sqlite3")).unwrap().len(),
            STAGING_BYTES_FLOOR
        );
        assert_eq!(
            fs::read(kept.join("live-export.sqlite3")).unwrap(),
            b"preserved"
        );
    }

    #[test]
    fn unavailable_primary_or_wal_never_means_an_empty_generation() {
        let root = fixture();
        let missing = root.join("missing.sqlite3");
        assert!(with_admission(&missing, || Ok(())).is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        let primary = primary(&root);
        fs::create_dir(root.join("mail.sqlite3-wal")).unwrap();
        assert!(with_admission(&primary, || Ok(())).is_err());
        assert_eq!(fs::read(primary).unwrap(), b"mailbox fixture");
    }

    #[test]
    fn saturated_inventory_does_not_descend_into_the_retained_trees() {
        let root = fixture();
        for ordinal in 0..MAX_RETAINED_STAGES {
            let kept = stage(&root, ordinal, b"evidence");
            fs::create_dir(kept.join("nested")).unwrap();
        }
        let inventory = inspect_staging(&root).unwrap();
        assert_eq!(inventory.artifacts(), MAX_RETAINED_STAGES);
        assert_eq!(
            inventory.bytes, 0,
            "the saturated count is sufficient to refuse"
        );
        assert!(check_budget(&inventory, 0, u64::MAX).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn staging_symlinks_are_refused_without_traversing_their_targets() {
        let root = fixture();
        let outside = fixture();
        let primary = primary(&root);
        fs::write(outside.join("sentinel"), b"outside evidence").unwrap();
        let kept = stage(&root, 0, b"preserved");
        std::os::unix::fs::symlink(&outside, kept.join("linked")).unwrap();
        let error = with_admission(&primary, || Ok(())).unwrap_err();
        assert!(error.contains("symlink or special file"));
        assert_eq!(
            fs::read(outside.join("sentinel")).unwrap(),
            b"outside evidence"
        );
    }

    #[test]
    fn lease_is_released_when_the_producer_panics() {
        let root = fixture();
        let primary = primary(&root);
        let result = std::panic::catch_unwind(|| {
            let _ = with_admission(&primary, || -> Result<(), String> {
                panic!("producer panic")
            });
        });
        assert!(result.is_err());
        assert!(with_admission(&primary, || Ok(())).is_ok());
    }

    #[test]
    fn memory_backup_never_inspects_a_filesystem_parent() {
        assert_eq!(
            with_admission(Path::new(":memory:"), || Ok(17)).unwrap(),
            17
        );
    }

    #[test]
    fn explicit_evidence_relocation_is_seen_without_cached_or_pid_state() {
        let root = fixture();
        let primary = primary(&root);
        let mut retained = Vec::new();
        for ordinal in 0..MAX_RETAINED_STAGES {
            retained.push(stage(&root, ordinal, b"evidence"));
        }
        assert!(with_admission(&primary, || Ok(())).is_err());
        // Model an explicit operator action, not automatic deletion/reclaim.
        let archived = root.join("operator-retained-evidence");
        fs::rename(&retained[0], &archived).unwrap();
        assert!(with_admission(&primary, || Ok(())).is_ok());
        assert_eq!(
            fs::read(archived.join("snapshot.sqlite3")).unwrap(),
            b"evidence"
        );
    }

    #[test]
    fn staging_lookalike_files_are_not_empty_evidence() {
        let root = fixture();
        let primary = primary(&root);
        let unexpected = root.join(".mcp-agent-mail-proactive-backup-unexpected");
        fs::write(&unexpected, b"unclassified evidence").unwrap();
        assert!(with_admission(&primary, || Ok(())).is_err());
        assert_eq!(fs::read(unexpected).unwrap(), b"unclassified evidence");
        // The shared staging module tests the real bounded directory walk,
        // including parent and nested entries, rather than a counter alone.
    }

    #[cfg(unix)]
    #[test]
    fn lock_is_shared_with_an_independent_process() {
        const CHILD_PATH: &str = "AM_BACKUP_ADMISSION_TEST_CHILD_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let refused = Admission::acquire(Path::new(&path))
                .err()
                .expect("parent holds the OS lease");
            assert_eq!(refused.kind(), io::ErrorKind::WouldBlock);
            return;
        }
        let root = fixture();
        let primary = primary(&root);
        let _held = Admission::acquire(&primary).unwrap();
        // module_path! includes the crate prefix; libtest names do not. Check
        // the result count too, so a mistaken filter cannot pass with zero tests.
        let module = module_path!().split_once("::").unwrap().1;
        let test_name = format!("{module}::lock_is_shared_with_an_independent_process");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture"])
            .env(CHILD_PATH, &primary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
