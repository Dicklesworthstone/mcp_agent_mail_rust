//! `fm-archive-state-files-reservation-artifact-generation-normalize` — P1.
//!
//! ## What's broken
//!
//! Reservation archive artifacts written before br-n8qh6 use the legacy
//! `id-<rowid>.json` key. A replacement mailbox database can reuse that rowid,
//! so an old artifact is indistinguishable from a new reservation. Generation
//! stamped artifacts avoid that collision with `id-<rowid>-g<generation>.json`.
//!
//! ## Fix
//!
//! This fixer only acts when it can establish the artifact's identity from its
//! filename and JSON plus a freshly revalidated WAL-aware logical DB source:
//!
//! - A legacy artifact is stamped and renamed only when its `(project, id)` has
//!   a live DB reservation in the current generation.
//! - An artifact whose filename generation (or, for legacy names, JSON
//!   `db_generation`) differs from the current DB generation is quarantined.
//! - Malformed artifacts, symlinks, and legacy artifacts without a matching
//!   live DB row are left untouched for operator review.
//!
//! Every write uses the doctor `mutate()` chokepoint. Migration first writes
//! the generation stamp in place and then uses `Op::Rename`; foreign debris is
//! quarantined with `Op::Rename`, preserving bytes and making `am doctor undo`
//! restore the original path exactly.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use mcp_agent_mail_core::reservation_artifact::{
    parse_reservation_artifact_filename, reservation_artifact_filename,
};
use serde::Serialize;
use serde_json::Value;
use sqlmodel_sqlite::SqliteConnection;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-archive-state-files-reservation-artifact-generation-normalize";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "archive_state_files";
const PROJECTS_DIR: &str = "projects";
const RESERVATIONS_DIR: &str = "file_reservations";

#[derive(Debug, Clone, Serialize)]
pub struct ReservationArtifactNormalizeFinding {
    pub db_path: PathBuf,
    pub storage_root: PathBuf,
    pub current_generation: String,
    legacy_migrations: Vec<LegacyMigration>,
    quarantines: Vec<QuarantineArtifact>,
}

