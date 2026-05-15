//! `fm-db-state-files-wal-shm-sidecar-drift` — P0.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! The WAL/SHM sidecars next to `storage.sqlite3` have drifted
//! away from the canonical "main + WAL + SHM as a coherent set"
//! invariant. Four distinct signals, each P0 because each
//! indicates the next `am serve` boot will fail or silently lose
//! data:
//!
//! 1. **Asymmetric**: WAL exists without SHM, or vice versa.
//!    SQLite recreates both on open, but a half-present pair
//!    usually means a crash mid-checkpoint or a manual delete
//!    of one half — the next open may replay incorrectly.
//! 2. **Header-only WAL**: WAL file is ≤ 32 bytes (only the
//!    file header, no committed frames). If `am serve` is dead
//!    AND the WAL has been at 32 bytes for long enough that
//!    no live writer would have caused it, it's leftover from
//!    a crashed run.
//! 3. **Stale WAL**: DB mtime is more than 24h ahead of WAL
//!    mtime. The WAL is supposed to be the most-recent surface
//!    SQLite writes to; if main is newer, an external process
//!    (Python writer, manual SQL) touched the DB without
//!    routing through WAL — corruption likely.
//! 4. **Quarantine pile-up**: more than 3
//!    `.cleanup-quarantine-*` or `.startup-quarantine-*` files
//!    in `storage_root` mtime'd in the last 24h. The boot path
//!    quarantines stale sidecars on startup; >3 in 24h means
//!    something is recreating the conditions repeatedly
//!    (typically a co-resident Python writer — see
//!    `fm-db-state-files-python-server-coresident-write`).
//!
//! ## Detection (pure function)
//!
//! All probes are pure filesystem reads (`symlink_metadata`,
//! `read_dir`). No DB connections, no shell-outs. The detector
//! returns at most ONE finding per DB candidate, with a
//! `signals: Vec<Signal>` listing every triggering condition —
//! operators see all drift dimensions in one shot rather than
//! getting 4 separate findings to triage.
//!
//! ## Fix
//!
//! **Detect-only.** Recovery is a sequence of operator-side
//! decisions: stop `am serve` (so no live writer races), then
//! decide per-signal whether to checkpoint via
//! `PRAGMA wal_checkpoint(TRUNCATE)` (header-only WAL),
//! quarantine the sidecars via `mv -v ...` (asymmetric / stale),
//! or investigate a Python co-resident writer (quarantine pile-
//! up). Auto-fix would require multiple Op variants in sequence
//! gated on operator consent; we'd rather operators audit each
//! signal manually.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const FM_ID: &str = "fm-db-state-files-wal-shm-sidecar-drift";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "db_state_files";

/// 24-hour drift threshold (seconds) for the "WAL much older
/// than DB" signal. Operators with intentionally-long-lived
/// snapshots can lower this via test inputs.
pub const DEFAULT_STALE_WAL_SECS: u64 = 24 * 3600;

/// Quarantine pile-up threshold — anything above this means the
/// boot path's quarantine pile is growing rather than churning.
pub const DEFAULT_QUARANTINE_PILEUP_MAX: usize = 3;

/// Header-only WAL: SQLite writes a 32-byte header + frames; a
/// WAL at exactly the header size has zero committed frames.
pub const WAL_HEADER_BYTES: u64 = 32;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum Signal {
    /// WAL exists but SHM doesn't (or vice versa).
    AsymmetricSidecars { wal_exists: bool, shm_exists: bool },
    /// WAL file size ≤ `WAL_HEADER_BYTES` — no committed frames.
    HeaderOnlyWal { wal_size_bytes: u64 },
    /// DB mtime is `drift_secs` ahead of WAL mtime; threshold
    /// crossed.
    StaleWal {
        db_mtime_secs: u64,
        wal_mtime_secs: u64,
        drift_secs: u64,
    },
    /// More than `count` `.cleanup-quarantine-*` /
    /// `.startup-quarantine-*` files mtime'd in the last 24h
    /// in the same storage dir.
    QuarantinePileUp { count: usize, threshold: usize },
}

