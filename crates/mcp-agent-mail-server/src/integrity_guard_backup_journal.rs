//! Durable, cross-process admission for automatic backups (GH#326).
//!
//! A synced intent precedes every export and the advisory lock is held until
//! completion. A killed worker therefore leaves a bounded cooldown, rather
//! than forgetting the failed copy on the next startup. Explicit operator
//! backup/recovery commands are not gated here.
//!
//! Two checksummed slots share one permanent inode. Updating the inactive slot
//! never truncates, replaces, or deletes the locked file. Interrupted completion
//! writes retain the preceding admitted intent. Both invalid slots fail closed
//! for automatic backup admission only; this is not evidence of live corruption.

use super::schedule::{BackupCompletion, BackupKind, BACKUP_RETRY_INITIAL, BACKUP_RETRY_MAX};
use sha2::{Digest as _, Sha256};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use nix::fcntl::{AtFlags, OFlag, open, openat};
use nix::sys::stat::{Mode, SFlag, fstatat};

const SLOT_BYTES: usize = 128;
const PAYLOAD_BYTES: usize = SLOT_BYTES - 32;
const JOURNAL_BYTES: usize = SLOT_BYTES * 2;
const MAGIC: &[u8; 8] = b"AMBACK01";
const DIRECTORY_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_NONBLOCK)
    .union(OFlag::O_CLOEXEC);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct State {
    sequence: u64,
    failures: u32,
    verified_pending: bool,
    in_flight: bool,
    recorded_at: u64,
    delay_secs: u64,
}

impl State {
    fn remaining(self, now: u64) -> u64 {
        // A reversed wall clock is never permission to retry immediately.
        self.delay_secs.saturating_sub(now.saturating_sub(self.recorded_at))
    }

    fn encode(self) -> [u8; SLOT_BYTES] {
        let mut bytes = [0_u8; SLOT_BYTES];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..16].copy_from_slice(&self.sequence.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.failures.to_le_bytes());
        bytes[20] = u8::from(self.verified_pending) | (u8::from(self.in_flight) << 1);
        bytes[24..32].copy_from_slice(&self.recorded_at.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.delay_secs.to_le_bytes());
        let digest = Sha256::digest(&bytes[..PAYLOAD_BYTES]);
        bytes[PAYLOAD_BYTES..].copy_from_slice(&digest);
        bytes
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != SLOT_BYTES
            || &bytes[..8] != MAGIC
            || bytes[20] & !3 != 0
            || bytes[21..24].iter().any(|byte| *byte != 0)
            || bytes[40..PAYLOAD_BYTES].iter().any(|byte| *byte != 0)
            || Sha256::digest(&bytes[..PAYLOAD_BYTES])[..] != bytes[PAYLOAD_BYTES..]
        {
            return None;
        }
        let state = Self {
            sequence: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
            failures: u32::from_le_bytes(bytes[16..20].try_into().ok()?),
            verified_pending: bytes[20] & 1 != 0,
            in_flight: bytes[20] & 2 != 0,
            recorded_at: u64::from_le_bytes(bytes[24..32].try_into().ok()?),
            delay_secs: u64::from_le_bytes(bytes[32..40].try_into().ok()?),
        };
        if state.sequence == 0
            || state.delay_secs > BACKUP_RETRY_MAX.as_secs()
            || (state.in_flight
                && (state.failures == 0 || state.delay_secs != BACKUP_RETRY_MAX.as_secs()))
        {
            return None;
        }
        Some(state)
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn unix_seconds() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| invalid("automatic backup journal: wall clock predates the Unix epoch"))
}

