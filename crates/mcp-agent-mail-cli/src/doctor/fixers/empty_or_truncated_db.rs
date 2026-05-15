//! `fm-db-state-files-empty-or-truncated-db` — P0.
//!
//! **Subsystem**: db_state_files (Phase 1 archaeology — HANDOFF
//! P3-C #4 ranking).
//!
//! ## What's broken
//!
//! `storage.sqlite3` is too small to be a valid SQLite database
//! OR fails `PRAGMA quick_check`. Indicates partial-write
//! corruption (truncated by `fs::write` mid-stream, a kernel
//! crash during DB grow, or a manual `> storage.sqlite3` shell
//! redirect that wiped the file). Either case loses every
//! message body, agent identity, and contact graph in the DB.
//!
//! This is P0 — the DB is the canonical store. The Rust pool
//! refuses to open a malformed DB; the doctor's job is to
//! detect the state and point the operator at recovery.
//!
//! ## Detection (pure function)
//!
//! 1. `fs::metadata(path)` — if file doesn't exist OR size is
//!    below the SQLite header minimum (100 bytes), emit
//!    `Reason::TooSmall { size }`.
//! 2. Open via `SqliteConnection::open_file`. If open fails,
//!    emit `Reason::OpenFailed { error }`.
//! 3. `PRAGMA quick_check;`. If the result is anything other
//!    than `"ok"`, emit `Reason::QuickCheckFailed { result }`.
//! 4. Otherwise no finding.
//!
//! ## Fix
//!
//! **None.** Doctor cannot rebuild a corrupted SQLite file
//! deterministically without operator intervention. The finding
//! emits a `manual_remediation` envelope pointing operators at
//! `am doctor reconstruct --json` which walks the Git archive
//! and rebuilds the DB from message files.
//!
//! `auto_fixable: false` (detect-only); fix() is a no-op for
//! API uniformity.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-empty-or-truncated-db";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "db_state_files";