impl Signal {
    fn as_kebab(&self) -> &'static str {
        match self {
            Signal::AsymmetricSidecars { .. } => "asymmetric_sidecars",
            Signal::HeaderOnlyWal { .. } => "header_only_wal",
            Signal::StaleWal { .. } => "stale_wal",
            Signal::QuarantinePileUp { .. } => "quarantine_pile_up",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WalShmSidecarDriftFinding {
    pub db_path: PathBuf,
    /// Every drift signal triggered for this DB. At least one;
    /// often two or more co-occur (e.g., header-only WAL +
    /// stale WAL on a long-dead `am serve`).
    pub signals: Vec<Signal>,
}

impl WalShmSidecarDriftFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "WAL/SHM sidecars next to {} show drift: {}",
            self.db_path.display(),
            self.signals
                .iter()
                .map(|s| s.as_kebab())
                .collect::<Vec<_>>()
                .join(", "),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "signals": self.signals,
                "stale_wal_threshold_secs": DEFAULT_STALE_WAL_SECS,
                "quarantine_pile_up_threshold": DEFAULT_QUARANTINE_PILEUP_MAX,
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        let mut steps = vec!["Stop `am serve` (so no live writer races your repair).".to_string()];
        for s in &self.signals {
            match s {
                Signal::AsymmetricSidecars { .. } => steps.push(
                    "Asymmetric WAL/SHM: quarantine BOTH `storage.sqlite3-wal` and \
                     `storage.sqlite3-shm` via `mv -v ... .bak.$(date +%s)`. SQLite will \
                     recreate both on next open from the main DB's checkpoint."
                        .to_string(),
                ),
                Signal::HeaderOnlyWal { wal_size_bytes } => steps.push(format!(
                    "Header-only WAL ({wal_size_bytes} bytes): no committed frames. \
                     Quarantine `storage.sqlite3-wal` via `mv -v ... .bak.$(date +%s)`."
                )),
                Signal::StaleWal { drift_secs, .. } => steps.push(format!(
                    "Stale WAL ({drift_secs}s older than main): an external writer touched \
                     the main DB without WAL. Investigate Python writer (see \
                     `fm-db-state-files-python-server-coresident-write`) and then quarantine \
                     the stale WAL."
                )),
                Signal::QuarantinePileUp { count, threshold } => steps.push(format!(
                    "Quarantine pile-up ({count} files in 24h, threshold {threshold}): boot \
                     path is repeatedly quarantining and a co-resident writer is recreating \
                     the conditions. Stop the Python writer; investigate `am robot agents` \
                     for stale processes."
                )),
            }
        }
        format!(
            "DB {} has WAL/SHM drift. Recovery steps:\n  • {}",
            self.db_path.display(),
            steps.join("\n  • "),
        )
    }
}

/// Detector inputs. `stale_wal_threshold_secs` and
/// `quarantine_pile_up_threshold` are overridable for tests.
#[derive(Debug, Clone)]
pub struct DetectInputs {
    pub db_candidates: Vec<PathBuf>,
    pub stale_wal_threshold_secs: u64,
    pub quarantine_pile_up_threshold: usize,
    /// Override "now" for deterministic stale-WAL detection in
    /// tests. `None` uses `SystemTime::now()`.
    pub now_override: Option<SystemTime>,
}

impl DetectInputs {
    pub fn new(db_candidates: Vec<PathBuf>) -> Self {
        Self {
            db_candidates,
            stale_wal_threshold_secs: DEFAULT_STALE_WAL_SECS,
            quarantine_pile_up_threshold: DEFAULT_QUARANTINE_PILEUP_MAX,
            now_override: None,
        }
    }
}