/// Open every ancestor through its retained parent; never follow a late
/// user-controlled directory or leaf symlink while opening the journal.
fn open_parent(sqlite_path: &Path) -> io::Result<File> {
    if !sqlite_path.is_absolute() {
        return Err(invalid("automatic backup journal requires an absolute mailbox path"));
    }
    let parent = sqlite_path.parent().ok_or_else(|| invalid("mailbox has no parent"))?;
    #[cfg(target_os = "macos")]
    let physical_parent = {
        let mut physical = parent.to_path_buf();
        for (alias, target) in [("/var", "/private/var"), ("/tmp", "/private/tmp"), ("/etc", "/private/etc")] {
            let alias = Path::new(alias);
            if let Ok(tail) = parent.strip_prefix(alias)
                && mcp_agent_mail_core::disk::is_trusted_system_directory_alias(alias)
            {
                physical = Path::new(target).join(tail);
                break;
            }
        }
        physical
    };
    #[cfg(target_os = "macos")]
    let parent = physical_parent.as_path();

    let mut directory = File::from(open("/", DIRECTORY_FLAGS, Mode::empty())?);
    for component in parent.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                directory = File::from(openat(
                    &directory, name, DIRECTORY_FLAGS, Mode::empty(),
                )?);
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(invalid("automatic backup journal refuses parent traversal"));
            }
        }
    }
    let source = fstatat(
        &directory,
        sqlite_path.file_name().ok_or_else(|| invalid("mailbox has no filename"))?,
        AtFlags::AT_SYMLINK_NOFOLLOW,
    )?;
    if source.st_mode & SFlag::S_IFMT.bits() != SFlag::S_IFREG.bits() {
        return Err(invalid("automatic backup journal requires a regular mailbox file"));
    }
    Ok(directory)
}

/// Owns one mailbox's cross-process backup lock through the entire export.
/// Drop releases only the descriptor/lock. It never removes the durable intent.
pub(super) struct AutomaticBackupLease {
    file: File,
    parent: File,
    leaf: OsString,
    state: State,
    kind: BackupKind,
    previous_failures: u32,
    previous_delay: u64,
}

impl AutomaticBackupLease {
    pub(super) fn try_begin(sqlite_path: &Path, requested: BackupKind) -> io::Result<Option<Self>> {
        Self::try_begin_at(sqlite_path, requested, unix_seconds()?)
    }

