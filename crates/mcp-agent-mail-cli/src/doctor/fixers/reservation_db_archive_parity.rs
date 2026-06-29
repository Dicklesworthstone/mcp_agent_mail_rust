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
//! Auto-fix reconciles the two drift classes from `mcp_agent_mail_rust#112`
//! (52% stale `agent_id`, 10% stuck-NULL `released_ts`) through the doctor
//! `mutate()` chokepoint (hash-witnessed, `undo`-reversible, behind `--yes`).
//! Each reconcile direction has a *principled winner*, never a guess:
//!
//! - **Release is monotonic and irreversible → released wins.** If either store
//!   records a release, the reservation is released; the lagging store is synced
//!   to the earliest known `released_ts`. A stuck-NULL DB row (over-blocking, the
//!   #112 harm) is updated via a batched `Op::DbExec` (hot row + the durable
//!   `file_reservation_releases` ledger); a stale-active archive artifact is
//!   rewritten via `Op::WriteFile` (only its `released_ts` field changes).
//! - **Holder on a still-active reservation → the immutable acquire-time archive
//!   wins.** The `id-<id>.json` artifact is written once at acquire and is the
//!   human-auditable record of the holder, while the DB hot row is what drifts
//!   under the #112 atomicity bug. The DB `agent_id` is updated via `Op::DbExec`
//!   — but only when the archive's holder resolves to a *registered* agent in the
//!   project. If it does not, the row is genuinely ambiguous: the fixer refuses
//!   to guess and leaves it for manual reconciliation.
//!
//! Cross-project global-id collisions (GH#167) are quarantined via `Op::Rename`
//! as before. Every remaining drift class (`path_pattern`, `exclusive`,
//! `thread_provenance`, `missing_archive`, `archive_without_db`) stays
//! detect-only: it needs operator-supplied truth about which side is
//! authoritative.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use mcp_agent_mail_db::sqlmodel_core::Value;
use mcp_agent_mail_tools::reservation_parity::{
    ReservationParityReport, check_reservation_parity_with_canonical_conn,
};
use serde::Serialize;
use sqlmodel_sqlite::SqliteConnection;
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
        // Auto-fixable drift = cross-project global-id collisions (quarantine) +
        // the two #112 drift classes with a principled winner: holder (agent_id,
        // archive wins) and release (released_ts, released wins). `active_status`
        // is derived from `released_ts`, so it is not counted again here.
        let collisions = self.report.drift.archive_id_collisions;
        let reconcilable =
            self.report.drift.agent_id_mismatches + self.report.drift.released_ts_mismatches;
        let auto_fixable = collisions > 0 || reconcilable > 0;
        // Upper-bound estimate: the live fixer resolves each holder against the
        // registered-agent roster and reports honest taken/skipped counts.
        let estimated_actions = collisions + reconcilable;
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
                "file reservation DB/archive parity drift: {} mismatch signal(s) in {} ({reconcilable} reconcilable, {collisions} global-id collision(s) auto-quarantinable)",
                self.report.drift.total(),
                self.db_path.display(),
            ),
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "storage_root": self.storage_root.to_string_lossy(),
                "health_line": self.report.health_line(),
                "archive_id_collisions": collisions,
                "reconcilable_mismatches": reconcilable,
                "report": self.report,
                "reconcile_policy": {
                    "release": "monotonic/irreversible — if either store records a release the reservation is released; the lagging store is synced to the earliest known released_ts (DB hot row + ledger via Op::DbExec, or the archive artifact's released_ts via Op::WriteFile).",
                    "holder": "the immutable acquire-time archive wins for a still-active reservation, but only when the archive holder resolves to a registered agent; otherwise the row is left for manual reconciliation rather than guessed.",
                    "detect_only": "path_pattern, exclusive, thread_provenance, missing_archive, archive_without_db, and unresolved-holder rows need operator-supplied truth and are never auto-reconciled.",
                },
                "manual_remediation": {
                    "warning": "Holder and release drift are auto-reconciled through the reversible mutate() chokepoint (`--yes`). The remaining classes (path_pattern, exclusive, thread_provenance, missing_archive, archive_without_db, unresolved holder) stay detect-only — do not reconcile those by guessing.",
                    "steps": [
                        "Run `am doctor fix --only fm-db-state-files-reservation-db-archive-parity --list --json` for structured drift examples.",
                        "Run `am doctor fix --only fm-db-state-files-reservation-db-archive-parity --yes` to reconcile holder/release drift and quarantine global-id collisions (reversible via `am doctor undo`).",
                        "For remaining detect-only drift where the archive is authoritative, run `am doctor reconstruct --dry-run --json` to preview a DB rebuild before applying it.",
                        "For remaining detect-only drift where SQLite/release-ledger evidence is authoritative, regenerate the affected archive artifacts through a dedicated repair path; do not hand-edit production state without preserving the original bytes.",
                        "Re-run `am doctor health` and this detector until reservation_parity reports drift=0.",
                    ],
                },
            }),
            remediation: FindingRemediation {
                command,
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable,
                estimated_actions,
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

/// One archive artifact whose `released_ts` lags a DB-recorded release and must
/// be rewritten to the canonical (earliest) release timestamp. Only the
/// `released_ts` field changes; every other field is preserved verbatim
/// (`serde_json` `preserve_order` keeps the key order stable).
struct ArchiveReleaseRewrite {
    path: PathBuf,
    content: Vec<u8>,
    mode: u32,
}

/// The reconcile plan derived from the report plus a live read of the DB and
/// archive. Pure data: `fix` turns it into hash-witnessed `mutate()` calls.
#[derive(Default)]
struct ReconcilePlan {
    /// Integer-only SQL statements (UPDATE/INSERT), applied as ONE batched
    /// `Op::DbExec` so the whole DB-side reconcile is a single reversible unit.
    db_statements: Vec<String>,
    /// Archive artifacts to rewrite (release-monotonic, archive lagging).
    archive_rewrites: Vec<ArchiveReleaseRewrite>,
    /// Genuinely-ambiguous rows left for manual reconciliation (e.g. the archive
    /// names a holder we cannot map to a registered agent — we refuse to guess).
    skipped_ambiguous: usize,
}

/// Per-reservation accumulator while folding the typed parity examples.
#[derive(Default)]
struct ReservationDrift {
    /// The archive's holder name when it diverges from the DB row.
    agent_archive: Option<String>,
    /// `effective_released_ts` parsed from the `released_ts` example's DB side.
    released_db: Option<i64>,
    /// `released_ts` parsed from the example's archive side.
    released_archive: Option<i64>,
    /// Whether a `released_ts` example was present for this reservation.
    has_release_drift: bool,
}

/// Parse a parity example's `released_ts` label (`"NULL"` or a positive integer
/// of microseconds) back into an optional timestamp.
fn parse_ts_label(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("NULL") {
        return None;
    }
    trimmed.parse::<i64>().ok().filter(|micros| *micros > 0)
}

/// Resolve an archive holder name to its registered `agent_id` in the named
/// project. Returns `None` when the holder is not a registered agent — the
/// signal that this holder drift is genuinely ambiguous and must not be guessed.
fn resolve_agent_id(conn: &SqliteConnection, project_slug: &str, agent_name: &str) -> Option<i64> {
    let rows = conn
        .query_sync(
            "SELECT a.id AS agent_id
             FROM agents a JOIN projects p ON p.id = a.project_id
             WHERE p.slug = ? AND a.name = ?",
            &[
                Value::Text(project_slug.to_string()),
                Value::Text(agent_name.to_string()),
            ],
        )
        .ok()?;
    rows.first()?.get_named::<i64>("agent_id").ok()
}

#[cfg(unix)]
fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o777)
        .unwrap_or(0o644)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> u32 {
    0o644
}