/// Detector. PURE — all filesystem reads, no DB connections, no
/// shell-outs.
pub fn detect(inputs: &DetectInputs) -> Vec<WalShmSidecarDriftFinding> {
    let mut out = Vec::new();
    for db in &inputs.db_candidates {
        let mut signals = Vec::new();

        let wal = sidecar_path(db, "-wal");
        let shm = sidecar_path(db, "-shm");
        let db_meta = std::fs::symlink_metadata(db);
        let wal_meta = std::fs::symlink_metadata(&wal);
        let shm_meta = std::fs::symlink_metadata(&shm);

        // Skip if no DB at all (sibling FM owns "missing main DB").
        if db_meta.is_err() {
            continue;
        }

        let wal_exists = wal_meta.is_ok();
        let shm_exists = shm_meta.is_ok();
        if wal_exists != shm_exists {
            signals.push(Signal::AsymmetricSidecars {
                wal_exists,
                shm_exists,
            });
        }
        if let Ok(meta) = &wal_meta
            && meta.file_type().is_file()
            && meta.len() <= WAL_HEADER_BYTES
        {
            signals.push(Signal::HeaderOnlyWal {
                wal_size_bytes: meta.len(),
            });
        }
        if let (Ok(d), Ok(w)) = (&db_meta, &wal_meta)
            && let (Ok(d_mtime), Ok(w_mtime)) = (d.modified(), w.modified())
            && let Ok(diff) = d_mtime.duration_since(w_mtime)
            && diff.as_secs() > inputs.stale_wal_threshold_secs
        {
            let now = inputs.now_override.unwrap_or_else(SystemTime::now);
            signals.push(Signal::StaleWal {
                db_mtime_secs: epoch_secs(now, d.modified().unwrap_or(now)),
                wal_mtime_secs: epoch_secs(now, w.modified().unwrap_or(now)),
                drift_secs: diff.as_secs(),
            });
        }
        // Quarantine pile-up: scan the DB's parent dir.
        if let Some(parent) = db.parent() {
            let count = count_recent_quarantine_files(parent, inputs.now_override);
            if count > inputs.quarantine_pile_up_threshold {
                signals.push(Signal::QuarantinePileUp {
                    count,
                    threshold: inputs.quarantine_pile_up_threshold,
                });
            }
        }

        if !signals.is_empty() {
            out.push(WalShmSidecarDriftFinding {
                db_path: db.clone(),
                signals,
            });
        }
    }
    out
}

fn sidecar_path(db: &Path, suffix: &str) -> PathBuf {
    let parent = db.parent().unwrap_or_else(|| Path::new("."));
    let name = db
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    parent.join(format!("{name}{suffix}"))
}

