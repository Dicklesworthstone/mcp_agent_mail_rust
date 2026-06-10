//! `fm-db-state-files-reservation-db-archive-parity` — P1
//! detect-only.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! File reservations are represented twice:
//!
//! - SQLite rows in `file_reservations` plus the
//!   `file_reservation_releases` sidecar ledger.
//! - Stable archive artifacts at
//!   `<storage_root>/projects/<slug>/file_reservations/id-<id>.json`.
//!
//! The pre-commit guard and human archive review consume the archive side,
//! while MCP tools primarily read SQLite. If those stores disagree, a held
//! reservation can over-block, under-block, or resurrect after release.
//!
//! ## Detection
//!
//! Opens each candidate DB read-only/immutable and runs the shared
//! reservation parity checker from `mcp-agent-mail-tools`. The checker
//! compares stable reservation ids, holder agent names, effective release
//! timestamps (`file_reservation_releases` wins over the hot row), active
//! status, and thread/reason provenance.
//!
//! ## Fix
//!
//! Detect-only for now. The checker intentionally reports drift but does not
//! carry enough artifact path/action detail to choose and apply the
//! authoritative side safely through `mutate()`. A future repair FM should
//! use the same comparison core but emit concrete `Op::DbExec` /
//! `Op::WriteFile` actions with an explicit reconciliation policy.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use mcp_agent_mail_tools::reservation_parity::{
    ReservationParityReport, check_reservation_parity_with_canonical_conn,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-db-state-files-reservation-db-archive-parity";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "db_state_files";

#[derive(Debug, Clone, Serialize)]
pub struct ReservationDbArchiveParityFinding {
    pub db_path: PathBuf,
    pub storage_root: PathBuf,
    pub report: ReservationParityReport,
}