impl ReservationArtifactNormalizeFinding {
    pub fn to_finding(&self) -> super::Finding {
        let migrations = self.legacy_migrations.len();
        let foreign = self
            .quarantines
            .iter()
            .filter(|artifact| artifact.reason == QuarantineReason::ForeignGeneration)
            .count();
        let duplicates = self.quarantines.len().saturating_sub(foreign);
        let estimated_actions = migrations * 2 + self.quarantines.len();
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title: format!(
                "reservation archive generation normalization: {migrations} legacy artifact(s), {foreign} foreign-generation artifact(s), {duplicates} duplicate legacy artifact(s) in {}",
                self.db_path.display(),
            ),
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "storage_root": self.storage_root.to_string_lossy(),
                "current_generation": self.current_generation,
                "legacy_migrations": self.legacy_migrations.iter().map(|migration| serde_json::json!({
                    "source": migration.source.to_string_lossy(),
                    "destination": migration.destination.to_string_lossy(),
                    "project": &migration.project,
                    "reservation_id": migration.reservation_id,
                })).collect::<Vec<_>>(),
                "quarantines": self.quarantines.iter().map(|artifact| serde_json::json!({
                    "source": artifact.source.to_string_lossy(),
                    "reason": artifact.reason,
                })).collect::<Vec<_>>(),
                "safety_policy": "Only a regular JSON artifact whose embedded id and project match its path is eligible. Legacy names additionally require a live (project, id) DB row. Filename generation wins over JSON db_generation; malformed, symlinked, and unmatched artifacts remain untouched.",
            }),
            remediation: FindingRemediation {
                command: format!("am doctor fix --only {FM_ID} --yes"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: true,
                estimated_actions,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct LegacyMigration {
    source: PathBuf,
    destination: PathBuf,
    project: String,
    reservation_id: i64,
    content: Vec<u8>,
    mode: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum QuarantineReason {
    ForeignGeneration,
    LegacyDuplicateOfStampedArtifact,
}

#[derive(Debug, Clone, Serialize)]
struct QuarantineArtifact {
    source: PathBuf,
    project: String,
    reason: QuarantineReason,
}

/// Detect reservation artifacts that can be safely normalized for each stable
/// DB read candidate. A DB without a seeded generation identity is left
/// alone: this read-only detector must never mint an identity token itself.
pub fn detect(
    storage_root: Option<&Path>,
    candidate_dbs: &[PathBuf],
) -> Vec<ReservationArtifactNormalizeFinding> {
    let read_candidates = super::explicit_offline_db_read_candidates(
        candidate_dbs,
        "reservation artifact normalization detection",
    );
    detect_prepared(storage_root, &read_candidates)
}

pub(crate) fn detect_prepared(
    storage_root: Option<&Path>,
    read_candidates: &[super::DoctorDbReadCandidate],
) -> Vec<ReservationArtifactNormalizeFinding> {
    let Some(storage_root) = storage_root else {
        return Vec::new();
    };
    if !is_non_symlink_dir(storage_root) {
        return Vec::new();
    }

    let mut findings = Vec::new();
    for candidate in read_candidates {
        let Some(conn) = candidate.connection() else {
            continue;
        };
        let Some(current_generation) = read_current_generation(conn) else {
            continue;
        };
        let Some(live_reservations) = read_live_reservations(conn) else {
            continue;
        };
        let (legacy_migrations, quarantines) =
            scan_artifacts(storage_root, &current_generation, &live_reservations);
        if legacy_migrations.is_empty() && quarantines.is_empty() {
            continue;
        }
        findings.push(ReservationArtifactNormalizeFinding {
            db_path: candidate.target_path().to_path_buf(),
            storage_root: storage_root.to_path_buf(),
            current_generation,
            legacy_migrations,
            quarantines,
        });
    }
    findings
}

/// Apply a previously detected plan through the hash-witnessed doctor
/// chokepoint. A vanished artifact or a newly-present target is counted as a
/// skip instead of risking an overwrite.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &ReservationArtifactNormalizeFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    let candidate = super::DoctorDbReadCandidate::open_live_or_explicit_offline(
        &finding.db_path,
        "reservation artifact normalization pre-fix source selection",
    );
    fix_prepared(ctx, finding, &candidate)
}

pub(crate) fn fix_prepared(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &ReservationArtifactNormalizeFinding,
    candidate: &super::DoctorDbReadCandidate,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    let refreshed = candidate.refresh("reservation artifact pre-mutation revalidation");
    let Some(fresh_finding) = detect_prepared(
        Some(&finding.storage_root),
        std::slice::from_ref(&refreshed),
    )
    .into_iter()
    .next() else {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    };

    let mut outcome = FixOutcome::default();

    for migration in &fresh_finding.legacy_migrations {
        if !is_regular_file(&migration.source) || migration.destination.exists() {
            outcome.actions_skipped += 1;
            continue;
        }

        // Write the stamp to the source first. The following rename has the
        // no-clobber guarantee built into `mutate(Op::Rename)`, so a concurrent
        // target appearance is refused instead of overwriting an artifact.
        let write = mutate(
            ctx,
            &migration.source,
            Op::WriteFile {
                content: migration.content.clone(),
                mode: migration.mode,
            },
        )?;
        if !write.ok {
            outcome.actions_skipped += 1;
            continue;
        }
        outcome.actions_taken += 1;

        let rename = mutate(
            ctx,
            &migration.source,
            Op::Rename {
                to: migration.destination.clone(),
            },
        )?;
        if rename.ok {
            outcome.actions_taken += 1;
        } else {
            outcome.actions_skipped += 1;
        }
    }

    for artifact in &fresh_finding.quarantines {
        if !is_regular_file(&artifact.source) {
            outcome.actions_skipped += 1;
            continue;
        }
        let quarantine = quarantine_destination(ctx, artifact);
        let result = mutate(
            ctx,
            &artifact.source,
            Op::Rename {
                to: quarantine.clone(),
            },
        )?;
        if result.ok {
            outcome.actions_taken += 1;
            outcome.quarantined_paths.push(quarantine);
        } else {
            outcome.actions_skipped += 1;
        }
    }

    Ok(outcome)
}

fn read_current_generation(conn: &SqliteConnection) -> Option<String> {
    let rows = conn
        .query_sync(
            "SELECT generation_id FROM db_identity WHERE singleton = 0",
            &[],
        )
        .ok()?;
    rows.first()?
        .get_named::<String>("generation_id")
        .ok()
        .filter(|generation| !generation.is_empty())
}

fn read_live_reservations(conn: &SqliteConnection) -> Option<HashSet<(String, i64)>> {
    let rows = conn
        .query_sync(
            "SELECT p.slug AS project_slug, fr.id AS reservation_id
             FROM file_reservations fr
             JOIN projects p ON p.id = fr.project_id",
            &[],
        )
        .ok()?;
    let mut reservations = HashSet::with_capacity(rows.len());
    for row in rows {
        let project = row.get_named::<String>("project_slug").ok()?;
        let id = row.get_named::<i64>("reservation_id").ok()?;
        if id > 0 {
            reservations.insert((project, id));
        }
    }
    Some(reservations)
}

fn scan_artifacts(
    storage_root: &Path,
    current_generation: &str,
    live_reservations: &HashSet<(String, i64)>,
) -> (Vec<LegacyMigration>, Vec<QuarantineArtifact>) {
    let projects = storage_root.join(PROJECTS_DIR);
    if !is_non_symlink_dir(&projects) {
        return (Vec::new(), Vec::new());
    }

    let mut migrations = Vec::new();
    let mut quarantines = Vec::new();
    let Ok(projects) = std::fs::read_dir(projects) else {
        return (migrations, quarantines);
    };

    for project_entry in projects.flatten() {
        let Ok(file_type) = project_entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Some(project) = project_entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let reservation_dir = project_entry.path().join(RESERVATIONS_DIR);
        if !is_non_symlink_dir(&reservation_dir) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(reservation_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(parsed) = parse_reservation_artifact_filename(&name) else {
                continue;
            };
            let path = entry.path();
            let Some(mut json) = read_matching_artifact_json(&path, parsed.id, &project) else {
                continue;
            };
            let content_generation = json
                .get("db_generation")
                .and_then(Value::as_str)
                .filter(|generation| !generation.is_empty());
            // The filename is the authoritative key. JSON is a fallback for
            // legacy names written during the transition.
            let artifact_generation = parsed.generation.as_deref().or(content_generation);
            if artifact_generation.is_some_and(|generation| generation != current_generation) {
                quarantines.push(QuarantineArtifact {
                    source: path,
                    project: project.clone(),
                    reason: QuarantineReason::ForeignGeneration,
                });
                continue;
            }
            if parsed.generation.is_some() {
                continue;
            }
            if !live_reservations.contains(&(project.clone(), parsed.id)) {
                continue;
            }

            let destination = path.with_file_name(reservation_artifact_filename(
                Some(current_generation),
                parsed.id,
            ));
            if destination.exists() {
                if is_matching_current_artifact(
                    &destination,
                    parsed.id,
                    &project,
                    current_generation,
                ) {
                    quarantines.push(QuarantineArtifact {
                        source: path,
                        project: project.clone(),
                        reason: QuarantineReason::LegacyDuplicateOfStampedArtifact,
                    });
                }
                continue;
            }

            let Some(object) = json.as_object_mut() else {
                continue;
            };
            object.insert(
                "db_generation".to_string(),
                Value::String(current_generation.to_string()),
            );
            let Ok(mut content) = serde_json::to_vec_pretty(&json) else {
                continue;
            };
            content.push(b'\n');
            migrations.push(LegacyMigration {
                source: path,
                destination,
                project: project.clone(),
                reservation_id: parsed.id,
                content,
                mode: file_mode(&entry.path()),
            });
        }
    }
    (migrations, quarantines)
}

fn read_matching_artifact_json(path: &Path, id: i64, project: &str) -> Option<Value> {
    let raw = std::fs::read(path).ok()?;
    let json: Value = serde_json::from_slice(&raw).ok()?;
    let object = json.as_object()?;
    (object.get("id")?.as_i64()? == id && object.get("project")?.as_str()? == project)
        .then_some(json)
}

fn is_matching_current_artifact(
    path: &Path,
    id: i64,
    project: &str,
    current_generation: &str,
) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(parsed) = parse_reservation_artifact_filename(name) else {
        return false;
    };
    parsed.id == id
        && parsed.generation.as_deref() == Some(current_generation)
        && read_matching_artifact_json(path, id, project).is_some()
}