    fn try_begin_at(sqlite_path: &Path, requested: BackupKind, now: u64) -> io::Result<Option<Self>> {
        let parent = open_parent(sqlite_path)?;
        let mut leaf = sqlite_path.file_name().ok_or_else(|| invalid("mailbox has no filename"))?.to_os_string();
        leaf.push(".automatic-backup-state");
        let file = File::from(openat(
            &parent,
            leaf.as_os_str(),
            OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )?);
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        }
        let mut lease = Self {
            file,
            parent,
            leaf,
            state: State::default(),
            kind: requested,
            previous_failures: 0,
            previous_delay: 0,
        };
        lease.validate_entry()?;
        lease.state = lease.read_state()?;
        let pending_changed = requested == BackupKind::Verified && !lease.state.verified_pending;
        lease.state.verified_pending |= requested == BackupKind::Verified;
        if lease.state.remaining(now) > 0 {
            if pending_changed {
                // Retain the new request without sliding or clearing backoff.
                lease.persist()?;
            }
            return Ok(None);
        }
        lease.kind = if lease.state.verified_pending { BackupKind::Verified } else { requested };
        lease.previous_failures = lease.state.failures;
        lease.previous_delay = lease.state.delay_secs;
        lease.state.failures = lease.state.failures.saturating_add(1);
        lease.state.in_flight = true;
        lease.state.recorded_at = now;
        // Completion time is unknowable after a kill. Reserve the maximum
        // window before starting; a live worker is additionally protected by
        // its held lock, even if export takes longer than this window.
        lease.state.delay_secs = BACKUP_RETRY_MAX.as_secs();
        lease.persist()?; // No producer is called until both syncs succeed.
        Ok(Some(lease))
    }

    pub(super) const fn kind(&self) -> BackupKind {
        self.kind
    }

    pub(super) fn finish(self, completion: BackupCompletion) -> io::Result<()> {
        self.finish_at(completion, unix_seconds()?)
    }

    fn finish_at(mut self, completion: BackupCompletion, now: u64) -> io::Result<()> {
        match completion {
            BackupCompletion::Published => {
                self.state.failures = 0;
                self.state.delay_secs = 0;
                if self.kind == BackupKind::Verified {
                    self.state.verified_pending = false;
                }
            }
            BackupCompletion::Failed => {
                let exponent = self.state.failures.saturating_sub(1).min(31);
                self.state.delay_secs = BACKUP_RETRY_INITIAL
                    .saturating_mul(1_u32 << exponent)
                    .min(BACKUP_RETRY_MAX)
                    .as_secs();
            }
            BackupCompletion::Skipped => {
                self.state.failures = self.previous_failures;
                self.state.delay_secs = if self.kind == BackupKind::Verified || self.previous_failures > 0 {
                    self.previous_delay.max(BACKUP_RETRY_INITIAL.as_secs())
                } else {
                    0
                };
            }
        }
        self.state.in_flight = false;
        self.state.recorded_at = now;
        self.persist()
    }

    #[allow(clippy::unnecessary_cast)] // stat field widths differ across Unix targets.
    fn validate_entry(&self) -> io::Result<()> {
        let opened = self.file.metadata()?;
        let named = fstatat(&self.parent, self.leaf.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)?;
        if !opened.is_file()
            || opened.nlink() != 1
            || opened.mode() & 0o077 != 0
            || opened.len() > JOURNAL_BYTES as u64
            || named.st_nlink != 1
            || named.st_mode & 0o077 != 0
            || named.st_mode & SFlag::S_IFMT.bits() != SFlag::S_IFREG.bits()
            || opened.dev() != named.st_dev as u64
            || opened.ino() != named.st_ino as u64
        {
            return Err(invalid("automatic backup journal is not a private, exclusive, unchanged regular file"));
        }
        Ok(())
    }

    fn read_state(&mut self) -> io::Result<State> {
        let length = usize::try_from(self.file.metadata()?.len())
            .map_err(|_| invalid("automatic backup journal is oversized"))?;
        if length > JOURNAL_BYTES {
            return Err(invalid("automatic backup journal is oversized"));
        }
        if length == 0 {
            return Ok(State::default());
        }
        let mut bytes = [0_u8; JOURNAL_BYTES];
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut bytes[..length])?;
        let (slots, _) = bytes.as_chunks::<SLOT_BYTES>();
        for slot in slots {
            if slot.starts_with(b"AMBACK") && &slot[..8] != MAGIC {
                return Err(invalid("automatic backup journal uses an unsupported version"));
            }
        }
        let first = State::decode(&bytes[..SLOT_BYTES]);
        let second = State::decode(&bytes[SLOT_BYTES..]);
        if first.is_some_and(|state| state.sequence % 2 != 0)
            || second.is_some_and(|state| state.sequence % 2 != 1)
        {
            return Err(invalid("automatic backup journal has misplaced generations"));
        }
        match (first, second) {
            (Some(a), Some(b)) => {
                if a.sequence.abs_diff(b.sequence) != 1 {
                    return Err(invalid("automatic backup journal has ambiguous generations"));
                }
                Ok(if a.sequence > b.sequence { a } else { b })
            }
            (Some(state), None) | (None, Some(state)) => Ok(state),
            (None, None) => Err(invalid("automatic backup journal has no intact supported record; inspect it before retrying automatic exports")),
        }
    }

    fn persist(&mut self) -> io::Result<()> {
        self.validate_entry()?;
        self.state.sequence = self.state.sequence.checked_add(1)
            .ok_or_else(|| invalid("automatic backup journal generation exhausted"))?;
        let offset = (self.state.sequence % 2) * SLOT_BYTES as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&self.state.encode())?;
        self.file.sync_all()?;
        self.parent.sync_all()?;
        self.validate_entry()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::path::PathBuf;
    use std::process::Command;

    const NOW: u64 = 1_800_000_000;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("private fixture");
        let database = directory.path().join("storage.sqlite3");
        fs::write(&database, b"untouched mailbox sentinel").expect("seed mailbox");
        (directory, database)
    }

    fn journal_path(database: &Path) -> PathBuf {
        let mut path = database.as_os_str().to_os_string();
        path.push(".automatic-backup-state");
        PathBuf::from(path)
    }

    fn begin(database: &Path, kind: BackupKind, now: u64) -> AutomaticBackupLease {
        AutomaticBackupLease::try_begin_at(database, kind, now)
            .expect("admission IO")
            .expect("eligible admission")
    }

    #[test]
    fn lock_is_held_past_cooldown_until_the_export_finishes() {
        let (_directory, database) = fixture();
        let lease = begin(&database, BackupKind::Proactive, NOW);
        assert!(AutomaticBackupLease::try_begin_at(
            &database, BackupKind::Verified, NOW + BACKUP_RETRY_MAX.as_secs() * 2,
        ).unwrap().is_none());
        lease.finish_at(BackupCompletion::Published, NOW).unwrap();
        assert!(AutomaticBackupLease::try_begin_at(&database, BackupKind::Proactive, NOW)
            .unwrap().is_some());
    }

    #[test]
    fn killed_export_leaves_a_durable_bounded_intent() {
        let (_directory, database) = fixture();
        drop(begin(&database, BackupKind::Proactive, NOW));
        for elapsed in [0, 1, 300, BACKUP_RETRY_MAX.as_secs() - 1] {
            assert!(AutomaticBackupLease::try_begin_at(
                &database, BackupKind::Proactive, NOW + elapsed,
            ).unwrap().is_none(), "restart at {elapsed}s bypassed admission");
        }
        let lease = begin(&database, BackupKind::Proactive, NOW + BACKUP_RETRY_MAX.as_secs());
        assert_eq!(lease.state.failures, 2);
        assert_eq!(fs::metadata(journal_path(&database)).unwrap().len(), JOURNAL_BYTES as u64);
        assert_eq!(fs::read(&database).unwrap(), b"untouched mailbox sentinel");
    }

    #[test]
    fn completed_failure_delay_starts_at_completion_and_survives_reopen() {
        let (_directory, database) = fixture();
        let completed = NOW + 3600;
        begin(&database, BackupKind::Proactive, NOW)
            .finish_at(BackupCompletion::Failed, completed).unwrap();
        assert!(AutomaticBackupLease::try_begin_at(
            &database, BackupKind::Proactive, completed + BACKUP_RETRY_INITIAL.as_secs() - 1,
        ).unwrap().is_none());
        let retry = begin(&database, BackupKind::Proactive, completed + BACKUP_RETRY_INITIAL.as_secs());
        assert_eq!(retry.state.failures, 2);
    }

    #[test]
    fn verified_request_during_backoff_is_retained_without_sliding_deadline() {
        let (_directory, database) = fixture();
        begin(&database, BackupKind::Proactive, NOW)
            .finish_at(BackupCompletion::Failed, NOW).unwrap();
        for elapsed in [100, 300, 600] {
            assert!(AutomaticBackupLease::try_begin_at(
                &database, BackupKind::Verified, NOW + elapsed,
            ).unwrap().is_none());
        }
        let retry = begin(&database, BackupKind::Proactive, NOW + BACKUP_RETRY_INITIAL.as_secs());
        assert_eq!(retry.kind(), BackupKind::Verified);
        retry.finish_at(BackupCompletion::Published, NOW + BACKUP_RETRY_INITIAL.as_secs()).unwrap();
        let next = begin(&database, BackupKind::Proactive, NOW + BACKUP_RETRY_INITIAL.as_secs());
        assert_eq!(next.kind(), BackupKind::Proactive);
        assert_eq!(next.previous_failures, 0);
    }

    #[test]
    fn verified_skip_preserves_request_and_previous_failure_count() {
        let (_directory, database) = fixture();
        begin(&database, BackupKind::Verified, NOW)
            .finish_at(BackupCompletion::Skipped, NOW).unwrap();
        assert!(AutomaticBackupLease::try_begin_at(&database, BackupKind::Proactive, NOW)
            .unwrap().is_none());
        let retry = begin(&database, BackupKind::Proactive, NOW + BACKUP_RETRY_INITIAL.as_secs());
        assert_eq!(retry.kind(), BackupKind::Verified);
        assert_eq!(retry.previous_failures, 0);
    }

    #[test]
    fn normal_proactive_skip_is_not_a_failure() {
        let (_directory, database) = fixture();
        begin(&database, BackupKind::Proactive, NOW)
            .finish_at(BackupCompletion::Skipped, NOW).unwrap();
        let next = begin(&database, BackupKind::Proactive, NOW);
        assert_eq!(next.previous_failures, 0);
    }

    #[test]
    fn restart_every_quick_cycle_cannot_reset_exponential_failure_pacing() {
        let (_directory, database) = fixture();
        let mut attempts = Vec::new();
        for minute in (0_u64..24 * 60).step_by(5) {
            if let Some(lease) = AutomaticBackupLease::try_begin_at(
                &database, BackupKind::Verified, NOW + minute * 60,
            ).unwrap() {
                attempts.push(minute);
                lease.finish_at(BackupCompletion::Failed, NOW + minute * 60).unwrap();
            }
        }
        assert_eq!(attempts, [0, 15, 45, 105, 225, 465, 825, 1185]);
        assert_eq!(fs::metadata(journal_path(&database)).unwrap().len(), JOURNAL_BYTES as u64);
    }

    #[test]
    fn crash_after_each_admission_is_also_bounded_across_restarts() {
        let (_directory, database) = fixture();
        let mut attempts = Vec::new();
        for minute in (0_u64..24 * 60).step_by(5) {
            if let Some(lease) = AutomaticBackupLease::try_begin_at(
                &database, BackupKind::Proactive, NOW + minute * 60,
            ).unwrap() {
                attempts.push(minute);
                drop(lease); // No completion record: same durable state as a kill.
            }
        }
        assert_eq!(attempts, [0, 360, 720, 1080]);
    }

    #[test]
    fn backwards_clock_cannot_clear_admission() {
        let (_directory, database) = fixture();
        drop(begin(&database, BackupKind::Proactive, NOW));
        assert!(AutomaticBackupLease::try_begin_at(&database, BackupKind::Proactive, NOW - 3600)
            .unwrap().is_none());
    }

    #[test]
    fn interrupted_completion_retains_prior_synced_intent() {
        let (_directory, database) = fixture();
        let lease = begin(&database, BackupKind::Proactive, NOW);
        assert_eq!(lease.state.sequence, 1);
        drop(lease);
        let mut file = OpenOptions::new().write(true).open(journal_path(&database)).unwrap();
        // Slot zero is the NEXT completion slot, not the admitted intent slot.
        file.write_all(b"interrupted completion").unwrap();
        file.sync_all().unwrap();
        drop(file);
        assert!(AutomaticBackupLease::try_begin_at(&database, BackupKind::Proactive, NOW + 300)
            .unwrap().is_none());
    }

    #[test]
    fn corrupt_or_oversized_journal_is_not_rewritten_or_treated_as_healthy() {
        let (_directory, database) = fixture();
        for bytes in [b"not a journal".to_vec(), vec![0_u8; JOURNAL_BYTES + 1]] {
            fs::write(journal_path(&database), &bytes).unwrap();
            fs::set_permissions(journal_path(&database), fs::Permissions::from_mode(0o600)).unwrap();
            assert!(AutomaticBackupLease::try_begin_at(&database, BackupKind::Proactive, NOW).is_err());
            assert_eq!(fs::read(journal_path(&database)).unwrap(), bytes);
        }
        assert_eq!(fs::read(&database).unwrap(), b"untouched mailbox sentinel");
    }

    #[test]
    fn symlink_and_hardlink_journals_never_modify_their_target() {
        let (directory, database) = fixture();
        let target = directory.path().join("evidence");
        fs::write(&target, b"preserve evidence").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, journal_path(&database)).unwrap();
        assert!(AutomaticBackupLease::try_begin_at(&database, BackupKind::Proactive, NOW).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"preserve evidence");

        let other = directory.path().join("other.sqlite3");
        fs::write(&other, b"other mailbox").unwrap();
        fs::hard_link(&target, journal_path(&other)).unwrap();
        assert!(AutomaticBackupLease::try_begin_at(&other, BackupKind::Proactive, NOW).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"preserve evidence");
    }

    #[test]
    fn symlinked_ancestor_is_refused_without_creating_a_journal() {
        let (directory, database) = fixture();
        let alias = directory.path().join("alias");
        symlink(directory.path(), &alias).unwrap();
        assert!(AutomaticBackupLease::try_begin_at(
            &alias.join("storage.sqlite3"), BackupKind::Proactive, NOW,
        ).is_err());
        assert!(!journal_path(&database).exists());
    }

    #[test]
    fn replacement_entry_is_never_overwritten_on_completion() {
        let (directory, database) = fixture();
        let lease = begin(&database, BackupKind::Proactive, NOW);
        fs::rename(journal_path(&database), directory.path().join("retained-intent")).unwrap();
        fs::write(journal_path(&database), b"replacement evidence").unwrap();
        assert!(lease.finish_at(BackupCompletion::Published, NOW).is_err());
        assert_eq!(fs::read(journal_path(&database)).unwrap(), b"replacement evidence");
    }

    #[test]
    fn mailbox_admission_state_is_isolated_and_private() {
        let (directory, database) = fixture();
        drop(begin(&database, BackupKind::Proactive, NOW));
        let other = directory.path().join("other.sqlite3");
        fs::write(&other, b"other mailbox").unwrap();
        let lease = begin(&other, BackupKind::Proactive, NOW);
        assert_eq!(lease.previous_failures, 0);
        assert_eq!(fs::metadata(journal_path(&other)).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn records_check_version_checksum_and_bounded_delay() {
        let state = State { sequence: 1, failures: 1, in_flight: true,
            delay_secs: BACKUP_RETRY_MAX.as_secs(), recorded_at: NOW, ..State::default() };
        assert_eq!(State::decode(&state.encode()), Some(state));
        let mut damaged = state.encode();
        damaged[24] ^= 1;
        assert!(State::decode(&damaged).is_none());
        let future = State { delay_secs: BACKUP_RETRY_MAX.as_secs() + 1, ..state };
        assert!(State::decode(&future.encode()).is_none());
        let mut unknown = state.encode();
        unknown[..8].copy_from_slice(b"AMBACK99");
        let digest = Sha256::digest(&unknown[..PAYLOAD_BYTES]);
        unknown[PAYLOAD_BYTES..].copy_from_slice(&digest);
        assert!(State::decode(&unknown).is_none());
    }

    #[test]
    fn actual_separate_processes_share_failure_admission() {
        let (_directory, database) = fixture();
        for expected in ["admitted", "deferred"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "integrity_guard::backup_journal::tests::durable_backup_child_process_probe", "--nocapture"])
                .env("AM_BACKUP_JOURNAL_TEST_PATH", &database)
                .env("AM_BACKUP_JOURNAL_TEST_EXPECTED", expected)
                .output().unwrap();
            assert!(output.status.success(), "child: {}", String::from_utf8_lossy(&output.stderr));
            assert!(String::from_utf8_lossy(&output.stderr).contains("GH326_CHILD_PROBE_EXECUTED"),
                "child test filter matched no test");
        }
    }

    #[test]
    fn durable_backup_child_process_probe() {
        let Some(path) = std::env::var_os("AM_BACKUP_JOURNAL_TEST_PATH") else { return; };
        let expected = std::env::var("AM_BACKUP_JOURNAL_TEST_EXPECTED").unwrap();
        let lease = AutomaticBackupLease::try_begin_at(Path::new(&path), BackupKind::Proactive, NOW).unwrap();
        match expected.as_str() {
            "admitted" => lease.expect("first process admitted").finish_at(BackupCompletion::Failed, NOW).unwrap(),
            "deferred" => assert!(lease.is_none(), "fresh process bypassed durable failure"),
            _ => panic!("invalid fixture expectation"),
        }
        eprintln!("GH326_CHILD_PROBE_EXECUTED");
    }
}