/// SQLite file format requires at least a 100-byte header.
/// Anything smaller is necessarily corrupt or empty.
pub const SQLITE_HEADER_BYTES: u64 = 100;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum Reason {
    /// File doesn't exist on disk.
    Missing,
    /// File exists but is smaller than the SQLite header.
    TooSmall {
        size: u64,
    },
    /// `SqliteConnection::open_file` failed.
    OpenFailed {
        message: String,
    },
    /// `PRAGMA quick_check` returned something other than "ok".
    QuickCheckFailed {
        result: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct EmptyOrTruncatedDbFinding {
    pub db_path: PathBuf,
    pub reason: Reason,
}

impl EmptyOrTruncatedDbFinding {
    pub fn to_finding(&self) -> super::Finding {
        let reason_str = match &self.reason {
            Reason::Missing => "missing".to_string(),
            Reason::TooSmall { size } => format!("too_small (size={size})"),
            Reason::OpenFailed { message } => format!("open_failed ({message})"),
            Reason::QuickCheckFailed { result } => format!("quick_check_failed ({result})"),
        };
        let title = format!(
            "DB {} is empty or corrupted ({reason_str}); recover via `am doctor reconstruct`",
            self.db_path.display()
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "reason": self.reason,
                "sqlite_header_min_bytes": SQLITE_HEADER_BYTES,
                "recovery_command": "am doctor reconstruct --json",
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// Detector. PURE w.r.t. caller-supplied paths; opens each
/// candidate via `SqliteConnection` to run `PRAGMA quick_check`.
///
/// Skips paths that aren't on disk as a regular file (e.g.,
/// `:memory:` placeholders, symlinks, dirs). The caller's
/// `default_db_file_candidates()` helper already filters those
/// out for the canonical CLI path.
pub fn detect(candidate_paths: &[PathBuf]) -> Vec<EmptyOrTruncatedDbFinding> {
    use sqlmodel_sqlite::SqliteConnection;
    let mut out = Vec::new();
    for path in candidate_paths {
        // Stage 1: file presence + size.
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => {
                out.push(EmptyOrTruncatedDbFinding {
                    db_path: path.clone(),
                    reason: Reason::Missing,
                });
                continue;
            }
        };
        if !meta.file_type().is_file() {
            continue; // symlink / dir / device — not our domain
        }
        if meta.len() < SQLITE_HEADER_BYTES {
            out.push(EmptyOrTruncatedDbFinding {
                db_path: path.clone(),
                reason: Reason::TooSmall { size: meta.len() },
            });
            continue;
        }
        // Stage 2: open via SqliteConnection.
        let conn = match SqliteConnection::open_file(path.to_string_lossy().into_owned()) {
            Ok(c) => c,
            Err(e) => {
                out.push(EmptyOrTruncatedDbFinding {
                    db_path: path.clone(),
                    reason: Reason::OpenFailed {
                        message: format!("{e}"),
                    },
                });
                continue;
            }
        };
        // Stage 3: PRAGMA quick_check.
        let result = quick_check(&conn).unwrap_or_else(|| "<query_failed>".to_string());
        drop(conn);
        if !result.eq_ignore_ascii_case("ok") {
            out.push(EmptyOrTruncatedDbFinding {
                db_path: path.clone(),
                reason: Reason::QuickCheckFailed { result },
            });
        }
    }
    out
}

fn quick_check(conn: &sqlmodel_sqlite::SqliteConnection) -> Option<String> {
    let rows = conn.query_sync("PRAGMA quick_check;", &[]).ok()?;
    let first = rows.first()?;
    first.get_named::<String>("quick_check").ok()
}

/// Detect-only FM. `fix()` is a no-op for API uniformity.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &EmptyOrTruncatedDbFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlmodel_sqlite::SqliteConnection;
    use std::fs;
    use tempfile::TempDir;

    fn make_healthy_db(td: &TempDir, name: &str) -> PathBuf {
        let p = td.path().join(name);
        let conn = SqliteConnection::open_file(p.to_string_lossy().into_owned())
            .expect("open new sqlite db");
        conn.execute_raw("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);")
            .expect("create + insert");
        drop(conn);
        p
    }

    #[test]
    fn detector_returns_empty_for_healthy_db() {
        let td = TempDir::new().unwrap();
        let db = make_healthy_db(&td, "good.sqlite3");
        let findings = detect(&[db]);
        assert!(findings.is_empty(), "healthy DB must not flag");
    }

    #[test]
    fn detector_flags_missing_file() {
        let td = TempDir::new().unwrap();
        let findings = detect(&[td.path().join("nope.sqlite3")]);
        assert_eq!(findings.len(), 1);
        assert!(matches!(findings[0].reason, Reason::Missing));
    }

    #[test]
    fn detector_flags_truncated_file() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("truncated.sqlite3");
        // Smaller than the 100-byte SQLite header.
        fs::write(&p, b"not a real sqlite header").unwrap();
        let findings = detect(std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);
        match &findings[0].reason {
            Reason::TooSmall { size } => assert!(*size < SQLITE_HEADER_BYTES),
            other => panic!("expected TooSmall, got {other:?}"),
        }
    }

    #[test]
    fn detector_flags_invalid_sqlite_header_above_min_size() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("garbage.sqlite3");
        // Above the 100-byte minimum but not a real SQLite header.
        // Open may or may not fail depending on lazy-vs-eager
        // header validation; quick_check is what catches it.
        fs::write(&p, vec![0xFF_u8; 200]).unwrap();
        let findings = detect(std::slice::from_ref(&p));
        assert_eq!(
            findings.len(),
            1,
            "garbage-content file must flag (got: {findings:?})"
        );
        // Acceptable: OpenFailed OR QuickCheckFailed.
        assert!(
            matches!(
                findings[0].reason,
                Reason::OpenFailed { .. } | Reason::QuickCheckFailed { .. }
            ),
            "unexpected reason: {:?}",
            findings[0].reason
        );
    }

    #[test]
    fn finding_is_p0_detect_only_with_recovery_command() {
        let f = EmptyOrTruncatedDbFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            reason: Reason::TooSmall { size: 0 },
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P0");
        assert_eq!(g.subsystem, "db_state_files");
        assert!(!g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 0);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("am doctor reconstruct"));
    }
}
