//! `fm-db-state-files-integrity-page-malformed` — P0.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! `storage.sqlite3` has malformed page(s) — `PRAGMA integrity_check`
//! returns an error string (or a list of errors) instead of `"ok"`.
//! This is the canonical signal that on-disk B-tree pages have
//! drifted from their indexes, cells overflow incorrectly, or a
//! page boundary marker is wrong. Causes range from:
//!
//! - storage media failure (bad sectors, FS-level corruption),
//! - kernel crash mid-page-write before fsync completed,
//! - concurrent writes from two SQLite processes (the canonical
//!   reason for `fm-db-state-files-python-server-coresident-write`),
//! - a partial restore from backup that mismatched WAL + main.
//!
//! Once integrity is broken, every query that touches a malformed
//! page either errors or — worse — returns silently-wrong rows.
//! Recovery is non-trivial: SQLite's recovery extension or
//! `am doctor reconstruct` against the git archive.
//!
//! ## Detection (pure function)
//!
//! Production dispatch materializes live FrankenSQLite state to one retained
//! private logical snapshot, then runs:
//!
//! ```sql
//! PRAGMA integrity_check(1)
//! ```
//!
//! The `1` limits the result to the first error — full
//! integrity check on a multi-GB DB can take minutes; we only
//! need to know whether the DB is corrupt, not enumerate every
//! page that's broken. If the column value is the literal
//! string `"ok"`, no finding. Otherwise emit a P0 finding with
//! the error text as evidence.
//!
//! ### Performance note
//!
//! `PRAGMA integrity_check` reads every page in the DB. On
//! large mailbox DBs (multi-GB) this can run for several
//! minutes. The detector is intentionally **NOT** part of the
//! default `am doctor` sweep; agents wanting to run it must
//! invoke `am doctor fix --only fm-db-state-files-integrity-page-malformed
//! --list` explicitly. The default sweep relies on cheaper
//! detectors (`empty_or_truncated_db`, `wal_mode_disabled`,
//! `world_readable_storage_db`) for sub-200ms turnaround.
//!
//! ## Fix
//!
//! **Detect-only.** Auto-repair would require the
//! `am doctor reconstruct` path (Op::Rename the corrupt DB to
//! quarantine, then INSERT...SELECT from the git archive into
//! a fresh DB), which is a separate, already-implemented
//! command with its own UI. The manual_remediation envelope
//! routes operators there.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-integrity-page-malformed";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "db_state_files";

#[derive(Debug, Clone, Serialize)]
pub struct IntegrityPageMalformedFinding {
    pub db_path: PathBuf,
    /// The exact text returned by `PRAGMA integrity_check(1)`.
    /// For `"ok"` DBs the detector emits no finding; for
    /// non-`"ok"` results this carries SQLite's error
    /// description (e.g., `"*** in database main *** Page 42:
    /// ..."`).
    pub integrity_check_result: String,
    /// Size of the DB file in bytes — useful for operators
    /// deciding whether the corruption is whole-file (likely
    /// truncation, but caught by `empty_or_truncated_db` first)
    /// or page-level (likely media / concurrent-writer fault).
    pub db_size_bytes: u64,
}

impl IntegrityPageMalformedFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "DB {} failed PRAGMA integrity_check: {}",
            self.db_path.display(),
            // Truncate the result for the title; full result is
            // in evidence.
            self.integrity_check_result
                .chars()
                .take(120)
                .collect::<String>(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "integrity_check_result": self.integrity_check_result,
                "db_size_bytes": self.db_size_bytes,
                "recovery_paths": [
                    "`am doctor reconstruct --yes` (rebuilds DB from git archive — destructive on the corrupt file but reversible via undo).",
                    "Restore from backup: `am doctor undo <prior-run-id>` if the corruption appeared after a recent doctor run.",
                ],
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                // Detect-only — `am doctor reconstruct` is the
                // canonical fix path.
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        format!(
            "DB {} failed SQLite's integrity_check ({} bytes). Recovery requires \
             `am doctor reconstruct --yes` which Op::Rename's the corrupt file to \
             quarantine, then rebuilds a fresh DB by INSERT...SELECT from the git \
             archive. If the corruption appeared right after a doctor run, \
             `am doctor undo <run-id>` may be faster (restores byte-identical from \
             backup). Auto-fix is detect-only because reconstruct is a separate \
             chokepoint-managed surface with its own --yes gate.",
            self.db_path.display(),
            self.db_size_bytes,
        )
    }
}