impl ReservationDbArchiveParityFinding {
    pub fn to_finding(&self) -> super::Finding {
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title: format!(
                "file reservation DB/archive parity drift: {} mismatch signal(s) in {}",
                self.report.drift.total(),
                self.db_path.display(),
            ),
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "storage_root": self.storage_root.to_string_lossy(),
                "health_line": self.report.health_line(),
                "report": self.report,
                "manual_remediation": {
                    "warning": "Detect-only: do not reconcile by guessing. Preserve DB and archive, inspect examples, then choose the authoritative side per reservation.",
                    "steps": [
                        "Run `am doctor fix --only fm-db-state-files-reservation-db-archive-parity --list --json` for structured drift examples.",
                        "If the archive is authoritative for all affected reservations, run `am doctor reconstruct --dry-run --json` to preview a DB rebuild before applying it.",
                        "If SQLite/release-ledger evidence is authoritative, regenerate or rewrite the affected stable archive artifacts through a dedicated repair path; do not hand-edit production state without preserving the original bytes.",
                        "Re-run `am doctor health` and this detector until reservation_parity reports drift=0.",
                    ],
                },
            }),
            remediation: FindingRemediation {
                command: format!("am doctor fix --only {FM_ID} --list --json"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

pub fn detect(
    storage_root: Option<&Path>,
    candidate_dbs: &[PathBuf],
) -> Vec<ReservationDbArchiveParityFinding> {
    let Some(storage_root) = storage_root else {
        return Vec::new();
    };
    if !storage_root.is_dir() {
        return Vec::new();
    }

    let mut findings = Vec::new();
    for db_path in candidate_dbs {
        let Ok(conn) = super::open_immutable_sqlite(db_path) else {
            continue;
        };
        let Ok(report) = check_reservation_parity_with_canonical_conn(&conn, storage_root) else {
            continue;
        };
        if report.ok {
            continue;
        }
        findings.push(ReservationDbArchiveParityFinding {
            db_path: db_path.clone(),
            storage_root: storage_root.to_path_buf(),
            report,
        });
    }
    findings
}

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &ReservationDbArchiveParityFinding,
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
    use mcp_agent_mail_db::CanonicalDbConn;
    use tempfile::TempDir;

    const STALE_AGENT_SQL: &str = include_str!(
        "../../../../../tests/fixtures/reservation_regression/recipes/stale_agent_id_row.sql"
    );
    const STALE_AGENT_JSON: &str = include_str!(
        "../../../../../tests/fixtures/reservation_regression/recipes/stale_agent_id_row_archive_id_101.json"
    );
    const STUCK_NULL_SQL: &str = include_str!(
        "../../../../../tests/fixtures/reservation_regression/recipes/stuck_null_released_ts.sql"
    );
    const STUCK_NULL_JSON: &str = include_str!(
        "../../../../../tests/fixtures/reservation_regression/recipes/stuck_null_released_ts_archive_id_201.json"
    );
    const ARCHIVE_ACTIVE_SQL: &str = include_str!(
        "../../../../../tests/fixtures/reservation_regression/recipes/db_archive_active_state_mismatch.sql"
    );
    const ARCHIVE_ACTIVE_JSON: &str = include_str!(
        "../../../../../tests/fixtures/reservation_regression/recipes/db_archive_active_state_mismatch_archive_id_301.json"
    );

    fn materialize_fixture(
        sql: &str,
        archive_json: &str,
        reservation_id: i64,
    ) -> (TempDir, PathBuf) {
        let td = TempDir::new().expect("tempdir");
        let db_path = td.path().join("storage.sqlite3");
        let conn = CanonicalDbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(sql).expect("seed fixture SQL");
        drop(conn);

        let reservation_dir = td
            .path()
            .join("projects")
            .join("reservation-regression")
            .join("file_reservations");
        std::fs::create_dir_all(&reservation_dir).expect("mkdir reservation archive");
        std::fs::write(
            reservation_dir.join(format!("id-{reservation_id}.json")),
            archive_json,
        )
        .expect("write reservation archive fixture");

        (td, db_path)
    }

    #[test]
    fn detector_flags_stale_agent_fixture() {
        let (storage_root, db_path) = materialize_fixture(STALE_AGENT_SQL, STALE_AGENT_JSON, 101);
        let findings = detect(Some(storage_root.path()), std::slice::from_ref(&db_path));
        assert_eq!(findings.len(), 1);
        let report = &findings[0].report;
        assert_eq!(report.drift.agent_id_mismatches, 1);
        assert_eq!(report.drift.released_ts_mismatches, 0);
        assert!(report.examples.iter().any(|example| {
            example.detail.contains("reservation_id=101")
                && example.detail.contains("db_agent=StaleHolder")
                && example.detail.contains("archive_agent=CorrectHolder")
        }));
    }

    #[test]
    fn detector_flags_archive_released_db_null_fixture() {
        let (storage_root, db_path) = materialize_fixture(STUCK_NULL_SQL, STUCK_NULL_JSON, 201);
        let findings = detect(Some(storage_root.path()), std::slice::from_ref(&db_path));
        assert_eq!(findings.len(), 1);
        let report = &findings[0].report;
        assert_eq!(report.drift.released_ts_mismatches, 1);
        assert_eq!(report.drift.active_status_mismatches, 1);
        assert!(report.examples.iter().any(|example| {
            example.detail.contains("reservation_id=201")
                && example.detail.contains("db_released_ts=NULL")
                && example
                    .detail
                    .contains("archive_released_ts=1700002010000000")
        }));
    }

    #[test]
    fn detector_flags_db_released_archive_active_fixture() {
        let (storage_root, db_path) =
            materialize_fixture(ARCHIVE_ACTIVE_SQL, ARCHIVE_ACTIVE_JSON, 301);
        let findings = detect(Some(storage_root.path()), std::slice::from_ref(&db_path));
        assert_eq!(findings.len(), 1);
        let report = &findings[0].report;
        assert_eq!(report.drift.released_ts_mismatches, 1);
        assert_eq!(report.drift.active_status_mismatches, 1);
        assert!(report.examples.iter().any(|example| {
            example.detail.contains("reservation_id=301")
                && example.detail.contains("db_released_ts=1700003010000000")
                && example.detail.contains("archive_released_ts=NULL")
        }));
    }

    #[test]
    fn finding_is_detect_only_with_health_line_evidence() {
        let (storage_root, db_path) = materialize_fixture(STALE_AGENT_SQL, STALE_AGENT_JSON, 101);
        let finding = detect(Some(storage_root.path()), std::slice::from_ref(&db_path))
            .pop()
            .expect("finding");
        let rendered = finding.to_finding();
        assert_eq!(rendered.id, FM_ID);
        assert_eq!(rendered.severity, "P1");
        assert!(!rendered.remediation.auto_fixable);
        assert_eq!(rendered.remediation.estimated_actions, 0);
        assert!(
            rendered
                .evidence
                .get("health_line")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|line| line.contains("reservation_parity: drift"))
        );
    }
}
