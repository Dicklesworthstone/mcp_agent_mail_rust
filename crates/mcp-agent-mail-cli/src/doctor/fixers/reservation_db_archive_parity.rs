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
//! Auto-fix is intentionally narrow and applies only to the
//! `archive_id_collision` drift class (GH#167): an archive `id-<id>.json`
//! artifact whose reservation id exists in SQLite under a *different* project.
//! SQLite reservation ids are global while the archive parity key is
//! `(project_slug, id)`, so such an artifact is a stale duplicate left behind by
//! a reused id — never a missing DB row to insert. Those duplicates are
//! quarantined via `Op::Rename` (hash-witnessed + `undo`-reversible). All other
//! drift classes (agent/released_ts/active/thread mismatches,
//! `archive_without_db`, `missing_archive`) remain detect-only: they need
//! operator-supplied truth about which side is authoritative, so the fixer skips
//! them.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
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
        // Only cross-project global-id collisions are auto-fixable (quarantine the
        // stale duplicate); all other drift classes stay detect-only.
        let collisions = self.report.drift.archive_id_collisions;
        let auto_fixable = collisions > 0;
        let command = if auto_fixable {
            format!("am doctor fix --only {FM_ID} --yes")
        } else {
            format!("am doctor fix --only {FM_ID} --list --json")
        };
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title: format!(
                "file reservation DB/archive parity drift: {} mismatch signal(s) in {} ({collisions} global-id collision(s) auto-quarantinable)",
                self.report.drift.total(),
                self.db_path.display(),
            ),
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "storage_root": self.storage_root.to_string_lossy(),
                "health_line": self.report.health_line(),
                "archive_id_collisions": collisions,
                "report": self.report,
                "manual_remediation": {
                    "warning": "Most drift is detect-only: do not reconcile by guessing. Preserve DB and archive, inspect examples, then choose the authoritative side per reservation. The exception is `archive_id_collision` rows — stale duplicate archive artifacts whose id is owned by another project in SQLite — which are safe to quarantine automatically.",
                    "steps": [
                        "Run `am doctor fix --only fm-db-state-files-reservation-db-archive-parity --list --json` for structured drift examples.",
                        "For `archive_id_collision` examples (global reservation id reused across projects), run `am doctor fix --only fm-db-state-files-reservation-db-archive-parity --yes` to quarantine only the stale duplicate archive artifacts (reversible via `am doctor undo`).",
                        "If the archive is authoritative for all remaining affected reservations, run `am doctor reconstruct --dry-run --json` to preview a DB rebuild before applying it.",
                        "If SQLite/release-ledger evidence is authoritative, regenerate or rewrite the affected stable archive artifacts through a dedicated repair path; do not hand-edit production state without preserving the original bytes.",
                        "Re-run `am doctor health` and this detector until reservation_parity reports drift=0.",
                    ],
                },
            }),
            remediation: FindingRemediation {
                command,
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable,
                estimated_actions: collisions,
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
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &ReservationDbArchiveParityFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    let mut actions_taken = 0;
    let mut actions_skipped = 0;
    let mut quarantined_paths = Vec::new();
    // Per-entry suffix keeps same-id collisions from different projects from
    // colliding at the quarantine destination.
    let base_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut quarantine_index: u128 = 0;

    for example in &finding.report.examples {
        if example.field != "archive_id_collision" {
            // Every other drift class is detect-only — it needs operator-supplied
            // truth about which side is authoritative.
            continue;
        }
        // Reconstruct the active-scan archive artifact path from the project slug
        // (the directory the stale duplicate lives under) and the reservation id.
        let archive_path = finding
            .storage_root
            .join("projects")
            .join(&example.project_slug)
            .join("file_reservations")
            .join(format!("id-{}.json", example.reservation_id));
        // Idempotent: if the duplicate already vanished between detect and fix,
        // there is nothing to quarantine.
        if !archive_path.exists() {
            actions_skipped += 1;
            continue;
        }
        let quarantine = ctx
            .run_dir
            .join("quarantine")
            .join("reservation-id-collisions")
            .join(format!(
                "{}-id-{}.json.{}",
                example.project_slug,
                example.reservation_id,
                base_ns + quarantine_index
            ));
        mutate(
            ctx,
            &archive_path,
            Op::Rename {
                to: quarantine.clone(),
            },
        )?;
        actions_taken += 1;
        quarantine_index += 1;
        quarantined_paths.push(quarantine);
    }

    // No collision artifacts to move (the rest is detect-only) → record a skip so
    // the outcome honestly reflects that drift remains for manual reconciliation.
    if actions_taken == 0 && actions_skipped == 0 {
        actions_skipped = 1;
    }

    Ok(FixOutcome {
        actions_taken,
        actions_skipped,
        quarantined_paths,
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
        // A pure field-mismatch (no global-id collision) stays detect-only.
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

    // ── GH#167: cross-project global-id collision ──────────────────────────

    const COLLISION_SQL: &str = "\
CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, human_key TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL);
CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT, inception_ts INTEGER NOT NULL, last_active_ts INTEGER NOT NULL, capabilities TEXT, metadata TEXT, FOREIGN KEY(project_id) REFERENCES projects(id));
CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, path_pattern TEXT NOT NULL, exclusive INTEGER NOT NULL, reason TEXT, created_ts INTEGER NOT NULL, expires_ts INTEGER NOT NULL, released_ts INTEGER, FOREIGN KEY(project_id) REFERENCES projects(id), FOREIGN KEY(agent_id) REFERENCES agents(id));
CREATE TABLE file_reservation_releases (reservation_id INTEGER PRIMARY KEY, released_ts INTEGER NOT NULL, FOREIGN KEY(reservation_id) REFERENCES file_reservations(id));
INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'project-bravo', '/tmp/project-bravo', 1700001000000000);
INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, capabilities, metadata) VALUES (1, 1, 'BravoHolder', 'codex-cli', 'gpt-5', 'collision fixture holder', 1700001000000000, 1700001000000000, NULL, NULL);
INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) VALUES (701, 1, 1, 'src/bravo.rs', 1, 'br-collision-fixture', 1700001010000000, 1700004610000000, NULL);
";

    const COLLISION_BRAVO_JSON: &str = r#"{
  "id": 701,
  "project": "project-bravo",
  "agent": "BravoHolder",
  "path_pattern": "src/bravo.rs",
  "exclusive": true,
  "reason": "br-collision-fixture",
  "created_ts": 1700001010000000,
  "expires_ts": 1700004610000000,
  "released_ts": null
}"#;

    const COLLISION_ALPHA_JSON: &str = r#"{
  "id": 701,
  "project": "project-alpha",
  "agent": "AlphaGhost",
  "path_pattern": "src/alpha.rs",
  "exclusive": true,
  "reason": "br-old-alpha",
  "created_ts": 1699990000000000,
  "expires_ts": 1699993600000000,
  "released_ts": null
}"#;

    /// Materializes the collision scenario: SQLite reservation id 701 is owned by
    /// `project-bravo` (with a matching, aligned archive artifact), while a stale
    /// duplicate `id-701.json` also exists under the unrelated `project-alpha`
    /// archive — the global-id collision the fixer must quarantine.
    fn materialize_collision_fixture() -> (TempDir, PathBuf) {
        let td = TempDir::new().expect("tempdir");
        let db_path = td.path().join("storage.sqlite3");
        let conn = CanonicalDbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(COLLISION_SQL).expect("seed collision SQL");
        drop(conn);

        for (project, json) in [
            ("project-bravo", COLLISION_BRAVO_JSON),
            ("project-alpha", COLLISION_ALPHA_JSON),
        ] {
            let dir = td
                .path()
                .join("projects")
                .join(project)
                .join("file_reservations");
            std::fs::create_dir_all(&dir).expect("mkdir reservation archive");
            std::fs::write(dir.join("id-701.json"), json).expect("write reservation archive");
        }
        (td, db_path)
    }

    fn collision_ctx(td: &TempDir, run_id: &str) -> crate::doctor::mutate::MutateContext {
        use crate::doctor::mutate::{Capabilities, MutateContext};
        use crate::doctor::runs::scaffold_run_dir;
        let run_dir = scaffold_run_dir(td.path(), run_id).expect("scaffold run dir");
        let actions = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .expect("open actions.jsonl");
        MutateContext {
            run_id: run_id.to_string(),
            run_dir,
            capabilities: Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: std::sync::Mutex::new(actions),
            fixer_id: FM_ID.to_string(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: std::time::Instant::now(),
            extra_locks: Vec::new(),
        }
    }

    #[test]
    fn detector_classifies_cross_project_global_id_collision() {
        let (storage_root, db_path) = materialize_collision_fixture();
        let findings = detect(Some(storage_root.path()), std::slice::from_ref(&db_path));
        assert_eq!(findings.len(), 1);
        let report = &findings[0].report;
        // The duplicate is a global-id collision, NOT a missing DB row to insert.
        assert_eq!(report.drift.archive_id_collisions, 1);
        assert_eq!(report.drift.archive_without_db_rows, 0);
        assert_eq!(report.drift.missing_archive_artifacts, 0);
        assert_eq!(report.drift.total(), 1);
        let collision = report
            .examples
            .iter()
            .find(|e| e.field == "archive_id_collision")
            .expect("collision example");
        assert_eq!(collision.reservation_id, 701);
        assert_eq!(collision.project_slug, "project-alpha");
        assert!(collision.db_value.contains("project-bravo"));
        assert!(collision.detail.contains("global reservation id reused"));
        // The finding advertises the auto-quarantine remediation.
        let rendered = findings[0].to_finding();
        assert!(rendered.remediation.auto_fixable);
        assert_eq!(rendered.remediation.estimated_actions, 1);
        assert!(rendered.remediation.command.contains("--yes"));
    }

    #[test]
    fn fixer_quarantines_only_the_collision_artifact() {
        let (storage_root, db_path) = materialize_collision_fixture();
        let finding = detect(Some(storage_root.path()), std::slice::from_ref(&db_path))
            .pop()
            .expect("finding");

        let alpha = storage_root
            .path()
            .join("projects/project-alpha/file_reservations/id-701.json");
        let bravo = storage_root
            .path()
            .join("projects/project-bravo/file_reservations/id-701.json");
        assert!(alpha.exists() && bravo.exists());

        let ctx = collision_ctx(&storage_root, "2026-06-26T00-00-00Z__collision");
        let outcome = fix(&ctx, &finding).expect("fix");

        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.quarantined_paths.len(), 1);
        // The stale duplicate is gone from the active scan path...
        assert!(!alpha.exists(), "collision duplicate must be quarantined");
        // ...and the canonical (DB-owning project) artifact is untouched.
        assert!(bravo.exists(), "canonical artifact must NOT be touched");
        // The quarantined bytes are preserved (reversible via `am doctor undo`).
        let q = &outcome.quarantined_paths[0];
        assert!(
            q.exists(),
            "quarantined artifact must exist at {}",
            q.display()
        );
        assert!(
            std::fs::read_to_string(q)
                .unwrap()
                .contains("project-alpha")
        );

        // Re-running the detector now reports zero drift.
        let after = detect(Some(storage_root.path()), std::slice::from_ref(&db_path));
        assert!(after.is_empty(), "drift should be cleared after quarantine");
    }

    #[test]
    fn fixer_skips_non_collision_drift() {
        // A pure field mismatch (no collision) must remain detect-only: the fixer
        // takes no destructive action.
        let (storage_root, db_path) = materialize_fixture(STALE_AGENT_SQL, STALE_AGENT_JSON, 101);
        let finding = detect(Some(storage_root.path()), std::slice::from_ref(&db_path))
            .pop()
            .expect("finding");
        let ctx = collision_ctx(&storage_root, "2026-06-26T00-00-01Z__noop");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert!(outcome.quarantined_paths.is_empty());
        // The archive artifact is left in place for manual reconciliation.
        assert!(
            storage_root
                .path()
                .join("projects/reservation-regression/file_reservations/id-101.json")
                .exists()
        );
    }

    // ── br-5xbua: retention-pruned released reservations are not drift ─────────

    const PRUNED_SQL: &str = "\
CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, human_key TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL);
CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT, inception_ts INTEGER NOT NULL, last_active_ts INTEGER NOT NULL, capabilities TEXT, metadata TEXT, FOREIGN KEY(project_id) REFERENCES projects(id));
CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, path_pattern TEXT NOT NULL, exclusive INTEGER NOT NULL, reason TEXT, created_ts INTEGER NOT NULL, expires_ts INTEGER NOT NULL, released_ts INTEGER, FOREIGN KEY(project_id) REFERENCES projects(id), FOREIGN KEY(agent_id) REFERENCES agents(id));
CREATE TABLE file_reservation_releases (reservation_id INTEGER PRIMARY KEY, released_ts INTEGER NOT NULL, FOREIGN KEY(reservation_id) REFERENCES file_reservations(id));
INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'project-bravo', '/tmp/project-bravo', 1700001000000000);
INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, capabilities, metadata) VALUES (1, 1, 'BravoHolder', 'codex-cli', 'gpt-5', 'retention fixture', 1700001000000000, 1700001000000000, NULL, NULL);
";

    // A released reservation the retention prune deleted from SQLite, still in
    // the archive (released_ts positive) — expected, not drift.
    const PRUNED_RELEASED_JSON: &str = r#"{
  "id": 801,
  "project": "project-bravo",
  "agent": "BravoHolder",
  "path_pattern": "src/pruned.rs",
  "exclusive": true,
  "reason": "br-retention-fixture",
  "created_ts": 1700001010000000,
  "expires_ts": 1700004610000000,
  "released_ts": 1700004610000000
}"#;

    // An active reservation present in the archive but missing from SQLite —
    // genuine drift worth reconstructing.
    const PRUNED_ACTIVE_JSON: &str = r#"{
  "id": 802,
  "project": "project-bravo",
  "agent": "BravoHolder",
  "path_pattern": "src/active.rs",
  "exclusive": true,
  "reason": "br-retention-fixture",
  "created_ts": 1700001010000000,
  "expires_ts": 1700004610000000,
  "released_ts": null
}"#;

    #[test]
    fn detector_treats_pruned_released_archive_as_expected_not_drift() {
        let td = TempDir::new().expect("tempdir");
        let db_path = td.path().join("storage.sqlite3");
        let conn = CanonicalDbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(PRUNED_SQL).expect("seed retention SQL");
        drop(conn);

        let dir = td
            .path()
            .join("projects")
            .join("project-bravo")
            .join("file_reservations");
        std::fs::create_dir_all(&dir).expect("mkdir reservation archive");
        std::fs::write(dir.join("id-801.json"), PRUNED_RELEASED_JSON).expect("write released");
        std::fs::write(dir.join("id-802.json"), PRUNED_ACTIVE_JSON).expect("write active");

        let findings = detect(Some(td.path()), std::slice::from_ref(&db_path));
        // Only the active orphan is drift, so exactly one finding is raised.
        assert_eq!(findings.len(), 1, "active orphan is genuine drift");
        let report = &findings[0].report;
        // The released-but-pruned reservation is tracked as expected, not drift.
        assert_eq!(
            report.drift.pruned_released_archived, 1,
            "released-pruned reservation must be expected, not drift"
        );
        assert_eq!(
            report.drift.archive_without_db_rows, 1,
            "active orphan must remain genuine drift"
        );
        assert_eq!(
            report.drift.total(),
            1,
            "only the active orphan counts toward drift total"
        );
    }
}
