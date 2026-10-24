//! Read-only staging inventory and the automatic backup producer's budget.
//!
//! Health observes the complete bounded footprint. Admission may stop once the
//! retained-artifact count already refuses an export, but uses the same names,
//! file rules, and budget. Neither observation establishes ownership of these
//! parent-wide artifacts, excludes a concurrent exporter, or permits reclaim.

use std::fs;
use std::io;
use std::path::Path;

pub(crate) const PROACTIVE_STAGING_PREFIX: &[u8] = b".mcp-agent-mail-proactive-backup-";
const LEGACY_INSPECTION_PREFIX: &[u8] = b".snapshot-inspect-";
const LEGACY_INSPECTION_SUFFIX: &[u8] = b".sqlite3";
pub(crate) const MAX_STAGING_INVENTORY_ENTRIES: usize = 16_384;

/// Three retained failed exports exhaust automatic staging admission.
pub const MAX_RETAINED_STAGES: usize = 3;
/// The staging byte budget is at least 512 MiB, or five live generations.
pub const STAGING_BYTES_FLOOR: u64 = 512 * 1024 * 1024;
/// Anonymous stages need operator inspection; ordinary debris reclaim cannot
/// establish that a stage belongs to an inactive exporter.
pub const STAGING_REMEDIATION: &str = "inspect retained .mcp-agent-mail-proactive-backup-* directories and .snapshot-inspect-*.sqlite3 files in the database parent; operator-supervised relocation requires quiescing all exporters sharing that parent; am doctor reclaim does not move these stages";

/// Parent-wide modern staging directories and legacy inspection images.
///
/// The names do not identify a mailbox. This includes active and failed stages;
/// neither a name nor an inventory grants authority to move or delete one.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProactiveBackupStagingInventory {
    pub directories: usize,
    pub legacy_files: usize,
    /// Logical regular-file lengths, including sparse files. No SQLite opens.
    pub bytes: u64,
}

impl ProactiveBackupStagingInventory {
    #[must_use]
    pub const fn artifacts(&self) -> usize {
        self.directories.saturating_add(self.legacy_files)
    }
}

/// One metadata-only observation of the automatic export's staging budget.
///
/// A successful budget check is not publication admission: the producer must
/// still retain its cross-process parent lease and publication authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutomaticBackupStagingAssessment {
    pub inventory: ProactiveBackupStagingInventory,
    pub projected_bytes: u64,
    pub byte_limit: u64,
}

