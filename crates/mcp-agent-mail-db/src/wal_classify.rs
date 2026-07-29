//! WAL/SHM sidecar classification + safe-checkpoint policy (bead br-bvq1x.7.4 / G4).
//!
//! Sidecar handling has been a recurring root-cause surface: a 0-byte WAL was
//! once a false-positive "truncation"; a header-only WAL surfaced as a rebuild
//! error ("WAL file too small for header during rebuild"); stale-WAL
//! checkpointing immediately preceded probe failures; and a checkpoint attempted
//! while a live writer still owned the database produced a malformed image.
//!
//! This module classifies a database's WAL/SHM sidecars into a small set of
//! precise states and applies a safe policy that **never hides real corruption
//! and never checkpoints while another live writer owns the DB**:
//!
//! - benign 0-byte / idle WAL                       → no action
//! - valid 32-byte header (frameless idle WAL)      → no action (opens as-is)
//! - truncated WAL (1..=31 bytes, partial header)   → quarantine (cannot rebuild)
//! - malformed WAL (bad magic/checksum, incl. a     → quarantine (would corrupt)
//!   32-byte all-zeros garbage header — GH#99)
//! - stale WAL with committed frames                → checkpoint *only when safe*
//! - WAL/SHM symlink                                → quarantine (never follow)
//! - orphan sidecar (primary gone)                  → quarantine
//! - a live writer owning the DB                    → refuse all mutation
//!
//! NOTE: a complete 32-byte WAL header with a VALID magic is a frameless idle
//! WAL that the current engine opens and checkpoints without error (proven by
//! `engine_opens_and_checkpoints_a_32_byte_header_only_wal`). Treating it as a
//! truncation artifact false-failed `am doctor health` and cascaded into
//! spurious archive reconstructs (ts1/css incidents, 2026-06-17), so it is
//! classified `IdleEmpty`, not `HeaderOnlyOrTruncated`.
//!
//! It reuses [`crate::pool`] for sidecar inspection, live-owner detection
//! (D2/D4), and the underlying truncate checkpoint, rather than forking a
//! parallel WAL path.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::DbResult;
use crate::pool::{
    SQLITE_WAL_HEADER_BYTES, inspect_mailbox_ownership, inspect_mailbox_sidecar_state,
    wal_checkpoint_truncate_path,
};

// SQLite WAL header magic. Bit 0 is the big-endian-checksum flag: `..82`
// (bit 0 clear) means the header checksum words are computed little-endian,
// `..83` (bit 0 set) means big-endian.
/// WAL header magic whose checksums are computed little-endian.
const WAL_MAGIC_LITTLE_ENDIAN_CKSUM: u32 = 0x377f_0682;
/// WAL header magic whose checksums are computed big-endian.
const WAL_MAGIC_BIG_ENDIAN_CKSUM: u32 = 0x377f_0683;
/// WAL/SHM mtime difference (seconds) beyond which we flag suspicious drift.
const MTIME_DRIFT_THRESHOLD_SECS: i64 = 3600;

/// The classified state of a database's WAL sidecar.
///
/// This is the most significant content/structure verdict; orthogonal signals
/// (a live writer owning the DB, mtime drift) are carried as separate fields on
/// [`WalSidecarClassification`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WalSidecarState {
    /// No WAL sidecar present (clean WAL-mode-idle or non-WAL database).
    NoSidecar,
    /// A benign, openable WAL: a 0-byte WAL (idle/just-checkpointed) OR an
    /// exactly-32-byte header with a valid magic (a frameless idle WAL the
    /// engine opens as-is). Neither is a truncation artifact.
    IdleEmpty,
    /// Non-empty WAL SHORTER than the 32-byte header (1..=31 bytes): a partial,
    /// unreadable header. Opening it triggers "WAL file too small for header".
    HeaderOnlyOrTruncated,
    /// WAL with an invalid magic or header checksum — either larger than the
    /// header, or an exactly-32-byte garbage header (e.g. the all-zeros WAL from
    /// GH#99). Checkpointing it risks producing a malformed image.
    MalformedHeader,
    /// WAL larger than the header with a valid header — carries committed frames
    /// and should be checkpointed (when no live writer owns the DB).
    StaleNeedsCheckpoint,
    /// The WAL or SHM path is a symlink — never followed; quarantined in place.
    Symlink,
    /// A sidecar exists but the primary database file is missing — an orphan.
    Orphan,
}