fn quarantine_destination(
    ctx: &crate::doctor::mutate::MutateContext,
    artifact: &QuarantineArtifact,
) -> PathBuf {
    let bucket = match artifact.reason {
        QuarantineReason::ForeignGeneration => "reservation-foreign-generation",
        QuarantineReason::LegacyDuplicateOfStampedArtifact => "reservation-legacy-duplicates",
    };
    let name = artifact
        .source
        .file_name()
        .expect("scanned reservation artifact has a filename");
    ctx.run_dir
        .join("quarantine")
        .join(bucket)
        .join(&artifact.project)
        .join(name)
}

fn is_non_symlink_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_dir() && !metadata.file_type().is_symlink())
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_file() && !metadata.file_type().is_symlink())
}

#[cfg(unix)]
fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o777)
        .unwrap_or(0o644)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> u32 {
    0o644
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::mutate::{Capabilities, MutateContext};
    use mcp_agent_mail_db::CanonicalDbConn;
    use tempfile::TempDir;

    const CURRENT_GENERATION: &str = "aa11bb22";
    const FOREIGN_GENERATION: &str = "cc33dd44";
    const WAL_WRITER_PATH_ENV: &str = "AM_DOCTOR_GENERATION_WAL_WRITER_PATH";
    const WAL_WRITER_READY_ENV: &str = "AM_DOCTOR_GENERATION_WAL_WRITER_READY";
    const WAL_WRITER_RELEASE_ENV: &str = "AM_DOCTOR_GENERATION_WAL_WRITER_RELEASE";
    const WAL_WRITER_TEST: &str =
        "doctor::fixers::reservation_artifact_normalize::tests::wal_generation_truth_replaces_stale_quarantine_plan_before_mutation";
    const WAL_WRITER_WITNESS: &str = "GENERATION_WAL_WRITER_CHILD_RAN";

    fn fixture() -> (TempDir, PathBuf, PathBuf) {
        let td = TempDir::new().expect("tempdir");
        let db_path = td.path().join("storage.sqlite3");
        let conn = CanonicalDbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        conn.execute_raw(&format!(
            "CREATE TABLE db_identity (singleton INTEGER PRIMARY KEY CHECK (singleton = 0), generation_id TEXT NOT NULL);
             CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE);
             CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL);
             INSERT INTO db_identity (singleton, generation_id) VALUES (0, '{CURRENT_GENERATION}');
             INSERT INTO projects (id, slug) VALUES (1, 'live-project');
             INSERT INTO file_reservations (id, project_id) VALUES (7, 1);"
        ))
        .expect("seed");
        drop(conn);

        let reservation_dir = td
            .path()
            .join(PROJECTS_DIR)
            .join("live-project")
            .join(RESERVATIONS_DIR);
        std::fs::create_dir_all(&reservation_dir).expect("mkdir");
        std::fs::write(
            reservation_dir.join("id-7.json"),
            r#"{"id":7,"project":"live-project","agent":"legacy"}"#,
        )
        .expect("legacy artifact");
        std::fs::write(
            reservation_dir.join(format!("id-8-g{FOREIGN_GENERATION}.json")),
            format!(
                r#"{{"id":8,"project":"live-project","db_generation":"{FOREIGN_GENERATION}"}}"#
            ),
        )
        .expect("foreign artifact");
        // This artifact has no live DB row, so normalization must refuse to
        // turn it into a current-generation claim.
        std::fs::write(
            reservation_dir.join("id-9.json"),
            r#"{"id":9,"project":"live-project","agent":"unmatched"}"#,
        )
        .expect("unmatched artifact");
        (td, db_path, reservation_dir)
    }

    fn ctx(td: &TempDir) -> MutateContext {
        let run_dir = td.path().join(".doctor/runs/normalize");
        std::fs::create_dir_all(&run_dir).expect("run dir");
        let actions = std::fs::File::create(run_dir.join("actions.jsonl")).expect("actions");
        MutateContext {
            run_id: "normalize".to_string(),
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
    fn detector_plans_only_proven_legacy_migration_and_foreign_quarantine() {
        let (td, db_path, _) = fixture();
        let findings = detect(Some(td.path()), std::slice::from_ref(&db_path));
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.current_generation, CURRENT_GENERATION);
        assert_eq!(finding.legacy_migrations.len(), 1);
        assert_eq!(finding.legacy_migrations[0].reservation_id, 7);
        assert_eq!(finding.quarantines.len(), 1);
        assert_eq!(
            finding.quarantines[0].reason,
            QuarantineReason::ForeignGeneration
        );
        let rendered = finding.to_finding();
        assert!(rendered.remediation.auto_fixable);
        assert_eq!(rendered.remediation.estimated_actions, 3);
    }

    #[test]
    fn wal_generation_truth_replaces_stale_quarantine_plan_before_mutation() {
        if let Ok(db_path) = std::env::var(WAL_WRITER_PATH_ENV) {
            let ready_path = PathBuf::from(
                std::env::var(WAL_WRITER_READY_ENV).expect("generation WAL writer ready path"),
            );
            let release_path = PathBuf::from(
                std::env::var(WAL_WRITER_RELEASE_ENV).expect("generation WAL writer release path"),
            );
            let writer = mcp_agent_mail_db::DbConn::open_file(db_path)
                .expect("open cross-process generation WAL writer");
            writer
                .execute_raw(&format!(
                    "PRAGMA wal_autocheckpoint = 0;
                     UPDATE db_identity SET generation_id = '{FOREIGN_GENERATION}'
                     WHERE singleton = 0;"
                ))
                .expect("commit current generation only to WAL");
            std::fs::write(&ready_path, b"ready")
                .expect("publish generation WAL writer readiness");
            println!("{WAL_WRITER_WITNESS}");
            assert!(
                super::super::wait_for_cross_process_release(&release_path),
                "parent did not release generation WAL writer in time"
            );
            drop(writer);
            return;
        }

        let (td, db_path, reservation_dir) = fixture();
        let admitted = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("admit generation fixture through FrankenSQLite");
        admitted
            .execute_raw(
                "PRAGMA journal_mode = WAL;
                 PRAGMA wal_autocheckpoint = 0;
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .expect("settle generation baseline");
        mcp_agent_mail_db::close_db_conn(admitted, "settle generation WAL baseline");

        let stale_candidate = super::super::DoctorDbReadCandidate::open_live_or_explicit_offline(
            &db_path,
            "stale generation plan test",
        );
        let stale_finding =
            detect_prepared(Some(td.path()), std::slice::from_ref(&stale_candidate))
                .pop()
                .expect("initial generation should produce a plan");
        assert!(stale_finding.quarantines.iter().any(|artifact| {
            artifact.source == reservation_dir.join(format!("id-8-g{FOREIGN_GENERATION}.json"))
        }));

        let main_before = std::fs::read(&db_path).expect("read settled generation main");
        let ready_path = td.path().join("generation-wal-writer.ready");
        let release_path = td.path().join("generation-wal-writer.release");
        let child = std::process::Command::new(
            std::env::current_exe().expect("resolve generation test executable"),
        )
        .arg(WAL_WRITER_TEST)
        .arg("--exact")
        .arg("--nocapture")
        .env(WAL_WRITER_PATH_ENV, &db_path)
        .env(WAL_WRITER_READY_ENV, &ready_path)
        .env(WAL_WRITER_RELEASE_ENV, &release_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cross-process generation WAL writer");
        let child = super::super::CrossProcessWalChild::new(child, release_path);
        if !super::super::wait_for_cross_process_signal(&ready_path) {
            let output = child
                .release_and_wait()
                .expect("collect unready generation WAL writer");
            panic!(
                "generation WAL writer never became ready: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let main_after = std::fs::read(&db_path);
        let wal_len = std::fs::metadata(PathBuf::from(format!("{}-wal", db_path.display())))
            .map(|metadata| metadata.len());

        let foreign_artifact = reservation_dir.join(format!("id-8-g{FOREIGN_GENERATION}.json"));
        let foreign_before = std::fs::read(&foreign_artifact).expect("read generation artifact");
        let outcome = fix_prepared(&ctx(&td), &stale_finding, &stale_candidate)
            .expect("refresh generation immediately before mutation");

        let output = child
            .release_and_wait()
            .expect("collect generation WAL writer");
        assert!(
            output.status.success(),
            "generation WAL writer failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(WAL_WRITER_WITNESS),
            "generation WAL writer filter was vacuous: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            main_after.expect("read WAL-only generation main"),
            main_before,
            "fixture must keep the new generation out of the main image"
        );
        assert!(
            wal_len.is_ok_and(|len| len > 32),
            "fixture must retain committed generation WAL frames"
        );
        assert!(
            outcome.quarantined_paths.is_empty(),
            "an artifact matching the current WAL generation must not be quarantined"
        );
        assert_eq!(
            std::fs::read(&foreign_artifact).expect("read retained current artifact"),
            foreign_before
        );
        assert!(
            reservation_dir
                .join(format!("id-7-g{FOREIGN_GENERATION}.json"))
                .exists(),
            "the fresh generation should drive the still-valid legacy migration"
        );
        assert!(
            !reservation_dir
                .join(format!("id-7-g{CURRENT_GENERATION}.json"))
                .exists(),
            "the stale generation must not be stamped into a filename"
        );
    }

    #[test]
    fn fixer_stamps_renames_and_quarantines_through_mutate() {
        let (td, db_path, reservation_dir) = fixture();
        let finding = detect(Some(td.path()), std::slice::from_ref(&db_path))
            .pop()
            .expect("finding");
        let context = ctx(&td);
        let outcome = fix(&context, &finding).expect("fix");

        assert_eq!(outcome.actions_taken, 3);
        assert_eq!(outcome.quarantined_paths.len(), 1);
        assert!(!reservation_dir.join("id-7.json").exists());
        let normalized = reservation_dir.join(format!("id-7-g{CURRENT_GENERATION}.json"));
        let normalized_json: Value =
            serde_json::from_slice(&std::fs::read(&normalized).expect("normalized artifact"))
                .expect("normalized json");
        assert_eq!(
            normalized_json.get("db_generation").and_then(Value::as_str),
            Some(CURRENT_GENERATION)
        );
        assert!(reservation_dir.join("id-9.json").exists());
        assert!(outcome.quarantined_paths[0].exists());
        assert!(
            outcome.quarantined_paths[0]
                .to_string_lossy()
                .contains("reservation-foreign-generation")
        );
        let actions =
            std::fs::read_to_string(context.run_dir.join("actions.jsonl")).expect("actions");
        assert_eq!(actions.matches("\"op\":\"WriteFile\"").count(), 2);
        assert_eq!(actions.matches("\"op\":\"Rename\"").count(), 4);
    }

    #[test]
    fn legacy_filename_with_foreign_content_generation_is_quarantined() {
        let (td, db_path, reservation_dir) = fixture();
        let foreign_legacy = reservation_dir.join("id-10.json");
        std::fs::write(
            &foreign_legacy,
            format!(
                r#"{{"id":10,"project":"live-project","db_generation":"{FOREIGN_GENERATION}"}}"#
            ),
        )
        .expect("foreign legacy artifact");

        let finding = detect(Some(td.path()), std::slice::from_ref(&db_path))
            .pop()
            .expect("finding");
        assert!(finding.quarantines.iter().any(|artifact| {
            artifact.source == foreign_legacy
                && artifact.reason == QuarantineReason::ForeignGeneration
        }));

        let outcome = fix(&ctx(&td), &finding).expect("fix");
        assert!(!foreign_legacy.exists());
        assert!(outcome.quarantined_paths.iter().any(|path| {
            path.file_name().is_some_and(|name| name == "id-10.json")
                && path
                    .to_string_lossy()
                    .contains("reservation-foreign-generation")
        }));
    }

    #[test]
    fn legacy_duplicate_is_quarantined_only_when_stamped_peer_matches() {
        let (td, db_path, reservation_dir) = fixture();
        std::fs::write(
            reservation_dir.join(format!("id-7-g{CURRENT_GENERATION}.json")),
            format!(
                r#"{{"id":7,"project":"live-project","db_generation":"{CURRENT_GENERATION}"}}"#
            ),
        )
        .expect("stamped peer");
        let finding = detect(Some(td.path()), std::slice::from_ref(&db_path))
            .pop()
            .expect("finding");
        assert!(finding.legacy_migrations.is_empty());
        assert!(finding.quarantines.iter().any(|artifact| {
            artifact.reason == QuarantineReason::LegacyDuplicateOfStampedArtifact
        }));
        let outcome = fix(&ctx(&td), &finding).expect("fix");
        assert!(!reservation_dir.join("id-7.json").exists());
        assert!(
            reservation_dir
                .join(format!("id-7-g{CURRENT_GENERATION}.json"))
                .exists()
        );
        assert_eq!(outcome.actions_taken, 2);
    }
}