impl AutomaticBackupStagingAssessment {
    /// Apply the producer's count and projected-byte limits without mutation.
    ///
    /// # Errors
    /// Returns a refusal for a saturated count, exceeded byte budget, or overflow.
    pub fn check_budget(&self) -> io::Result<()> {
        let artifacts = self.inventory.artifacts();
        if artifacts >= MAX_RETAINED_STAGES {
            return Err(invalid(format!(
                "{artifacts} retained staging artifacts reached the automatic limit of {MAX_RETAINED_STAGES}"
            )));
        }
        let projected_total = self
            .inventory
            .bytes
            .checked_add(self.projected_bytes)
            .ok_or_else(|| invalid("retained plus projected staging size overflow"))?;
        if projected_total > self.byte_limit {
            return Err(invalid(format!(
                "{} retained bytes plus {} projected bytes exceed the {}-byte staging budget",
                self.inventory.bytes, self.projected_bytes, self.byte_limit
            )));
        }
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Inspect a regular file's logical length without following its leaf link.
///
/// # Errors
/// Refuses unreadable or non-regular entries, including symlinks. A missing
/// optional file is empty only when `missing_is_empty` explicitly permits it.
pub fn regular_file_bytes(path: &Path, missing_is_empty: bool) -> io::Result<u64> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(metadata.len()),
        Ok(_) => Err(invalid(format!("{} is not a regular file", path.display()))),
        Err(error) if missing_is_empty && error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

/// Combine an observed inventory with the exact producer projection.
///
/// Main plus WAL is one generation; the private export and canonical snapshot
/// coexist, requiring two generations. Read failures and overflow are unknown
/// admission, never a zero-byte healthy mailbox.
///
/// # Errors
/// Refuses unavailable primary/WAL metadata or an overflowing size projection.
pub fn assess_automatic_backup_staging(
    primary: &Path,
    inventory: ProactiveBackupStagingInventory,
) -> io::Result<AutomaticBackupStagingAssessment> {
    let main_bytes = regular_file_bytes(primary, false)?;
    let mut wal_name = primary
        .file_name()
        .ok_or_else(|| invalid("database has no name"))?
        .to_os_string();
    wal_name.push("-wal");
    let wal_bytes = regular_file_bytes(&primary.with_file_name(wal_name), true)?;
    assessment_for_sizes(inventory, main_bytes, wal_bytes)
}

fn assessment_for_sizes(
    inventory: ProactiveBackupStagingInventory,
    main_bytes: u64,
    wal_bytes: u64,
) -> io::Result<AutomaticBackupStagingAssessment> {
    let generation_bytes = main_bytes
        .checked_add(wal_bytes)
        .ok_or_else(|| invalid("database/WAL size overflow"))?;
    let projected_bytes = generation_bytes
        .checked_mul(2)
        .ok_or_else(|| invalid("backup staging estimate overflow"))?;
    let byte_limit = generation_bytes
        .checked_mul(5)
        .ok_or_else(|| invalid("backup staging budget overflow"))?
        .max(STAGING_BYTES_FLOOR);
    Ok(AutomaticBackupStagingAssessment {
        inventory,
        projected_bytes,
        byte_limit,
    })
}

/// Complete bounded health inventory beside the configured database.
///
/// Missing parents are empty during first-run bootstrap. Symlinks, special
/// files, unreadable entries, and exhausted scan budgets are unavailable
/// evidence. This live metadata observation is not a transactional snapshot
/// or a hostile-user sandbox; it never deliberately descends through a link.
///
/// # Errors
/// Refuses unavailable, unsafe, oversized, or incomplete staging inventories.
pub fn inspect_proactive_backup_staging(
    database_path: &Path,
) -> io::Result<ProactiveBackupStagingInventory> {
    inspect_proactive_backup_staging_with_limit(database_path, MAX_STAGING_INVENTORY_ENTRIES)
}

pub(crate) fn inspect_proactive_backup_staging_with_limit(
    database_path: &Path,
    max_entries: usize,
) -> io::Result<ProactiveBackupStagingInventory> {
    if database_path.file_name().is_none() {
        return Ok(ProactiveBackupStagingInventory::default());
    }
    let primary = if database_path.is_absolute() {
        database_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(database_path)
    };
    let parent = primary
        .parent()
        .ok_or_else(|| invalid("staging inventory database has no parent"))?;
    inspect_staging(parent, max_entries, false, true)
}

/// Producer inventory under its retained parent lease. A saturated count is
/// already a refusal, so do not traverse retained trees merely for a byte sum.
///
/// # Errors
/// Refuses unavailable or unsafe entries and an exhausted inventory budget.
pub fn inspect_staging_for_admission(parent: &Path) -> io::Result<ProactiveBackupStagingInventory> {
    inspect_staging(parent, MAX_STAGING_INVENTORY_ENTRIES, true, false)
}

fn inspect_staging(
    parent: &Path,
    max_entries: usize,
    stop_at_count_limit: bool,
    missing_parent_is_empty: bool,
) -> io::Result<ProactiveBackupStagingInventory> {
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(invalid(
                "staging inventory database parent is not a real directory",
            ));
        }
        Err(error) if missing_parent_is_empty && error.kind() == io::ErrorKind::NotFound => {
            return Ok(ProactiveBackupStagingInventory::default());
        }
        Err(error) => return Err(error),
    }
    let mut remaining = max_entries;
    let mut visit = || -> io::Result<()> {
        remaining = remaining.checked_sub(1).ok_or_else(|| {
            invalid("staging inventory entry limit exceeded; footprint is unavailable")
        })?;
        Ok(())
    };
    let mut inventory = ProactiveBackupStagingInventory::default();
    let mut pending = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        visit()?;
        let name = entry.file_name();
        let name = name.as_encoded_bytes();
        let legacy = name.starts_with(LEGACY_INSPECTION_PREFIX);
        if !name.starts_with(PROACTIVE_STAGING_PREFIX) && !legacy {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if legacy {
            if !metadata.file_type().is_file()
                || !name.ends_with(LEGACY_INSPECTION_SUFFIX)
                || name.len() <= LEGACY_INSPECTION_PREFIX.len() + LEGACY_INSPECTION_SUFFIX.len()
            {
                return Err(invalid(format!(
                    "legacy snapshot inspection entry {} is not a recognized regular staging file; its footprint is unavailable",
                    entry.path().display()
                )));
            }
            inventory.legacy_files += 1; // Bounded by the entry budget.
            inventory.bytes = inventory
                .bytes
                .checked_add(metadata.len())
                .ok_or_else(|| invalid("staging inventory byte count overflow"))?;
        } else {
            if !metadata.file_type().is_dir() {
                return Err(invalid(format!(
                    "staging-shaped entry {} is not a real directory",
                    entry.path().display()
                )));
            }
            inventory.directories += 1;
            pending.push(entry.path());
        }
        if stop_at_count_limit && inventory.artifacts() >= MAX_RETAINED_STAGES {
            return Ok(inventory);
        }
    }
    while let Some(directory) = pending.pop() {
        if !fs::symlink_metadata(&directory)?.file_type().is_dir() {
            return Err(invalid("staging directory changed type during inventory"));
        }
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            visit()?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_dir() {
                pending.push(entry.path());
            } else if metadata.file_type().is_file() {
                inventory.bytes = inventory
                    .bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| invalid("staging inventory byte count overflow"))?;
            } else {
                return Err(invalid(format!(
                    "staging entry {} is a symlink or special file; its footprint is unavailable",
                    entry.path().display()
                )));
            }
        }
    }
    Ok(inventory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixture() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "am-staging-assessment-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        // Retain fixtures: inspecting failed-export evidence never deletes it.
        root.canonicalize().unwrap()
    }

    #[test]
    fn count_byte_boundary_and_overflow_refusals_match_the_producer() {
        let assessment = AutomaticBackupStagingAssessment {
            inventory: ProactiveBackupStagingInventory {
                directories: 2,
                legacy_files: 0,
                bytes: 60,
            },
            projected_bytes: 40,
            byte_limit: 100,
        };
        assert!(assessment.check_budget().is_ok());
        assert!(
            AutomaticBackupStagingAssessment {
                projected_bytes: 41,
                ..assessment
            }
            .check_budget()
            .is_err()
        );
        let full_count = AutomaticBackupStagingAssessment {
            inventory: ProactiveBackupStagingInventory {
                directories: 3,
                legacy_files: 0,
                bytes: 0,
            },
            projected_bytes: 0,
            byte_limit: u64::MAX,
        };
        assert!(
            full_count
                .check_budget()
                .unwrap_err()
                .to_string()
                .contains("3 retained staging artifacts")
        );
        let overflow = AutomaticBackupStagingAssessment {
            inventory: ProactiveBackupStagingInventory {
                bytes: u64::MAX,
                ..ProactiveBackupStagingInventory::default()
            },
            projected_bytes: 1,
            byte_limit: u64::MAX,
        };
        assert!(
            overflow
                .check_budget()
                .unwrap_err()
                .to_string()
                .contains("overflow")
        );
    }

    #[test]
    fn real_modern_and_legacy_stages_share_admission_and_health() {
        let root = fixture();
        let primary = root.join("mail.db");
        fs::write(&primary, b"mailbox").unwrap();
        let modern = root.join(".mcp-agent-mail-proactive-backup-one");
        fs::create_dir(&modern).unwrap();
        fs::write(modern.join("snapshot.sqlite3"), b"modern").unwrap();
        let legacy = root.join(".snapshot-inspect-one.sqlite3");
        fs::write(&legacy, b"legacy").unwrap();
        let health = inspect_proactive_backup_staging(&primary).unwrap();
        assert_eq!(health, inspect_staging_for_admission(&root).unwrap());
        assert_eq!(health.artifacts(), 2);
        assert_eq!(health.bytes, 12);
        assert!(
            assess_automatic_backup_staging(&primary, health)
                .unwrap()
                .check_budget()
                .is_ok()
        );
        fs::write(root.join(".snapshot-inspect-two.sqlite3"), b"third").unwrap();
        for inventory in [
            inspect_proactive_backup_staging(&primary).unwrap(),
            inspect_staging_for_admission(&root).unwrap(),
        ] {
            let reason = assess_automatic_backup_staging(&primary, inventory)
                .unwrap()
                .check_budget()
                .unwrap_err()
                .to_string();
            assert!(reason.contains("3 retained staging artifacts"), "{reason}");
        }
        assert_eq!(fs::read(&primary).unwrap(), b"mailbox");
        assert_eq!(fs::read(legacy).unwrap(), b"legacy");
        assert_eq!(
            fs::read(modern.join("snapshot.sqlite3")).unwrap(),
            b"modern"
        );
    }

    #[test]
    fn generation_projection_counts_wal_and_rejects_unknown_sizes() {
        let root = fixture();
        let primary = root.join("mail.db");
        let wal = root.join("mail.db-wal");
        fs::write(&primary, [0_u8; 7]).unwrap();
        fs::write(&wal, [0_u8; 11]).unwrap();
        let assessment =
            assess_automatic_backup_staging(&primary, ProactiveBackupStagingInventory::default())
                .unwrap();
        assert_eq!(assessment.projected_bytes, 36);
        assert_eq!(assessment.byte_limit, STAGING_BYTES_FLOOR);
        let large = assessment_for_sizes(
            ProactiveBackupStagingInventory::default(),
            STAGING_BYTES_FLOOR,
            13,
        )
        .unwrap();
        assert_eq!(large.projected_bytes, 2 * (STAGING_BYTES_FLOOR + 13));
        assert_eq!(large.byte_limit, 5 * (STAGING_BYTES_FLOOR + 13));
        assert!(
            assessment_for_sizes(ProactiveBackupStagingInventory::default(), u64::MAX, 1).is_err()
        );
        assert!(
            assessment_for_sizes(ProactiveBackupStagingInventory::default(), u64::MAX / 4, 0)
                .is_err()
        );
        fs::rename(&wal, root.join("preserved-wal")).unwrap();
        fs::create_dir(&wal).unwrap();
        assert!(
            assess_automatic_backup_staging(&primary, ProactiveBackupStagingInventory::default())
                .is_err()
        );
        assert!(
            assess_automatic_backup_staging(
                &root.join("missing.db"),
                ProactiveBackupStagingInventory::default()
            )
            .is_err()
        );
    }

    #[test]
    fn complete_health_scan_does_not_hide_count_saturated_bytes_or_work_limits() {
        let root = fixture();
        let primary = root.join("mail.db");
        fs::write(&primary, b"mailbox").unwrap();
        for ordinal in 0..4 {
            let stage = root.join(format!(".mcp-agent-mail-proactive-backup-{ordinal}"));
            fs::create_dir(&stage).unwrap();
            fs::write(stage.join("snapshot.sqlite3"), b"evidence").unwrap();
        }
        let admission = inspect_staging_for_admission(&root).unwrap();
        assert_eq!(admission.artifacts(), 3);
        assert_eq!(admission.bytes, 0);
        let health = inspect_proactive_backup_staging(&primary).unwrap();
        assert_eq!(health.artifacts(), 4);
        assert_eq!(health.bytes, 32);
        assert!(inspect_proactive_backup_staging_with_limit(&primary, 8).is_err());
        assert_eq!(
            inspect_proactive_backup_staging_with_limit(&primary, 9).unwrap(),
            health
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_links_and_malformed_entries_never_establish_healthy_admission() {
        let root = fixture();
        let primary = root.join("mail.db");
        fs::write(&primary, b"mailbox").unwrap();
        let outside = root.join("outside-evidence");
        fs::write(&outside, b"preserve").unwrap();
        let alias = root.join(".snapshot-inspect-link.sqlite3");
        std::os::unix::fs::symlink(&outside, &alias).unwrap();
        assert!(inspect_proactive_backup_staging(&primary).is_err());
        assert!(inspect_staging_for_admission(&root).is_err());
        assert_eq!(fs::read_link(&alias).unwrap(), outside);
        fs::rename(&alias, root.join("preserved-link")).unwrap();
        fs::write(root.join(".snapshot-inspect-.sqlite3"), b"unclassified").unwrap();
        assert!(inspect_proactive_backup_staging(&primary).is_err());
        assert!(inspect_staging_for_admission(&root).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"preserve");
    }
}