impl WalSidecarState {
    /// Whether this state, on its own, indicates a damaged/unusable sidecar that
    /// must be quarantined rather than checkpointed.
    #[must_use]
    pub const fn is_damaged(self) -> bool {
        matches!(
            self,
            Self::HeaderOnlyOrTruncated | Self::MalformedHeader | Self::Symlink | Self::Orphan
        )
    }
}

/// Full classification of a database's sidecar situation.
#[derive(Debug, Clone, Serialize)]
pub struct WalSidecarClassification {
    pub state: WalSidecarState,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    /// WAL mtime minus primary mtime, in seconds (negative = WAL older). `None`
    /// when either mtime is unavailable.
    pub wal_mtime_drift_secs: Option<i64>,
    /// A live writer currently owns the database (D2 ownership). When true, no
    /// sidecar mutation (checkpoint or quarantine) is safe.
    pub live_writer_present: bool,
    pub detail: String,
}

impl WalSidecarClassification {
    /// Whether the WAL mtime drifts from the primary's by more than the
    /// suspicious-drift threshold (signal `(h)`).
    #[must_use]
    pub fn has_significant_mtime_drift(&self) -> bool {
        self.wal_mtime_drift_secs
            .is_some_and(|d| d.abs() >= MTIME_DRIFT_THRESHOLD_SECS)
    }
}

/// The safe action recommended for a classified sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarAction {
    /// Nothing to do (no sidecar, or a benign idle WAL).
    NoOp,
    /// Checkpoint the stale-but-valid WAL (safe: no live writer owns the DB).
    Checkpoint,
    /// Move the damaged WAL aside (header-only/truncated or malformed).
    QuarantineWal,
    /// Move a symlinked or orphaned sidecar aside.
    QuarantineSidecar,
    /// Refuse: a live writer owns the DB; never checkpoint/repair under it.
    RefuseLiveWriter,
}

/// The verdict of validating a 32-byte WAL header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WalHeaderVerdict {
    /// Fewer than 32 bytes were available — not a complete header.
    TooShort,
    /// The 4-byte magic is neither WAL magic value.
    BadMagic,
    /// Magic is valid but the header checksum does not match its body.
    ChecksumMismatch,
    /// Magic and header checksum are both valid.
    Valid,
}

/// Validate a SQLite WAL file header (the first 32 bytes).
///
/// Checks the 4-byte magic (`0x377f0682`/`0x377f0683`) and recomputes the
/// header checksum over the first 24 bytes using SQLite's accumulator algorithm
/// in the byte order the magic dictates, comparing against the stored
/// big-endian checksum words at offsets 24 and 28.
#[must_use]
pub fn wal_header_verdict(header: &[u8]) -> WalHeaderVerdict {
    if header.len() < 32 {
        return WalHeaderVerdict::TooShort;
    }
    let magic = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let big_endian_checksum = match magic {
        WAL_MAGIC_LITTLE_ENDIAN_CKSUM => false,
        WAL_MAGIC_BIG_ENDIAN_CKSUM => true,
        _ => return WalHeaderVerdict::BadMagic,
    };

    let read_word = |offset: usize| -> u32 {
        let bytes = [
            header[offset],
            header[offset + 1],
            header[offset + 2],
            header[offset + 3],
        ];
        if big_endian_checksum {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        }
    };

    // SQLite's WAL checksum: s1/s2 accumulators over the first 24 header bytes
    // (three 8-byte word pairs).
    let mut s1: u32 = 0;
    let mut s2: u32 = 0;
    let mut offset = 0;
    while offset < 24 {
        s1 = s1.wrapping_add(read_word(offset)).wrapping_add(s2);
        s2 = s2.wrapping_add(read_word(offset + 4)).wrapping_add(s1);
        offset += 8;
    }

    // The stored header checksum words are always big-endian on disk.
    let stored1 = u32::from_be_bytes([header[24], header[25], header[26], header[27]]);
    let stored2 = u32::from_be_bytes([header[28], header[29], header[30], header[31]]);
    if s1 == stored1 && s2 == stored2 {
        WalHeaderVerdict::Valid
    } else {
        WalHeaderVerdict::ChecksumMismatch
    }
}