/// Detector. PURE w.r.t. caller-supplied DB paths.
///
/// **Performance**: `PRAGMA integrity_check(1)` reads every
/// page. On a multi-GB DB this can take minutes. Callers should
/// gate this FM behind explicit operator opt-in
/// (`--only fm-db-state-files-integrity-page-malformed`) rather
/// than bundling it into a sub-200ms health probe.
pub fn detect(candidate_dbs: &[PathBuf]) -> Vec<IntegrityPageMalformedFinding> {
    let read_candidates =
        super::explicit_offline_db_read_candidates(candidate_dbs, "integrity-page detection");
    detect_prepared(&read_candidates)
}

pub(crate) fn detect_prepared(
    read_candidates: &[super::DoctorDbReadCandidate],
) -> Vec<IntegrityPageMalformedFinding> {
    let mut out = Vec::new();
    for candidate in read_candidates {
        if let Some(f) = detect_one(candidate) {
            out.push(f);
        }
    }
    out
}

fn detect_one(candidate: &super::DoctorDbReadCandidate) -> Option<IntegrityPageMalformedFinding> {
    let db_path = candidate.target_path();
    let result = match candidate.integrity_check_one() {
        super::DoctorIntegrityProbe::Result(result) => result,
        super::DoctorIntegrityProbe::Corruption(detail) => {
            let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
            return Some(IntegrityPageMalformedFinding {
                db_path: db_path.to_path_buf(),
                integrity_check_result: format!("open failed before integrity_check: {detail}"),
                db_size_bytes,
            });
        }
        super::DoctorIntegrityProbe::Unavailable => return None,
    };
    if result == "ok" {
        return None;
    }
    let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    Some(IntegrityPageMalformedFinding {
        db_path: db_path.to_path_buf(),
        integrity_check_result: result,
        db_size_bytes,
    })
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &IntegrityPageMalformedFinding,
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
    use sqlmodel_sqlite::SqliteConnection;
    use tempfile::TempDir;

    const WAL_WRITER_PATH_ENV: &str = "AM_DOCTOR_INTEGRITY_WAL_WRITER_PATH";
    const WAL_WRITER_READY_ENV: &str = "AM_DOCTOR_INTEGRITY_WAL_WRITER_READY";
    const WAL_WRITER_RELEASE_ENV: &str = "AM_DOCTOR_INTEGRITY_WAL_WRITER_RELEASE";
    const WAL_WRITER_TEST: &str =
        "doctor::fixers::integrity_page_malformed::tests::live_wal_only_check_violation_is_integrity_truth";
    const WAL_WRITER_WITNESS: &str = "INTEGRITY_WAL_WRITER_CHILD_RAN";

    fn make_healthy_db(td: &TempDir) -> PathBuf {
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
        drop(conn);
        db
    }

    #[test]
    fn detector_returns_empty_for_healthy_db() {
        let td = TempDir::new().unwrap();
        let db = make_healthy_db(&td);
        let findings = detect(std::slice::from_ref(&db));
        assert!(findings.is_empty(), "healthy DB must not flag");
    }

    #[test]
    fn live_wal_only_check_violation_is_integrity_truth() {
        if let Ok(db_path) = std::env::var(WAL_WRITER_PATH_ENV) {
            let ready_path = PathBuf::from(
                std::env::var(WAL_WRITER_READY_ENV).expect("integrity WAL writer ready path"),
            );
            let release_path = PathBuf::from(
                std::env::var(WAL_WRITER_RELEASE_ENV).expect("integrity WAL writer release path"),
            );
            let writer = mcp_agent_mail_db::DbConn::open_file(db_path)
                .expect("open cross-process integrity WAL writer");
            writer
                .execute_raw(
                    "PRAGMA wal_autocheckpoint = 0;
                     PRAGMA ignore_check_constraints = ON;
                     INSERT INTO checked_values(value) VALUES (-1);",
                )
                .expect("commit invalid CHECK row only to WAL");
            std::fs::write(&ready_path, b"ready")
                .expect("publish integrity WAL writer readiness");
            println!("{WAL_WRITER_WITNESS}");
            assert!(
                super::super::wait_for_cross_process_release(&release_path),
                "parent did not release integrity WAL writer in time"
            );
            drop(writer);
            return;
        }

        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let admitted = mcp_agent_mail_db::DbConn::open_file(db.display().to_string())
            .expect("open live integrity fixture");
        admitted
            .execute_raw(
                "CREATE TABLE checked_values(value INTEGER NOT NULL CHECK(value > 0));
                 PRAGMA journal_mode = WAL;
                 PRAGMA wal_autocheckpoint = 0;
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .expect("seed checked table");
        mcp_agent_mail_db::close_db_conn(admitted, "settle integrity WAL baseline");
        let main_before = std::fs::read(&db).expect("read settled integrity main");

        // SQLite's integrity_check validates CHECK constraints. Disable only
        // insertion-time enforcement and commit the invalid row to WAL, leaving
        // the settled main image healthy. A direct immutable main-file probe
        // therefore false-greens this fixture.
        let ready_path = td.path().join("integrity-wal-writer.ready");
        let release_path = td.path().join("integrity-wal-writer.release");
        let child = std::process::Command::new(
            std::env::current_exe().expect("resolve integrity test executable"),
        )
        .arg(WAL_WRITER_TEST)
        .arg("--exact")
        .arg("--nocapture")
        .env(WAL_WRITER_PATH_ENV, &db)
        .env(WAL_WRITER_READY_ENV, &ready_path)
        .env(WAL_WRITER_RELEASE_ENV, &release_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cross-process integrity WAL writer");
        let child = super::super::CrossProcessTestChild::new(child, release_path);
        if !super::super::wait_for_cross_process_signal(&ready_path) {
            let output = child
                .release_and_wait()
                .expect("collect unready integrity WAL writer");
            panic!(
                "integrity WAL writer never became ready: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let main_after = std::fs::read(&db);
        let wal_len = std::fs::metadata(PathBuf::from(format!("{}-wal", db.display())))
            .map(|metadata| metadata.len());

        let candidate = super::super::DoctorDbReadCandidate::open_live_or_explicit_offline(
            &db,
            "cross-process WAL-only integrity truth test",
        );
        let findings = detect_prepared(std::slice::from_ref(&candidate));

        let output = child
            .release_and_wait()
            .expect("collect integrity WAL writer");
        assert!(
            output.status.success(),
            "integrity WAL writer failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(WAL_WRITER_WITNESS),
            "integrity WAL writer filter was vacuous: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            main_after.expect("read WAL-only integrity main"),
            main_before,
            "fixture must keep the violating row out of the main image"
        );
        assert!(
            wal_len.is_ok_and(|len| len > 32),
            "fixture must retain committed WAL frames"
        );
        assert_eq!(findings.len(), 1, "WAL-only violation must be visible");
        assert!(
            findings[0]
                .integrity_check_result
                .to_ascii_lowercase()
                .contains("check constraint"),
            "unexpected integrity result: {}",
            findings[0].integrity_check_result
        );
    }

    #[test]
    fn live_physical_index_damage_is_not_hidden_by_logical_snapshot() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "CREATE TABLE indexed_values(id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE INDEX idx_indexed_values_name ON indexed_values(name);",
        )
        .unwrap();
        for id in 1..=50_i64 {
            conn.execute_raw(&format!(
                "INSERT INTO indexed_values(id, name) VALUES ({id}, 'IndexWitness{id:02}');"
            ))
            .unwrap();
        }
        conn.execute_raw("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        let root_page = conn
            .query_sync(
                "SELECT rootpage FROM sqlite_master WHERE name = 'idx_indexed_values_name'",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("rootpage")
            .unwrap();
        drop(conn);

        let mut bytes = std::fs::read(&db).unwrap();
        let raw_page_size = u16::from_be_bytes([bytes[16], bytes[17]]);
        let page_size = if raw_page_size == 1 {
            65_536_usize
        } else {
            usize::from(raw_page_size)
        };
        let page_start = (usize::try_from(root_page).unwrap() - 1) * page_size;
        let page = &bytes[page_start..page_start + page_size];
        let needle = b"IndexWitness";
        let hit = page
            .windows(needle.len())
            .skip(16)
            .position(|window| window == needle)
            .expect("index page must contain a witness key")
            + 16;
        bytes[page_start + hit + 7] ^= 0x01;
        std::fs::write(&db, bytes).unwrap();

        let admitted = mcp_agent_mail_db::DbConn::open_file(db.display().to_string())
            .expect("admit physical-index corruption through FrankenSQLite");
        let rows = admitted
            .query_sync("SELECT COUNT(*) AS count FROM indexed_values", &[])
            .expect("table b-tree remains readable");
        assert_eq!(rows[0].get_named::<i64>("count").unwrap(), 50);
        mcp_agent_mail_db::close_db_conn(admitted, "settle physical integrity fixture");

        let candidate = super::super::DoctorDbReadCandidate::open_live_or_explicit_offline(
            &db,
            "physical integrity authority test",
        );
        let logical_result = candidate
            .connection()
            .expect("logical snapshot")
            .query_sync("PRAGMA integrity_check(1)", &[])
            .expect("check rebuilt logical snapshot")[0]
            .get_named::<String>("integrity_check")
            .unwrap();
        assert_eq!(logical_result, "ok", "VACUUM should rebuild the index");
        let findings = detect_prepared(std::slice::from_ref(&candidate));
        assert_eq!(
            findings.len(),
            1,
            "physical index corruption must remain authoritative"
        );
        assert_ne!(findings[0].integrity_check_result, "ok");
    }

    #[test]
    fn detector_skips_missing_db() {
        let td = TempDir::new().unwrap();
        let findings = detect(&[td.path().join("nope.sqlite3")]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_non_sqlite_file() {
        // A non-SQLite file fails the direct SQLite-header probe
        // and is silently skipped (sibling FM
        // `empty_or_truncated_db` owns this surface).
        let td = TempDir::new().unwrap();
        let p = td.path().join("garbage.sqlite3");
        std::fs::write(&p, b"not a sqlite db").unwrap();
        let findings = detect(std::slice::from_ref(&p));
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_header_only_truncated_file() {
        // A file with only SQLite's 16-byte magic is not a page-level
        // integrity failure. The empty/truncated FM owns sub-100-byte
        // files because SQLite's database header itself is incomplete.
        let td = TempDir::new().unwrap();
        let p = td.path().join("truncated.sqlite3");
        std::fs::write(&p, super::super::empty_or_truncated_db::SQLITE_MAGIC).unwrap();
        let findings = detect(std::slice::from_ref(&p));
        assert!(findings.is_empty());
    }

    #[test]
    fn production_prepared_source_keeps_non_sqlite_and_truncated_ownership_separate() {
        let td = TempDir::new().unwrap();
        let garbage = td.path().join("garbage.sqlite3");
        let truncated = td.path().join("truncated.sqlite3");
        std::fs::write(&garbage, b"not a sqlite db").unwrap();
        std::fs::write(
            &truncated,
            super::super::empty_or_truncated_db::SQLITE_MAGIC,
        )
        .unwrap();
        let candidates = [garbage, truncated]
            .iter()
            .map(|path| {
                super::super::DoctorDbReadCandidate::open_live_or_explicit_offline(
                    path,
                    "prepared integrity ownership test",
                )
            })
            .collect::<Vec<_>>();
        assert!(
            detect_prepared(&candidates).is_empty(),
            "non-SQLite and truncated targets belong to empty_or_truncated_db"
        );
    }

    #[cfg(unix)]
    #[test]
    fn detector_skips_fifo_without_blocking() {
        use std::os::unix::fs::FileTypeExt as _;

        let td = TempDir::new().unwrap();
        let fifo = td.path().join("storage.sqlite3");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        assert!(
            std::fs::symlink_metadata(&fifo)
                .unwrap()
                .file_type()
                .is_fifo()
        );

        let findings = detect(std::slice::from_ref(&fifo));
        assert!(findings.is_empty(), "FIFO must not block or flag");
    }

    #[test]
    fn corruption_query_error_becomes_p0_finding() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
        drop(conn);

        let bytes = std::fs::read(&db).unwrap();
        let page_size = 4096;
        assert!(
            bytes.len() > page_size,
            "fixture DB should include a second page"
        );
        let mut corrupted = bytes;
        corrupted[page_size] ^= 0x7f;
        std::fs::write(&db, corrupted).unwrap();

        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .integrity_check_result
                .contains("PRAGMA integrity_check(1) failed")
                || findings[0]
                    .integrity_check_result
                    .contains("*** in database main ***")
                || findings[0].integrity_check_result.contains("malformed")
        );
    }

    #[test]
    fn finding_severity_is_p0_detect_only() {
        let f = IntegrityPageMalformedFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            integrity_check_result: "*** in database main *** Page 42: corrupt".to_string(),
            db_size_bytes: 1_234_567,
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P0");
        assert!(!g.remediation.auto_fixable);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("am doctor reconstruct"));
        assert!(s.contains("integrity_check_result"));
    }

    #[test]
    fn manual_remediation_includes_db_size_and_reconstruct_pointer() {
        let f = IntegrityPageMalformedFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            integrity_check_result: "***corrupt***".to_string(),
            db_size_bytes: 2_000_000,
        };
        let text = f.manual_remediation_text();
        assert!(text.contains("2000000"));
        assert!(text.contains("am doctor reconstruct"));
    }

    #[test]
    fn finding_title_truncates_long_integrity_results() {
        let long_result = "x".repeat(500);
        let f = IntegrityPageMalformedFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            integrity_check_result: long_result,
            db_size_bytes: 0,
        };
        let g = f.to_finding();
        // Title carries first 120 chars; evidence carries full.
        assert!(g.title.len() < 200);
    }
}
