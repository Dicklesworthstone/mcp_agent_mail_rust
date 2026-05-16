//! `fm-db-state-files-schema-version-mismatch` — P0 / P1.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! `PRAGMA user_version` on `storage.sqlite3` does NOT match the
//! Rust binary's compiled `mcp_agent_mail_db::schema::SCHEMA_VERSION`.
//!
//! Two cases with different severity:
//! - **On-disk version < compiled** (`Direction::ForwardMigrate`):
//!   the DB needs forward migration. Severity **P0** — startup
//!   would normally run the migration, but if the doctor sees
//!   this without a live `am serve`, an operator did something
//!   unusual (downgraded the binary mid-deploy, restored an old
//!   backup, etc.) and the next `am serve` boot will migrate.
//! - **On-disk version > compiled** (`Direction::Newer`): the
//!   DB was written by a newer Rust binary than the one currently
//!   on disk. Severity **P1** — auto-fix is impossible because
//!   we don't carry backward migrations. The operator must
//!   upgrade the binary or restore an older DB.
//!
//! `.no-migrate` marker at `<storage_root>/.no-migrate`: an
//! operator opt-out signal. When present, the finding still
//! emits (for visibility) but the evidence records that the
//! marker is intentional.
//!
//! ## Detection (pure function)
//!
//! 1. Open `storage.sqlite3` read-only.
//! 2. `SELECT * FROM pragma_user_version` (or `PRAGMA user_version;`).
//! 3. Compare with `mcp_agent_mail_db::schema::SCHEMA_VERSION`.
//! 4. If equal: no finding.
//! 5. Else: emit a finding with `Direction` + `.no-migrate`
//!    marker state.
//!
//! ## Fix
//!
//! **Detect-only.** Forward-migrate scenarios are handled by the
//! boot-time migration path inside `mcp-agent-mail serve`; doctor
//! intentionally does not duplicate that logic because running
//! the migration SQL through `Op::DbExec` while a live server is
//! starting up would race. Backward (newer-on-disk) scenarios
//! have no automated fix path — the operator must upgrade the
//! binary or restore an older DB.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use mcp_agent_mail_db::schema::SCHEMA_VERSION;
use serde::Serialize;
use sqlmodel_sqlite::{OpenFlags, SqliteConfig, SqliteConnection};
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-schema-version-mismatch";
const FM_SUBSYSTEM: &str = "db_state_files";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum Direction {
    /// `on_disk < compiled` — forward migration would catch us up.
    /// Severity P0.
    ForwardMigrate,
    /// `on_disk > compiled` — DB was written by a newer binary.
    /// Severity P1; no auto-fix.
    Newer,
}

impl Direction {
    fn as_kebab(self) -> &'static str {
        match self {
            Direction::ForwardMigrate => "forward_migrate_needed",
            Direction::Newer => "newer_than_binary",
        }
    }

    fn severity(self) -> &'static str {
        match self {
            Direction::ForwardMigrate => "P0",
            Direction::Newer => "P1",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaVersionMismatchFinding {
    pub db_path: PathBuf,
    pub on_disk_version: i32,
    pub compiled_version: i32,
    pub direction: Direction,
    /// Whether `<storage_root>/.no-migrate` exists. Operator
    /// opt-out signal — surfaced for evidence but doesn't suppress
    /// the finding (visibility > inferred intent).
    pub no_migrate_marker_present: bool,
}

impl SchemaVersionMismatchFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "schema_version mismatch in {}: on_disk={} compiled={} ({})",
            self.db_path.display(),
            self.on_disk_version,
            self.compiled_version,
            self.direction.as_kebab(),
        );
        super::Finding {
            id: FM_ID,
            severity: self.direction.severity(),
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "on_disk_user_version": self.on_disk_version,
                "compiled_schema_version": self.compiled_version,
                "direction": self.direction.as_kebab(),
                "no_migrate_marker_present": self.no_migrate_marker_present,
                "recovery_paths": match self.direction {
                    Direction::ForwardMigrate => serde_json::json!([
                        "Restart `am serve` (boot migration runs forward).",
                        "Or run `am doctor reconstruct` to rebuild from the git archive.",
                    ]),
                    Direction::Newer => serde_json::json!([
                        "Upgrade the `am` binary to match the on-disk schema.",
                        "Or restore an older DB backup written by the same binary version.",
                    ]),
                },
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                // Detect-only. Forward migration is the boot
                // path's job; backward migration is impossible.
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        match self.direction {
            Direction::ForwardMigrate => format!(
                "DB {} is at user_version={} but the binary expects {}. Restart `am serve` so \
                 the boot migration runs forward, or run `am doctor reconstruct` to rebuild \
                 from the git archive. Auto-fix is detect-only because running migration SQL \
                 through Op::DbExec while a live server is starting would race.",
                self.db_path.display(),
                self.on_disk_version,
                self.compiled_version,
            ),
            Direction::Newer => format!(
                "DB {} is at user_version={} but the binary only knows version {}. The DB \
                 was written by a newer `am` binary. Upgrade the binary or restore an older \
                 DB backup — there is no backward migration path.",
                self.db_path.display(),
                self.on_disk_version,
                self.compiled_version,
            ),
        }
    }
}