/// Build the archive-side rewrite for a stale-active artifact: set only its
/// `released_ts` to the canonical release timestamp, preserving every other
/// field and the original byte mode. Returns `None` (counted as ambiguous) if
/// the artifact is unreadable, unparseable, or its `id` does not match — never
/// fabricate or clobber a mismatched artifact.
fn build_archive_release_rewrite(
    storage_root: &Path,
    project_slug: &str,
    reservation_id: i64,
    released_ts: i64,
) -> Option<ArchiveReleaseRewrite> {
    let path = storage_root
        .join("projects")
        .join(project_slug)
        .join("file_reservations")
        .join(format!("id-{reservation_id}.json"));
    let raw = std::fs::read_to_string(&path).ok()?;
    let mut json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let object = json.as_object_mut()?;
    // Defensive: only rewrite when the artifact's own id matches the target.
    if object.get("id").and_then(serde_json::Value::as_i64) != Some(reservation_id) {
        return None;
    }
    object.insert(
        "released_ts".to_string(),
        serde_json::Value::from(released_ts),
    );
    let mode = file_mode(&path);
    let content = serde_json::to_vec_pretty(&json).ok()?;
    Some(ArchiveReleaseRewrite {
        path,
        content,
        mode,
    })
}

/// Fold the parity report's typed examples plus a live DB/archive read into a
/// pure reconcile plan. Opens the DB read-only; the connection is dropped before
/// `fix` issues any writable `Op::DbExec`.
fn build_reconcile_plan(finding: &ReservationDbArchiveParityFinding) -> ReconcilePlan {
    let mut plan = ReconcilePlan::default();
    let Ok(conn) = super::open_immutable_sqlite(&finding.db_path) else {
        // Can't read the DB → reconcile nothing. The collision quarantine path in
        // `fix` is archive-only and still runs.
        return plan;
    };

    use std::collections::BTreeMap;
    let mut by_reservation: BTreeMap<(String, i64), ReservationDrift> = BTreeMap::new();
    for example in &finding.report.examples {
        match example.field.as_str() {
            "agent_id" => {
                by_reservation
                    .entry((example.project_slug.clone(), example.reservation_id))
                    .or_default()
                    .agent_archive = Some(example.archive_value.clone());
            }
            "released_ts" => {
                let drift = by_reservation
                    .entry((example.project_slug.clone(), example.reservation_id))
                    .or_default();
                drift.released_db = parse_ts_label(&example.db_value);
                drift.released_archive = parse_ts_label(&example.archive_value);
                drift.has_release_drift = true;
            }
            // `active_status` is derived from `released_ts`; every other field is
            // detect-only and not reconciled here.
            _ => {}
        }
    }

    for ((project_slug, reservation_id), drift) in by_reservation {
        // ── Release monotonicity: released wins; sync the lagging store. ──
        if drift.has_release_drift {
            let canonical = match (drift.released_db, drift.released_archive) {
                // Both released but disagree → earliest release wins (monotonic).
                (Some(db), Some(archive)) => Some(db.min(archive)),
                (Some(db), None) => Some(db),
                (None, Some(archive)) => Some(archive),
                (None, None) => None,
            };
            if let Some(canonical_ts) = canonical {
                if drift.released_db != Some(canonical_ts) {
                    // DB lags → record the release in the hot row AND the durable
                    // ledger (integer-only literals; no string interpolation).
                    plan.db_statements.push(format!(
                        "UPDATE file_reservations SET released_ts = {canonical_ts} WHERE id = {reservation_id};"
                    ));
                    plan.db_statements.push(format!(
                        "INSERT OR REPLACE INTO file_reservation_releases (reservation_id, released_ts) VALUES ({reservation_id}, {canonical_ts});"
                    ));
                }
                if drift.released_archive != Some(canonical_ts) {
                    // Archive lags → rewrite only its released_ts.
                    match build_archive_release_rewrite(
                        &finding.storage_root,
                        &project_slug,
                        reservation_id,
                        canonical_ts,
                    ) {
                        Some(rewrite) => plan.archive_rewrites.push(rewrite),
                        None => plan.skipped_ambiguous += 1,
                    }
                }
            }
        }

        // ── Holder: the immutable acquire-time archive wins whenever it diverges
        //    and resolves to a registered agent. The archive records the holder
        //    at acquire time (before any release), so it stays authoritative even
        //    for a row we are concurrently reconciling to released; correcting a
        //    released row's holder is audit-only (it no longer blocks). ──
        if let Some(archive_agent) = drift.agent_archive {
            if let Some(agent_id) = resolve_agent_id(&conn, &project_slug, &archive_agent) {
                plan.db_statements.push(format!(
                    "UPDATE file_reservations SET agent_id = {agent_id} WHERE id = {reservation_id} AND agent_id <> {agent_id};"
                ));
            } else {
                // Genuinely ambiguous: the archive names a holder we cannot map
                // to a registered agent. Refuse to guess.
                plan.skipped_ambiguous += 1;
            }
        }
    }

    plan
}

pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &ReservationDbArchiveParityFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    let mut actions_taken = 0;
    let mut actions_skipped = 0;
    let mut quarantined_paths = Vec::new();

    // ── 1. Principled reconcile of the two #112 drift classes. ──────────────
    let plan = build_reconcile_plan(finding);
    // Batch every integer-only DB statement into ONE hash-witnessed Op::DbExec
    // (a single verbatim DB backup, reversible via `am doctor undo`).
    if !plan.db_statements.is_empty() {
        let sql = plan.db_statements.join("\n");
        let result = mutate(ctx, &finding.db_path, Op::DbExec { sql })?;
        if result.ok {
            actions_taken += 1;
        } else {
            actions_skipped += 1;
        }
    }
    // Rewrite each lagging archive artifact's released_ts (release wins).
    for rewrite in &plan.archive_rewrites {
        let result = mutate(
            ctx,
            &rewrite.path,
            Op::WriteFile {
                content: rewrite.content.clone(),
                mode: rewrite.mode,
            },
        )?;
        if result.ok {
            actions_taken += 1;
        } else {
            actions_skipped += 1;
        }
    }
    actions_skipped += plan.skipped_ambiguous;

    // ── 2. GH#167 cross-project global-id collisions: quarantine the stale
    //       duplicate archive artifact (unchanged). ───────────────────────────
    // Per-entry suffix keeps same-id collisions from different projects from
    // colliding at the quarantine destination.
    let base_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut quarantine_index: u128 = 0;
    for example in &finding.report.examples {
        if example.field != "archive_id_collision" {
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

    // Nothing actionable at all → record a skip so the outcome honestly reflects
    // that drift remains for manual reconciliation.
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
    fn finding_advertises_holder_reconcile_with_health_line_evidence() {
        let (storage_root, db_path) = materialize_fixture(STALE_AGENT_SQL, STALE_AGENT_JSON, 101);
        let finding = detect(Some(storage_root.path()), std::slice::from_ref(&db_path))
            .pop()
            .expect("finding");
        let rendered = finding.to_finding();
        assert_eq!(rendered.id, FM_ID);
        assert_eq!(rendered.severity, "P1");
        // Holder (agent_id) drift is now auto-reconcilable (archive wins).
        assert!(rendered.remediation.auto_fixable);
        assert_eq!(rendered.remediation.estimated_actions, 1);
        assert!(rendered.remediation.command.contains("--yes"));
        assert!(
            rendered
                .evidence
                .get("health_line")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|line| line.contains("reservation_parity: drift"))
        );
        // The reconcile policy is published in the evidence for agents.
        assert!(rendered.evidence.get("reconcile_policy").is_some());
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

    // ── F2 reconcile (br-bvq1x.6.2): before/after parity = 0 drift on F4 ─────

    // A holder mismatch where the archive names an UNREGISTERED agent — genuinely
    // ambiguous, so the fixer must refuse to guess (skip, no mutation).
    const UNRESOLVABLE_AGENT_SQL: &str = "\
CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, human_key TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL);
CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT, inception_ts INTEGER NOT NULL, last_active_ts INTEGER NOT NULL, capabilities TEXT, metadata TEXT, FOREIGN KEY(project_id) REFERENCES projects(id));
CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, path_pattern TEXT NOT NULL, exclusive INTEGER NOT NULL, reason TEXT, created_ts INTEGER NOT NULL, expires_ts INTEGER NOT NULL, released_ts INTEGER, FOREIGN KEY(project_id) REFERENCES projects(id), FOREIGN KEY(agent_id) REFERENCES agents(id));
CREATE TABLE file_reservation_releases (reservation_id INTEGER PRIMARY KEY, released_ts INTEGER NOT NULL, FOREIGN KEY(reservation_id) REFERENCES file_reservations(id));
INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'reservation-regression', '/tmp/reservation-regression', 1700001000000000);
INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, capabilities, metadata) VALUES (2, 1, 'StaleHolder', 'codex-cli', 'gpt-5', 'stale DB holder', 1700001000000000, 1700001000000000, NULL, NULL);
INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) VALUES (101, 1, 2, 'src/reservation.rs', 1, 'br-bvq1x.6.4 stale agent_id fixture', 1700001010000000, 1700004610000000, NULL);
";

    const UNRESOLVABLE_AGENT_JSON: &str = r#"{
  "id": 101,
  "project": "reservation-regression",
  "agent": "PhantomHolder",
  "path_pattern": "src/reservation.rs",
  "exclusive": true,
  "reason": "br-bvq1x.6.4 stale agent_id fixture",
  "created_ts": 1700001010000000,
  "expires_ts": 1700004610000000,
  "released_ts": null
}"#;

    #[test]
    fn fixer_skips_unresolvable_holder_drift() {
        // The archive names a holder that is NOT a registered agent: genuinely
        // ambiguous, so the fixer must refuse to guess (no mutation).
        let (storage_root, db_path) =
            materialize_fixture(UNRESOLVABLE_AGENT_SQL, UNRESOLVABLE_AGENT_JSON, 101);
        let finding = detect(Some(storage_root.path()), std::slice::from_ref(&db_path))
            .pop()
            .expect("finding");
        let ctx = collision_ctx(&storage_root, "2026-06-29T00-00-01Z__unresolvable");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(
            outcome.actions_taken, 0,
            "an unresolvable holder must never be guessed"
        );
        assert!(
            outcome.actions_skipped >= 1,
            "ambiguous row counted as skipped"
        );
        assert!(outcome.quarantined_paths.is_empty());
        // Both stores are left intact, and the drift remains for manual review.
        assert!(
            storage_root
                .path()
                .join("projects/reservation-regression/file_reservations/id-101.json")
                .exists()
        );
        assert_eq!(
            detect(Some(storage_root.path()), std::slice::from_ref(&db_path)).len(),
            1,
            "drift remains for manual reconciliation"
        );
    }

    #[test]
    fn fixer_reconciles_stale_agent_via_archive_winner() {
        // Fixture 1: DB holder StaleHolder, archive holder CorrectHolder (a
        // registered agent). Archive wins → DB agent_id is updated; drift → 0.
        let (storage_root, db_path) = materialize_fixture(STALE_AGENT_SQL, STALE_AGENT_JSON, 101);
        let mut findings = detect(Some(storage_root.path()), std::slice::from_ref(&db_path));
        assert_eq!(findings.len(), 1, "precondition: holder drift present");
        let finding = findings.pop().expect("finding");
        let ctx = collision_ctx(&storage_root, "2026-06-29T00-00-02Z__agent");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 1, "one batched Op::DbExec");
        assert!(outcome.quarantined_paths.is_empty());
        assert!(
            detect(Some(storage_root.path()), std::slice::from_ref(&db_path)).is_empty(),
            "holder reconcile must drive drift to 0"
        );
    }

    #[test]
    fn fixer_reconciles_stuck_null_released_ts_into_db() {
        // Fixture 2: archive recorded the release, the DB stayed stuck-NULL
        // (the #112 over-block). Release wins → the DB hot row + ledger are
        // updated; drift → 0.
        let (storage_root, db_path) = materialize_fixture(STUCK_NULL_SQL, STUCK_NULL_JSON, 201);
        let mut findings = detect(Some(storage_root.path()), std::slice::from_ref(&db_path));
        assert_eq!(findings.len(), 1, "precondition: release drift present");
        let finding = findings.pop().expect("finding");
        let ctx = collision_ctx(&storage_root, "2026-06-29T00-00-03Z__stuck-null");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 1, "one batched Op::DbExec");
        assert!(outcome.quarantined_paths.is_empty());
        assert!(
            detect(Some(storage_root.path()), std::slice::from_ref(&db_path)).is_empty(),
            "release reconcile must drive drift to 0"
        );
    }

    #[test]
    fn fixer_reconciles_stale_active_archive_via_release_winner() {
        // Fixture 3: the DB (hot row + ledger) recorded the release, the archive
        // artifact stayed active. Release wins → the archive's released_ts is
        // rewritten; drift → 0.
        let (storage_root, db_path) =
            materialize_fixture(ARCHIVE_ACTIVE_SQL, ARCHIVE_ACTIVE_JSON, 301);
        let mut findings = detect(Some(storage_root.path()), std::slice::from_ref(&db_path));
        assert_eq!(findings.len(), 1, "precondition: release drift present");
        let finding = findings.pop().expect("finding");
        let ctx = collision_ctx(&storage_root, "2026-06-29T00-00-04Z__archive-active");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 1, "one archive rewrite");
        assert!(outcome.quarantined_paths.is_empty());
        // The archive artifact now carries the release timestamp.
        let archive = std::fs::read_to_string(
            storage_root
                .path()
                .join("projects/reservation-regression/file_reservations/id-301.json"),
        )
        .expect("read archive");
        assert!(
            archive.contains("1700003010000000"),
            "archive released_ts rewritten: {archive}"
        );
        assert!(
            detect(Some(storage_root.path()), std::slice::from_ref(&db_path)).is_empty(),
            "release reconcile must drive drift to 0"
        );
    }

    /// Materializes all three #112 drift modes in a single DB + archive so one
    /// fix run can be proven to reconcile the whole corpus to zero.
    fn materialize_combined_corpus() -> (TempDir, PathBuf) {
        const SQL: &str = "\
CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, human_key TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL);
CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT, inception_ts INTEGER NOT NULL, last_active_ts INTEGER NOT NULL, capabilities TEXT, metadata TEXT, FOREIGN KEY(project_id) REFERENCES projects(id));
CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, path_pattern TEXT NOT NULL, exclusive INTEGER NOT NULL, reason TEXT, created_ts INTEGER NOT NULL, expires_ts INTEGER NOT NULL, released_ts INTEGER, FOREIGN KEY(project_id) REFERENCES projects(id), FOREIGN KEY(agent_id) REFERENCES agents(id));
CREATE TABLE file_reservation_releases (reservation_id INTEGER PRIMARY KEY, released_ts INTEGER NOT NULL, FOREIGN KEY(reservation_id) REFERENCES file_reservations(id));
INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'reservation-regression', '/tmp/reservation-regression', 1700001000000000);
INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, capabilities, metadata) VALUES
  (1, 1, 'CorrectHolder', 'codex-cli', 'gpt-5', 'holder', 1700001000000000, 1700001000000000, NULL, NULL),
  (2, 1, 'StaleHolder', 'codex-cli', 'gpt-5', 'holder', 1700001000000000, 1700001000000000, NULL, NULL),
  (3, 1, 'ReleaseHolder', 'codex-cli', 'gpt-5', 'holder', 1700001000000000, 1700001000000000, NULL, NULL),
  (4, 1, 'ArchiveActiveHolder', 'codex-cli', 'gpt-5', 'holder', 1700001000000000, 1700001000000000, NULL, NULL);
INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) VALUES
  (101, 1, 2, 'src/reservation.rs', 1, 'r101', 1700001010000000, 1700004610000000, NULL),
  (201, 1, 3, 'src/stuck-null.rs', 1, 'r201', 1700002010000000, 1700005610000000, NULL),
  (301, 1, 4, 'src/archive-active.rs', 1, 'r301', 1700003010000000, 1700006610000000, 1700003010000000);
INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (301, 1700003010000000);
";
        let td = TempDir::new().expect("tempdir");
        let db_path = td.path().join("storage.sqlite3");
        let conn = CanonicalDbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(SQL).expect("seed combined corpus");
        drop(conn);

        let dir = td
            .path()
            .join("projects")
            .join("reservation-regression")
            .join("file_reservations");
        std::fs::create_dir_all(&dir).expect("mkdir archive");
        // 101: holder drift (archive names the correct, registered holder).
        std::fs::write(
            dir.join("id-101.json"),
            r#"{
  "id": 101,
  "project": "reservation-regression",
  "agent": "CorrectHolder",
  "path_pattern": "src/reservation.rs",
  "exclusive": true,
  "reason": "r101",
  "created_ts": 1700001010000000,
  "expires_ts": 1700004610000000,
  "released_ts": null
}"#,
        )
        .unwrap();
        // 201: release drift, DB lagging (archive released, DB stuck-NULL).
        std::fs::write(
            dir.join("id-201.json"),
            r#"{
  "id": 201,
  "project": "reservation-regression",
  "agent": "ReleaseHolder",
  "path_pattern": "src/stuck-null.rs",
  "exclusive": true,
  "reason": "r201",
  "created_ts": 1700002010000000,
  "expires_ts": 1700005610000000,
  "released_ts": 1700002010000000
}"#,
        )
        .unwrap();
        // 301: release drift, archive lagging (DB released, archive active).
        std::fs::write(
            dir.join("id-301.json"),
            r#"{
  "id": 301,
  "project": "reservation-regression",
  "agent": "ArchiveActiveHolder",
  "path_pattern": "src/archive-active.rs",
  "exclusive": true,
  "reason": "r301",
  "created_ts": 1700003010000000,
  "expires_ts": 1700006610000000,
  "released_ts": null
}"#,
        )
        .unwrap();
        (td, db_path)
    }

    #[test]
    fn fixer_drives_f4_corpus_to_zero_drift() {
        // The literal F2 acceptance: the whole #112 drift corpus reconciles to
        // zero in a single fix run (holder + both release directions).
        let (storage_root, db_path) = materialize_combined_corpus();
        let before = detect(Some(storage_root.path()), std::slice::from_ref(&db_path));
        assert_eq!(before.len(), 1);
        // 5 drift signals: agent(101) + released(201)+active(201) + released(301)+active(301).
        assert_eq!(
            before[0].report.drift.total(),
            5,
            "{}",
            before[0].report.health_line()
        );
        let finding = detect(Some(storage_root.path()), std::slice::from_ref(&db_path))
            .pop()
            .expect("finding");
        let ctx = collision_ctx(&storage_root, "2026-06-29T00-00-05Z__corpus");
        let outcome = fix(&ctx, &finding).expect("fix");
        // One batched Op::DbExec (holder 101 + release 201) + one archive rewrite (301).
        assert_eq!(
            outcome.actions_taken, 2,
            "one DbExec batch + one archive rewrite"
        );
        assert!(
            detect(Some(storage_root.path()), std::slice::from_ref(&db_path)).is_empty(),
            "the F4 corpus must reconcile to zero drift"
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

    #[test]
    fn fixer_reconciles_combined_holder_and_release_drift_on_one_row() {
        // A single reservation carrying BOTH #112 drift modes (the overlapping
        // 52%-stale-agent / 10%-stuck-NULL-release populations): a stale DB holder
        // AND a release the DB missed. One fix run resolves both → 0 drift, which
        // a holder gate keyed on "active only" would have left half-reconciled.
        const SQL: &str = "\
CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, human_key TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL);
CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT, inception_ts INTEGER NOT NULL, last_active_ts INTEGER NOT NULL, capabilities TEXT, metadata TEXT, FOREIGN KEY(project_id) REFERENCES projects(id));
CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, path_pattern TEXT NOT NULL, exclusive INTEGER NOT NULL, reason TEXT, created_ts INTEGER NOT NULL, expires_ts INTEGER NOT NULL, released_ts INTEGER, FOREIGN KEY(project_id) REFERENCES projects(id), FOREIGN KEY(agent_id) REFERENCES agents(id));
CREATE TABLE file_reservation_releases (reservation_id INTEGER PRIMARY KEY, released_ts INTEGER NOT NULL, FOREIGN KEY(reservation_id) REFERENCES file_reservations(id));
INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'reservation-regression', '/tmp/reservation-regression', 1700001000000000);
INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, capabilities, metadata) VALUES
  (1, 1, 'CorrectHolder', 'codex-cli', 'gpt-5', 'holder', 1700001000000000, 1700001000000000, NULL, NULL),
  (2, 1, 'StaleHolder', 'codex-cli', 'gpt-5', 'holder', 1700001000000000, 1700001000000000, NULL, NULL);
INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) VALUES (401, 1, 2, 'src/combo.rs', 1, 'r401', 1700001010000000, 1700004610000000, NULL);
";
        const JSON: &str = r#"{
  "id": 401,
  "project": "reservation-regression",
  "agent": "CorrectHolder",
  "path_pattern": "src/combo.rs",
  "exclusive": true,
  "reason": "r401",
  "created_ts": 1700001010000000,
  "expires_ts": 1700004610000000,
  "released_ts": 1700004010000000
}"#;
        let (storage_root, db_path) = materialize_fixture(SQL, JSON, 401);
        let before = detect(Some(storage_root.path()), std::slice::from_ref(&db_path));
        assert_eq!(before.len(), 1);
        // agent(401) + released(401) + active(401) = 3 drift signals.
        assert_eq!(
            before[0].report.drift.total(),
            3,
            "{}",
            before[0].report.health_line()
        );
        let finding = detect(Some(storage_root.path()), std::slice::from_ref(&db_path))
            .pop()
            .expect("finding");
        let ctx = collision_ctx(&storage_root, "2026-06-29T00-00-06Z__combo");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(
            outcome.actions_taken, 1,
            "holder + release reconcile in one DbExec batch"
        );
        assert!(
            detect(Some(storage_root.path()), std::slice::from_ref(&db_path)).is_empty(),
            "a row with both drift modes must reconcile to zero"
        );
    }
}