/// Append a sidecar suffix (`-wal`/`-shm`) to a primary database path.
fn sidecar_path(primary: &Path, suffix: &str) -> PathBuf {
    let mut os = primary.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

fn read_wal_header(wal_path: &Path) -> Option<[u8; 32]> {
    use std::io::Read;
    let mut file = std::fs::File::open(wal_path).ok()?;
    let mut buf = [0u8; 32];
    file.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn mtime_drift_secs(primary: &Path, wal_path: &Path) -> Option<i64> {
    let primary_mtime = std::fs::metadata(primary).ok()?.modified().ok()?;
    let wal_mtime = std::fs::symlink_metadata(wal_path).ok()?.modified().ok()?;
    match wal_mtime.duration_since(primary_mtime) {
        Ok(forward) => i64::try_from(forward.as_secs()).ok(),
        Err(backward) => i64::try_from(backward.duration().as_secs())
            .ok()
            .map(|secs| -secs),
    }
}

/// Magic-aware test of whether a WAL sidecar FILE is a removable artifact.
///
/// Returns `true` for a WAL that blocks a clean open/rebuild (and should be
/// quarantined), `false` for a valid WAL that SQLite opens as-is. This is the
/// boolean the startup self-heal and doctor probes gate quarantine (and the
/// "needs repair" refusal) on. It refines the size-only
/// [`crate::pool::sqlite_wal_is_header_only_or_truncated`] by reading the 32-byte
/// header when one is fully present:
///
/// - `0` bytes → `false` (valid idle / just-checkpointed).
/// - `1..=31` bytes → `true` (incomplete, unreadable, truncated header).
/// - `32` bytes, **valid** magic → `false` (frameless idle WAL; opens as-is — ts1/css).
/// - `32` bytes, **invalid** magic → `true` (garbage header — GH#99/#119).
/// - `> 32` bytes → `false` (carries frames; checkpoint it, never quarantine).
///
/// Non-files (symlinks) return `false` here — they are handled by the dedicated
/// symlink-quarantine path and never dereferenced.
#[must_use]
pub fn wal_sidecar_is_truncation_artifact(wal_path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(wal_path) else {
        return false;
    };
    if !meta.file_type().is_file() {
        return false;
    }
    let len = meta.len();
    if len == 0 || len > SQLITE_WAL_HEADER_BYTES {
        return false;
    }
    if len < SQLITE_WAL_HEADER_BYTES {
        // 1..=31 bytes: a partial header that cannot even be read — SQLite
        // surfaces this as "WAL file too small for header".
        return true;
    }
    // Exactly the 32-byte header: a removable artifact ONLY when the magic /
    // checksum is invalid (a garbage header, e.g. the all-zeros WAL from GH#99).
    // A valid engine-written header is a benign frameless idle WAL that the
    // engine opens and checkpoints without error (proven by
    // `engine_opens_and_checkpoints_a_32_byte_header_only_wal`).
    !matches!(
        read_wal_header(wal_path).map(|header| wal_header_verdict(&header)),
        Some(WalHeaderVerdict::Valid)
    )
}

/// Classify a database's WAL/SHM sidecars from the filesystem alone.
///
/// This is filesystem-pure (no DB connection, no `/proc` scan): the live-writer
/// signal is left `false` here and filled in by [`safe_checkpoint`] (or any
/// caller that has run ownership detection), so the classifier stays cheap and
/// deterministically testable. `:memory:` databases have no sidecars.
#[must_use]
pub fn classify_wal_sidecar(primary: &Path) -> WalSidecarClassification {
    if primary.as_os_str() == ":memory:" {
        return WalSidecarClassification {
            state: WalSidecarState::NoSidecar,
            wal_bytes: 0,
            shm_bytes: 0,
            wal_mtime_drift_secs: None,
            live_writer_present: false,
            detail: "in-memory database (no sidecars)".to_string(),
        };
    }

    let wal_path = sidecar_path(primary, "-wal");
    let shm_path = sidecar_path(primary, "-shm");

    // Symlinked sidecars are a safety hazard regardless of content: never follow
    // them. Check before any size/content read so we never dereference one.
    if is_symlink(&wal_path) || is_symlink(&shm_path) {
        return WalSidecarClassification {
            state: WalSidecarState::Symlink,
            wal_bytes: 0,
            shm_bytes: 0,
            wal_mtime_drift_secs: None,
            live_writer_present: false,
            detail: "WAL or SHM sidecar is a symlink; quarantine without following".to_string(),
        };
    }

    let sidecars = inspect_mailbox_sidecar_state(primary);
    let wal_bytes = sidecars.wal_bytes.unwrap_or(0);
    let shm_bytes = sidecars.shm_bytes.unwrap_or(0);
    let primary_exists = primary.is_file();

    // Orphan: a sidecar lingers without its primary database.
    if !primary_exists && (sidecars.wal_exists || sidecars.shm_exists) {
        return WalSidecarClassification {
            state: WalSidecarState::Orphan,
            wal_bytes,
            shm_bytes,
            wal_mtime_drift_secs: None,
            live_writer_present: false,
            detail: "sidecar present without its primary database (orphan)".to_string(),
        };
    }

    if !sidecars.wal_exists {
        return WalSidecarClassification {
            state: WalSidecarState::NoSidecar,
            wal_bytes,
            shm_bytes,
            wal_mtime_drift_secs: None,
            live_writer_present: false,
            detail: "no WAL sidecar".to_string(),
        };
    }

    let drift = mtime_drift_secs(primary, &wal_path);

    let (state, detail) = if wal_bytes == 0 {
        (
            WalSidecarState::IdleEmpty,
            "0-byte WAL: benign idle/just-checkpointed state".to_string(),
        )
    } else if wal_bytes < SQLITE_WAL_HEADER_BYTES {
        // 1..=31 bytes: a partial header that cannot even be read. SQLite
        // surfaces this as "WAL file too small for header" — a genuine
        // truncation artifact.
        (
            WalSidecarState::HeaderOnlyOrTruncated,
            format!(
                "WAL is {wal_bytes} bytes (< 32-byte header): incomplete/truncated header, no committed frames"
            ),
        )
    } else {
        // >= 32 bytes: a complete header is present — validate its magic. A WAL
        // that is EXACTLY the 32-byte header with a valid magic is a frameless
        // idle WAL that SQLite opens without error (a freshly-created or
        // just-checkpointed WAL-mode DB sits here between writes — ts1 incident,
        // 2026-06-17). It is NOT a truncation artifact. Only an invalid-magic
        // 32-byte header (e.g. an all-zeros garbage WAL, GH#99) is malformed.
        match read_wal_header(&wal_path) {
            Some(header) => match wal_header_verdict(&header) {
                WalHeaderVerdict::Valid if wal_bytes == SQLITE_WAL_HEADER_BYTES => (
                    WalSidecarState::IdleEmpty,
                    "WAL is exactly the 32-byte header with a valid magic: frameless idle WAL, openable as-is".to_string(),
                ),
                WalHeaderVerdict::Valid => (
                    WalSidecarState::StaleNeedsCheckpoint,
                    format!("WAL ({wal_bytes} bytes) has a valid header and committed frames"),
                ),
                verdict => (
                    WalSidecarState::MalformedHeader,
                    format!("WAL ({wal_bytes} bytes) header invalid: {verdict:?}"),
                ),
            },
            None => (
                WalSidecarState::MalformedHeader,
                format!("WAL ({wal_bytes} bytes) header unreadable"),
            ),
        }
    };

    WalSidecarClassification {
        state,
        wal_bytes,
        shm_bytes,
        wal_mtime_drift_secs: drift,
        live_writer_present: false,
        detail,
    }
}

/// The safe action for a classification.
///
/// A live writer owning the DB overrides everything: we never mutate sidecars
/// (checkpoint or quarantine) under a live owner, because that is exactly the
/// situation that previously produced a malformed image.
#[must_use]
pub fn recommended_action(classification: &WalSidecarClassification) -> SidecarAction {
    if classification.live_writer_present {
        return SidecarAction::RefuseLiveWriter;
    }
    match classification.state {
        WalSidecarState::NoSidecar | WalSidecarState::IdleEmpty => SidecarAction::NoOp,
        WalSidecarState::StaleNeedsCheckpoint => SidecarAction::Checkpoint,
        WalSidecarState::HeaderOnlyOrTruncated | WalSidecarState::MalformedHeader => {
            SidecarAction::QuarantineWal
        }
        WalSidecarState::Symlink | WalSidecarState::Orphan => SidecarAction::QuarantineSidecar,
    }
}

/// Report from a [`safe_checkpoint`] attempt.
#[derive(Debug, Clone, Serialize)]
pub struct SafeCheckpointReport {
    pub classification: WalSidecarClassification,
    pub action: SidecarAction,
    pub checkpointed: bool,
    pub frames_checkpointed: u64,
    pub wal_bytes_before: u64,
    pub wal_bytes_after: u64,
}

/// Classify the WAL and checkpoint it **only when safe**.
///
/// Refuses (without mutating anything) when a live writer owns the database, and
/// checkpoints only a stale-but-valid WAL. Logs the WAL sidecar size before and
/// after every action so operators can see exactly what changed. Damaged
/// sidecars (header-only/truncated, malformed, symlink, orphan) are reported
/// with a quarantine recommendation but left in place — quarantine is the
/// caller's (doctor's) reversible, hash-witnessed operation, not this read-path.
pub fn safe_checkpoint(primary: &Path, storage_root: &Path) -> DbResult<SafeCheckpointReport> {
    let live_writer_present = inspect_mailbox_ownership(primary, storage_root).blocks_mutation();
    let mut classification = classify_wal_sidecar(primary);
    classification.live_writer_present = live_writer_present;
    let action = recommended_action(&classification);
    let wal_bytes_before = classification.wal_bytes;

    if action != SidecarAction::Checkpoint {
        tracing::info!(
            primary = %primary.display(),
            state = ?classification.state,
            action = ?action,
            wal_bytes_before,
            shm_bytes = classification.shm_bytes,
            mtime_drift_secs = classification.wal_mtime_drift_secs,
            live_writer_present,
            detail = %classification.detail,
            "wal safe-checkpoint: no checkpoint performed"
        );
        return Ok(SafeCheckpointReport {
            classification,
            action,
            checkpointed: false,
            frames_checkpointed: 0,
            wal_bytes_before,
            wal_bytes_after: wal_bytes_before,
        });
    }

    let frames_checkpointed = wal_checkpoint_truncate_path(primary)?;
    let wal_bytes_after = inspect_mailbox_sidecar_state(primary)
        .wal_bytes
        .unwrap_or(0);
    tracing::info!(
        primary = %primary.display(),
        frames_checkpointed,
        wal_bytes_before,
        wal_bytes_after,
        "wal safe-checkpoint: checkpointed stale WAL (no live writer present)"
    );
    Ok(SafeCheckpointReport {
        classification,
        action,
        checkpointed: true,
        frames_checkpointed,
        wal_bytes_before,
        wal_bytes_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a healthy WAL-mode database with a couple of rows. Returns the
    /// primary path inside `dir`.
    fn make_wal_db(dir: &Path) -> PathBuf {
        let primary = dir.join("storage.sqlite3");
        let conn = crate::DbConn::open_file(primary.display().to_string()).expect("open");
        conn.execute_raw("PRAGMA journal_mode = WAL;").expect("wal");
        conn.execute_raw("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
            .expect("schema");
        conn.execute_raw("INSERT INTO t (v) VALUES ('a'), ('b'), ('c');")
            .expect("seed");
        primary
    }

    #[test]
    fn classify_no_sidecar_when_wal_absent() {
        let dir = tempfile::tempdir().unwrap();
        let primary = make_wal_db(dir.path());
        // Checkpoint+truncate so the WAL is removed/zeroed, then assert idle.
        let _ = wal_checkpoint_truncate_path(&primary);
        let c = classify_wal_sidecar(&primary);
        assert!(
            matches!(
                c.state,
                WalSidecarState::NoSidecar | WalSidecarState::IdleEmpty
            ),
            "checkpointed DB should be NoSidecar/IdleEmpty, got {:?}",
            c.state
        );
        assert_eq!(recommended_action(&c), SidecarAction::NoOp);
    }

    #[test]
    fn classify_zero_byte_wal_is_idle() {
        let dir = tempfile::tempdir().unwrap();
        let primary = make_wal_db(dir.path());
        let _ = wal_checkpoint_truncate_path(&primary);
        // Force a 0-byte WAL beside the primary.
        std::fs::write(sidecar_path(&primary, "-wal"), b"").unwrap();
        let c = classify_wal_sidecar(&primary);
        assert_eq!(c.state, WalSidecarState::IdleEmpty);
        assert_eq!(c.wal_bytes, 0);
        assert_eq!(recommended_action(&c), SidecarAction::NoOp);
    }

    #[test]
    fn classify_header_only_or_truncated_wal() {
        let dir = tempfile::tempdir().unwrap();
        let primary = make_wal_db(dir.path());
        let _ = wal_checkpoint_truncate_path(&primary);
        // 16 bytes: non-empty but too small for the 32-byte header.
        std::fs::write(sidecar_path(&primary, "-wal"), [0xABu8; 16]).unwrap();
        let c = classify_wal_sidecar(&primary);
        assert_eq!(c.state, WalSidecarState::HeaderOnlyOrTruncated);
        assert_eq!(recommended_action(&c), SidecarAction::QuarantineWal);
    }

    #[test]
    fn classify_malformed_wal_bad_magic() {
        let dir = tempfile::tempdir().unwrap();
        let primary = make_wal_db(dir.path());
        let _ = wal_checkpoint_truncate_path(&primary);
        // >32 bytes but garbage magic.
        std::fs::write(sidecar_path(&primary, "-wal"), [0x00u8; 64]).unwrap();
        let c = classify_wal_sidecar(&primary);
        assert_eq!(c.state, WalSidecarState::MalformedHeader);
        assert_eq!(recommended_action(&c), SidecarAction::QuarantineWal);
    }

    #[test]
    fn classify_valid_32_byte_header_is_idle_not_truncated() {
        // ts1/css regression (2026-06-17): a valid engine-written 32-byte header
        // (zero frames) is a benign frameless idle WAL that the engine opens
        // as-is — it must classify as IdleEmpty (NoOp), NOT HeaderOnlyOrTruncated
        // (quarantine). Quarantining it false-failed `am doctor health` and
        // churned the startup self-heal on every restart.
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let header = {
            let conn = crate::DbConn::open_file(primary.display().to_string()).expect("open");
            conn.execute_raw("PRAGMA journal_mode = WAL;").expect("wal");
            conn.execute_raw("PRAGMA wal_autocheckpoint = 0;")
                .expect("no autockpt");
            conn.execute_raw("CREATE TABLE t (id INTEGER PRIMARY KEY);")
                .expect("schema");
            conn.execute_raw("INSERT INTO t (id) VALUES (1);")
                .expect("seed");
            let h = read_wal_header(&sidecar_path(&primary, "-wal")).expect("32-byte header");
            let _ = wal_checkpoint_truncate_path(&primary);
            h
        };
        std::fs::write(sidecar_path(&primary, "-wal"), header).expect("write 32-byte wal");
        let c = classify_wal_sidecar(&primary);
        assert_eq!(c.wal_bytes, 32);
        assert_eq!(
            c.state,
            WalSidecarState::IdleEmpty,
            "valid 32-byte header must be IdleEmpty: {}",
            c.detail
        );
        assert_eq!(recommended_action(&c), SidecarAction::NoOp);
        // And the magic-aware predicate must agree it is NOT a removable artifact.
        assert!(!wal_sidecar_is_truncation_artifact(&sidecar_path(
            &primary, "-wal"
        )));
    }

    #[test]
    fn classify_garbage_32_byte_header_is_malformed_and_quarantined() {
        // GH#99/#119: an all-zeros 32-byte WAL has an INVALID magic — it is a
        // genuine garbage header that trips the rebuild path and must still be
        // quarantined. The magic check is what separates it from the valid case.
        let dir = tempfile::tempdir().unwrap();
        let primary = make_wal_db(dir.path());
        let _ = wal_checkpoint_truncate_path(&primary);
        std::fs::write(sidecar_path(&primary, "-wal"), [0x00u8; 32]).unwrap();
        let c = classify_wal_sidecar(&primary);
        assert_eq!(c.wal_bytes, 32);
        assert_eq!(
            c.state,
            WalSidecarState::MalformedHeader,
            "garbage 32-byte header must be MalformedHeader: {}",
            c.detail
        );
        assert_eq!(recommended_action(&c), SidecarAction::QuarantineWal);
        assert!(wal_sidecar_is_truncation_artifact(&sidecar_path(
            &primary, "-wal"
        )));
    }

    #[test]
    fn wal_sidecar_is_truncation_artifact_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let primary = make_wal_db(dir.path());
        let _ = wal_checkpoint_truncate_path(&primary);
        let wal = sidecar_path(&primary, "-wal");

        // 0 bytes → valid idle, not an artifact.
        std::fs::write(&wal, b"").unwrap();
        assert!(!wal_sidecar_is_truncation_artifact(&wal));
        // 1..=31 bytes → incomplete header, a genuine artifact.
        std::fs::write(&wal, [0xABu8; 16]).unwrap();
        assert!(wal_sidecar_is_truncation_artifact(&wal));
        std::fs::write(&wal, [0xABu8; 31]).unwrap();
        assert!(wal_sidecar_is_truncation_artifact(&wal));
        // 32 bytes, garbage magic → artifact.
        std::fs::write(&wal, [0x00u8; 32]).unwrap();
        assert!(wal_sidecar_is_truncation_artifact(&wal));
        // >32 bytes (garbage) → NOT a "header-only" artifact (handled elsewhere).
        std::fs::write(&wal, [0x00u8; 64]).unwrap();
        assert!(!wal_sidecar_is_truncation_artifact(&wal));
        // Missing WAL → not an artifact.
        std::fs::remove_file(&wal).unwrap();
        assert!(!wal_sidecar_is_truncation_artifact(&wal));
    }

    #[test]
    fn classify_stale_wal_with_real_committed_frames() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let conn = crate::DbConn::open_file(primary.display().to_string()).expect("open");
        conn.execute_raw("PRAGMA journal_mode = WAL;").expect("wal");
        conn.execute_raw("PRAGMA wal_autocheckpoint = 0;")
            .expect("disable autockpt");
        conn.execute_raw("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
            .expect("schema");
        conn.execute_raw("INSERT INTO t (v) VALUES ('x'), ('y');")
            .expect("seed");
        // Keep the connection open so the WAL is not checkpointed on close.
        let c = classify_wal_sidecar(&primary);
        drop(conn);
        // A real WAL-mode write leaves committed frames in a valid WAL.
        assert_eq!(
            c.state,
            WalSidecarState::StaleNeedsCheckpoint,
            "a real committed WAL should validate (magic + checksum): {}",
            c.detail
        );
        assert!(c.wal_bytes > SQLITE_WAL_HEADER_BYTES);
        assert_eq!(recommended_action(&c), SidecarAction::Checkpoint);
    }

    #[test]
    fn wal_header_verdict_accepts_a_real_wal_header() {
        // De-risks the checksum implementation against a genuine engine-written
        // WAL: it must validate (magic + header checksum) as Valid.
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let conn = crate::DbConn::open_file(primary.display().to_string()).expect("open");
        conn.execute_raw("PRAGMA journal_mode = WAL;").expect("wal");
        conn.execute_raw("PRAGMA wal_autocheckpoint = 0;")
            .expect("disable autockpt");
        conn.execute_raw("CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .expect("schema");
        conn.execute_raw("INSERT INTO t (id) VALUES (1);")
            .expect("seed");
        let header = read_wal_header(&sidecar_path(&primary, "-wal"))
            .expect("real WAL should have a 32-byte header");
        drop(conn);
        assert_eq!(
            wal_header_verdict(&header),
            WalHeaderVerdict::Valid,
            "engine-written WAL header must pass magic + checksum validation"
        );
    }

    #[test]
    fn wal_header_verdict_rejects_bad_magic_and_short() {
        assert_eq!(wal_header_verdict(&[0u8; 16]), WalHeaderVerdict::TooShort);
        assert_eq!(wal_header_verdict(&[0u8; 32]), WalHeaderVerdict::BadMagic);
    }

    #[test]
    fn engine_opens_and_checkpoints_a_32_byte_header_only_wal() {
        // EMPIRICAL probe (ts1 incident, 2026-06-17): a live 0.3.13 server creates
        // a 32-byte idle WAL and reopens it cleanly across restarts, yet
        // `am doctor health` false-failed ("needs repair: WAL sidecar
        // header-only/truncated (32 bytes)"). The historical GH#99/#119 workaround
        // quarantines any <=32-byte WAL because the engine once tripped "WAL file
        // too small for header during rebuild". This test pins the CURRENT engine
        // behavior: it must OPEN, QUERY, and CHECKPOINT a DB whose WAL is exactly
        // the engine-written 32-byte header (zero frames) without error. If this
        // ever regresses, the classification change below is unsafe.
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");

        // 1. Build a real WAL-mode DB, capture an engine-written 32-byte header,
        //    then checkpoint+truncate so all committed data lives in the primary.
        let header = {
            let conn = crate::DbConn::open_file(primary.display().to_string()).expect("open");
            conn.execute_raw("PRAGMA journal_mode = WAL;").expect("wal");
            conn.execute_raw("PRAGMA wal_autocheckpoint = 0;")
                .expect("disable autockpt");
            conn.execute_raw("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
                .expect("schema");
            conn.execute_raw("INSERT INTO t (v) VALUES ('a'), ('b');")
                .expect("seed");
            let h = read_wal_header(&sidecar_path(&primary, "-wal"))
                .expect("a real WAL must carry a 32-byte header");
            let _ = wal_checkpoint_truncate_path(&primary);
            h
        };
        assert_eq!(
            header.len(),
            32,
            "engine WAL header must be exactly 32 bytes"
        );

        // 2. Lay down EXACTLY the 32-byte header as the WAL (valid magic, 0 frames).
        std::fs::write(sidecar_path(&primary, "-wal"), header).expect("write 32-byte wal");

        // 3. The engine must OPEN + QUERY + CHECKPOINT it without the GH#99/#119
        //    "WAL file too small for header" error.
        let conn = crate::DbConn::open_file(primary.display().to_string())
            .expect("engine must open a DB with a 32-byte header-only WAL");
        let rows = conn
            .query_sync("SELECT v FROM t ORDER BY id", &[])
            .expect("query over a 32-byte-WAL DB must succeed");
        assert_eq!(rows.len(), 2, "committed rows must remain readable");
        conn.execute_raw("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("checkpoint over a 32-byte header-only WAL must succeed");
    }

    #[test]
    fn wal_header_verdict_detects_corrupted_body_under_valid_magic() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let conn = crate::DbConn::open_file(primary.display().to_string()).expect("open");
        conn.execute_raw("PRAGMA journal_mode = WAL;").expect("wal");
        conn.execute_raw("PRAGMA wal_autocheckpoint = 0;")
            .expect("disable autockpt");
        conn.execute_raw("CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .expect("schema");
        conn.execute_raw("INSERT INTO t (id) VALUES (1);")
            .expect("seed");
        let mut header = read_wal_header(&sidecar_path(&primary, "-wal")).expect("header");
        drop(conn);
        // Flip a body byte (page-size field) but keep the magic intact → checksum mismatch.
        header[10] ^= 0xFF;
        assert_eq!(
            wal_header_verdict(&header),
            WalHeaderVerdict::ChecksumMismatch,
            "valid magic + corrupted body must fail the header checksum"
        );
    }

    #[cfg(unix)]
    #[test]
    fn classify_symlinked_wal_is_flagged() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let primary = make_wal_db(dir.path());
        let _ = wal_checkpoint_truncate_path(&primary);
        let target = dir.path().join("elsewhere.bin");
        std::fs::write(&target, b"not a wal").unwrap();
        let wal = sidecar_path(&primary, "-wal");
        let _ = std::fs::remove_file(&wal);
        symlink(&target, &wal).unwrap();
        let c = classify_wal_sidecar(&primary);
        assert_eq!(c.state, WalSidecarState::Symlink);
        assert_eq!(recommended_action(&c), SidecarAction::QuarantineSidecar);
    }

    #[test]
    fn classify_orphan_sidecar_without_primary() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("gone.sqlite3");
        // No primary; a WAL lingers.
        std::fs::write(sidecar_path(&primary, "-wal"), [0u8; 64]).unwrap();
        let c = classify_wal_sidecar(&primary);
        assert_eq!(c.state, WalSidecarState::Orphan);
        assert_eq!(recommended_action(&c), SidecarAction::QuarantineSidecar);
    }

    #[test]
    fn live_writer_overrides_to_refuse() {
        // The classification's state is irrelevant once a live writer owns the
        // DB: the recommended action must be RefuseLiveWriter.
        let classification = WalSidecarClassification {
            state: WalSidecarState::StaleNeedsCheckpoint,
            wal_bytes: 4096,
            shm_bytes: 32768,
            wal_mtime_drift_secs: Some(5),
            live_writer_present: true,
            detail: "stale but live-owned".to_string(),
        };
        assert_eq!(
            recommended_action(&classification),
            SidecarAction::RefuseLiveWriter
        );
    }

    #[test]
    fn safe_checkpoint_checkpoints_stale_wal_when_unowned() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let conn = crate::DbConn::open_file(primary.display().to_string()).expect("open");
        conn.execute_raw("PRAGMA journal_mode = WAL;").expect("wal");
        conn.execute_raw("PRAGMA wal_autocheckpoint = 0;")
            .expect("disable autockpt");
        conn.execute_raw("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
            .expect("schema");
        conn.execute_raw("INSERT INTO t (v) VALUES ('a'), ('b');")
            .expect("seed");
        drop(conn);

        let report = safe_checkpoint(&primary, dir.path()).expect("safe_checkpoint");
        // No live writer in a unit test → the stale WAL is checkpointed and shrinks.
        assert!(!report.classification.live_writer_present);
        assert_eq!(report.action, SidecarAction::Checkpoint);
        assert!(report.checkpointed);
        assert!(
            report.wal_bytes_after <= report.wal_bytes_before,
            "checkpoint should not grow the WAL: before={} after={}",
            report.wal_bytes_before,
            report.wal_bytes_after
        );
    }
}