fn epoch_secs(now: SystemTime, t: SystemTime) -> u64 {
    let _ = now;
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Count files in `dir` matching `.cleanup-quarantine-*` or
/// `.startup-quarantine-*` whose mtime is within the last 24h.
fn count_recent_quarantine_files(dir: &Path, now_override: Option<SystemTime>) -> usize {
    let now = now_override.unwrap_or_else(SystemTime::now);
    let cutoff = now - Duration::from_secs(DEFAULT_STALE_WAL_SECS);
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let mut count = 0;
    for entry in read.flatten() {
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        let matches =
            name_s.contains(".cleanup-quarantine-") || name_s.contains(".startup-quarantine-");
        if !matches {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && let Ok(mtime) = meta.modified()
            && mtime > cutoff
        {
            count += 1;
        }
    }
    count
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &WalShmSidecarDriftFinding,
) -> Result<FixOutcome, MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_db(td: &TempDir) -> PathBuf {
        let p = td.path().join("storage.sqlite3");
        fs::write(&p, b"SQLite format 3\0").unwrap();
        p
    }

    fn touch(p: &Path, content: &[u8]) {
        fs::write(p, content).unwrap();
    }

    #[test]
    fn detector_returns_empty_for_healthy_pair() {
        let td = TempDir::new().unwrap();
        let db = make_db(&td);
        // Healthy WAL has at least one frame (size > 32).
        touch(&td.path().join("storage.sqlite3-wal"), &[0u8; 4096]);
        touch(&td.path().join("storage.sqlite3-shm"), &[0u8; 32768]);
        let findings = detect(&DetectInputs::new(vec![db]));
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_flags_asymmetric_wal_without_shm() {
        let td = TempDir::new().unwrap();
        let db = make_db(&td);
        touch(&td.path().join("storage.sqlite3-wal"), &[0u8; 4096]);
        // SHM intentionally missing.
        let findings = detect(&DetectInputs::new(vec![db]));
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0].signals[0],
            Signal::AsymmetricSidecars {
                wal_exists: true,
                shm_exists: false,
            }
        ));
    }

    #[test]
    fn detector_flags_header_only_wal() {
        let td = TempDir::new().unwrap();
        let db = make_db(&td);
        touch(&td.path().join("storage.sqlite3-wal"), &[0u8; 32]);
        touch(&td.path().join("storage.sqlite3-shm"), &[0u8; 32768]);
        let findings = detect(&DetectInputs::new(vec![db]));
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .signals
                .iter()
                .any(|s| matches!(s, Signal::HeaderOnlyWal { wal_size_bytes: 32 }))
        );
    }

    #[test]
    fn detector_flags_quarantine_pile_up() {
        let td = TempDir::new().unwrap();
        let db = make_db(&td);
        touch(&td.path().join("storage.sqlite3-wal"), &[0u8; 4096]);
        touch(&td.path().join("storage.sqlite3-shm"), &[0u8; 32768]);
        // Four quarantine files in the same dir — over threshold.
        for i in 0..4 {
            fs::write(td.path().join(format!(".cleanup-quarantine-{i}")), b"x").unwrap();
        }
        let inputs = DetectInputs::new(vec![db]);
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].signals.iter().any(|s| matches!(
            s,
            Signal::QuarantinePileUp {
                count: 4,
                threshold: 3
            }
        )));
    }

    #[test]
    fn detector_skips_missing_db() {
        let td = TempDir::new().unwrap();
        let findings = detect(&DetectInputs::new(vec![td.path().join("nope.sqlite3")]));
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_healthy_when_threshold_lowered_does_not_create_false_positives() {
        // Threshold smaller than reality still won't fire if there
        // are no quarantine files at all.
        let td = TempDir::new().unwrap();
        let db = make_db(&td);
        touch(&td.path().join("storage.sqlite3-wal"), &[0u8; 4096]);
        touch(&td.path().join("storage.sqlite3-shm"), &[0u8; 32768]);
        let mut inputs = DetectInputs::new(vec![db]);
        inputs.quarantine_pile_up_threshold = 0; // even at 0, no files → no finding.
        let findings = detect(&inputs);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_aggregates_multiple_signals_into_one_finding() {
        // Header-only WAL + quarantine pile-up at the same time.
        let td = TempDir::new().unwrap();
        let db = make_db(&td);
        touch(&td.path().join("storage.sqlite3-wal"), &[0u8; 32]);
        touch(&td.path().join("storage.sqlite3-shm"), &[0u8; 32768]);
        for i in 0..5 {
            fs::write(td.path().join(format!(".startup-quarantine-{i}")), b"x").unwrap();
        }
        let findings = detect(&DetectInputs::new(vec![db]));
        assert_eq!(findings.len(), 1, "one aggregated finding");
        assert!(findings[0].signals.len() >= 2);
    }

    #[test]
    fn finding_severity_is_p0_detect_only() {
        let f = WalShmSidecarDriftFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            signals: vec![Signal::HeaderOnlyWal { wal_size_bytes: 32 }],
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P0");
        assert!(!g.remediation.auto_fixable);
    }

    #[test]
    fn manual_remediation_includes_stop_serve_step() {
        let f = WalShmSidecarDriftFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            signals: vec![Signal::QuarantinePileUp {
                count: 7,
                threshold: 3,
            }],
        };
        let text = f.manual_remediation_text();
        assert!(text.contains("Stop `am serve`"));
        assert!(text.contains("7"));
    }
}