/// Detector inputs: candidate DB paths + the parent dir to probe
/// for the `.no-migrate` marker. The marker lives next to the DB
/// at `<dirname(db)>/.no-migrate`.
pub fn detect(candidate_dbs: &[PathBuf]) -> Vec<SchemaVersionMismatchFinding> {
    let mut out = Vec::new();
    for db in candidate_dbs {
        if let Some(f) = detect_one(db) {
            out.push(f);
        }
    }
    out
}

fn detect_one(db_path: &std::path::Path) -> Option<SchemaVersionMismatchFinding> {
    // Pass-35-review Gemini F1 (P1): URI + immutable=1 so the
    // read-only open cannot create -shm on a WAL-mode DB. See the
    // detailed rationale in text_timestamp_contamination.rs.
    let uri = super::sqlite_immutable_uri(db_path);
    let mut flags = OpenFlags::read_only();
    flags.uri = true;
    let config = SqliteConfig::file(uri).flags(flags);
    let conn = SqliteConnection::open(&config).ok()?;
    let rows = conn.query_sync("PRAGMA user_version", &[]).ok()?;
    let on_disk: i64 = rows.first()?.get_named::<i64>("user_version").ok()?;
    let on_disk = i32::try_from(on_disk).ok()?;
    let compiled = SCHEMA_VERSION;
    if on_disk == compiled {
        return None;
    }
    let direction = if on_disk < compiled {
        Direction::ForwardMigrate
    } else {
        Direction::Newer
    };
    let no_migrate_marker_present = db_path
        .parent()
        .map(|p| p.join(".no-migrate").exists())
        .unwrap_or(false);
    Some(SchemaVersionMismatchFinding {
        db_path: db_path.to_path_buf(),
        on_disk_version: on_disk,
        compiled_version: compiled,
        direction,
        no_migrate_marker_present,
    })
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &SchemaVersionMismatchFinding,
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
    use tempfile::TempDir;

    fn make_db_with_version(td: &TempDir, version: i32) -> PathBuf {
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(&format!("PRAGMA user_version = {version}"))
            .unwrap();
        drop(conn);
        db
    }

    #[test]
    fn detector_returns_empty_when_versions_match() {
        let td = TempDir::new().unwrap();
        let db = make_db_with_version(&td, SCHEMA_VERSION);
        assert!(detect(std::slice::from_ref(&db)).is_empty());
    }

    #[test]
    fn detector_flags_forward_migrate_needed() {
        let td = TempDir::new().unwrap();
        let db = make_db_with_version(&td, SCHEMA_VERSION - 1);
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].direction, Direction::ForwardMigrate);
        assert_eq!(findings[0].on_disk_version, SCHEMA_VERSION - 1);
        assert_eq!(findings[0].compiled_version, SCHEMA_VERSION);
        let g = findings[0].to_finding();
        assert_eq!(g.severity, "P0");
    }

    #[test]
    fn detector_flags_newer_than_binary() {
        let td = TempDir::new().unwrap();
        let db = make_db_with_version(&td, SCHEMA_VERSION + 1);
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].direction, Direction::Newer);
        let g = findings[0].to_finding();
        assert_eq!(g.severity, "P1");
    }

    #[test]
    fn detector_skips_missing_db() {
        let td = TempDir::new().unwrap();
        let findings = detect(&[td.path().join("nope.sqlite3")]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_records_no_migrate_marker() {
        let td = TempDir::new().unwrap();
        let db = make_db_with_version(&td, SCHEMA_VERSION + 1);
        std::fs::write(td.path().join(".no-migrate"), b"").unwrap();
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].no_migrate_marker_present);
    }

    #[test]
    fn finding_recovery_paths_differ_by_direction() {
        let forward = SchemaVersionMismatchFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            on_disk_version: 0,
            compiled_version: SCHEMA_VERSION,
            direction: Direction::ForwardMigrate,
            no_migrate_marker_present: false,
        };
        let newer = SchemaVersionMismatchFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            on_disk_version: SCHEMA_VERSION + 5,
            compiled_version: SCHEMA_VERSION,
            direction: Direction::Newer,
            no_migrate_marker_present: false,
        };
        assert!(forward.manual_remediation_text().contains("Restart"));
        assert!(newer.manual_remediation_text().contains("Upgrade"));
        assert_eq!(forward.to_finding().severity, "P0");
        assert_eq!(newer.to_finding().severity, "P1");
    }

    #[test]
    fn finding_is_detect_only() {
        let f = SchemaVersionMismatchFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            on_disk_version: 0,
            compiled_version: SCHEMA_VERSION,
            direction: Direction::ForwardMigrate,
            no_migrate_marker_present: false,
        };
        let g = f.to_finding();
        assert!(!g.remediation.auto_fixable);
    }
}
