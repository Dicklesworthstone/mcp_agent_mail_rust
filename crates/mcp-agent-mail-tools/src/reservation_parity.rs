//! Read-only reservation DB/archive parity checks.

use mcp_agent_mail_db::sqlmodel_core::{Row, Value};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const RESERVATION_PARITY_SCHEMA_VERSION: &str = "reservation_db_archive_parity.v1";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReservationParityDriftSummary {
    pub missing_archive_artifacts: usize,
    pub archive_without_db_rows: usize,
    /// Archive `id-<id>.json` artifacts whose reservation id exists in `SQLite` only
    /// under a *different* project.
    ///
    /// `SQLite` reservation ids are global while the archive parity key is
    /// `(project_slug, id)`, so these are stale duplicate artifacts left behind by an
    /// id that was later reused — NOT missing DB rows to insert (GH#167). They are
    /// safe to quarantine, never to reconstruct into `SQLite`.
    pub archive_id_collisions: usize,
    pub agent_id_mismatches: usize,
    pub released_ts_mismatches: usize,
    pub active_status_mismatches: usize,
    pub thread_provenance_mismatches: usize,
    pub parse_errors: usize,
}

impl ReservationParityDriftSummary {
    #[must_use]
    pub const fn total(&self) -> usize {
        self.missing_archive_artifacts
            + self.archive_without_db_rows
            + self.archive_id_collisions
            + self.agent_id_mismatches
            + self.released_ts_mismatches
            + self.active_status_mismatches
            + self.thread_provenance_mismatches
            + self.parse_errors
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReservationParityExample {
    pub reservation_id: i64,
    pub project_slug: String,
    pub field: String,
    pub db_value: String,
    pub archive_value: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReservationParityReport {
    pub schema_version: &'static str,
    pub ok: bool,
    pub db_reservations: usize,
    pub archive_reservations: usize,
    pub drift: ReservationParityDriftSummary,
    pub examples: Vec<ReservationParityExample>,
}

impl ReservationParityReport {
    #[must_use]
    pub fn health_line(&self) -> String {
        if self.ok {
            return format!(
                "reservation_parity: ok db={} archive={} drift=0",
                self.db_reservations, self.archive_reservations
            );
        }

        let mut fields = Vec::new();
        if self.drift.missing_archive_artifacts > 0 {
            fields.push(format!(
                "missing_archive={}",
                self.drift.missing_archive_artifacts
            ));
        }
        if self.drift.archive_without_db_rows > 0 {
            fields.push(format!(
                "archive_without_db={}",
                self.drift.archive_without_db_rows
            ));
        }
        if self.drift.archive_id_collisions > 0 {
            fields.push(format!(
                "archive_id_collision={}",
                self.drift.archive_id_collisions
            ));
        }
        if self.drift.agent_id_mismatches > 0 {
            fields.push(format!("agent_id={}", self.drift.agent_id_mismatches));
        }
        if self.drift.released_ts_mismatches > 0 {
            fields.push(format!("released_ts={}", self.drift.released_ts_mismatches));
        }
        if self.drift.active_status_mismatches > 0 {
            fields.push(format!(
                "active_status={}",
                self.drift.active_status_mismatches
            ));
        }
        if self.drift.thread_provenance_mismatches > 0 {
            fields.push(format!(
                "thread_provenance={}",
                self.drift.thread_provenance_mismatches
            ));
        }
        if self.drift.parse_errors > 0 {
            fields.push(format!("parse_errors={}", self.drift.parse_errors));
        }
        let examples = self
            .examples
            .iter()
            .take(3)
            .map(|example| {
                format!(
                    "{}:{}:{}",
                    example.project_slug, example.reservation_id, example.field
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        format!(
            "reservation_parity: drift total={} db={} archive={} fields=[{}] examples=[{}]",
            self.drift.total(),
            self.db_reservations,
            self.archive_reservations,
            fields.join(","),
            examples
        )
    }
}

#[derive(Debug, Clone)]
struct DbReservationState {
    reservation_id: i64,
    project_slug: String,
    agent_name: String,
    reason: String,
    reservation_released_ts: Option<i64>,
    ledger_released_ts: Option<i64>,
}

impl DbReservationState {
    fn effective_released_ts(&self) -> Option<i64> {
        self.ledger_released_ts.or(self.reservation_released_ts)
    }

    fn active_status(&self) -> &'static str {
        if positive_ts(self.effective_released_ts()) {
            "released"
        } else {
            "active"
        }
    }
}

#[derive(Debug, Clone)]
struct ArchiveReservationState {
    reservation_id: i64,
    project_slug: String,
    agent_name: String,
    thread_provenance: String,
    released_ts: Option<i64>,
}

impl ArchiveReservationState {
    fn active_status(&self) -> &'static str {
        if positive_ts(self.released_ts) {
            "released"
        } else {
            "active"
        }
    }
}

fn positive_ts(ts: Option<i64>) -> bool {
    ts.is_some_and(|value| value > 0)
}

fn ts_label(ts: Option<i64>) -> String {
    ts.map_or_else(|| "NULL".to_string(), |value| value.to_string())
}

fn query_db_reservations_with<F>(mut query: F) -> Result<Vec<DbReservationState>, String>
where
    F: FnMut(&str, &[Value]) -> Result<Vec<Row>, String>,
{
    let rows = query(
        "SELECT fr.id AS reservation_id,
                p.slug AS project_slug,
                COALESCE(a.name, '<missing-agent-id:' || fr.agent_id || '>') AS agent_name,
                COALESCE(fr.reason, '') AS reason,
                fr.released_ts AS reservation_released_ts,
                rr.released_ts AS ledger_released_ts
         FROM file_reservations fr
         JOIN projects p ON p.id = fr.project_id
         LEFT JOIN agents a ON a.id = fr.agent_id AND a.project_id = fr.project_id
         LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id
         ORDER BY p.slug, fr.id",
        &[],
    )?;

    rows.into_iter()
        .map(|row| {
            Ok(DbReservationState {
                reservation_id: row
                    .get_named::<i64>("reservation_id")
                    .map_err(|error| error.to_string())?,
                project_slug: row
                    .get_named::<String>("project_slug")
                    .map_err(|error| error.to_string())?,
                agent_name: row
                    .get_named::<String>("agent_name")
                    .map_err(|error| error.to_string())?,
                reason: row
                    .get_named::<String>("reason")
                    .map_err(|error| error.to_string())?,
                reservation_released_ts: row
                    .get_named::<Option<i64>>("reservation_released_ts")
                    .map_err(|error| error.to_string())?,
                ledger_released_ts: row
                    .get_named::<Option<i64>>("ledger_released_ts")
                    .map_err(|error| error.to_string())?,
            })
        })
        .collect()
}

pub fn check_reservation_parity_with_db_conn(
    conn: &mcp_agent_mail_db::DbConn,
    storage_root: &Path,
) -> Result<ReservationParityReport, String> {
    check_reservation_parity_with_query(
        |sql, params| {
            conn.query_sync(sql, params)
                .map_err(|error| error.to_string())
        },
        storage_root,
    )
}

pub fn check_reservation_parity_with_canonical_conn(
    conn: &mcp_agent_mail_db::CanonicalDbConn,
    storage_root: &Path,
) -> Result<ReservationParityReport, String> {
    check_reservation_parity_with_query(
        |sql, params| {
            conn.query_sync(sql, params)
                .map_err(|error| error.to_string())
        },
        storage_root,
    )
}

fn check_reservation_parity_with_query<F>(
    query: F,
    storage_root: &Path,
) -> Result<ReservationParityReport, String>
where
    F: FnMut(&str, &[Value]) -> Result<Vec<Row>, String>,
{
    let db_reservations = query_db_reservations_with(query)?;
    let archive_scan = scan_archive_reservations(storage_root);
    let archive_reservations = archive_scan.reservations;
    let mut drift = ReservationParityDriftSummary {
        parse_errors: archive_scan.parse_errors.len(),
        ..ReservationParityDriftSummary::default()
    };
    let mut examples = Vec::new();

    for error in archive_scan.parse_errors.into_iter().take(3) {
        examples.push(ReservationParityExample {
            reservation_id: 0,
            project_slug: "<archive>".to_string(),
            field: "parse_error".to_string(),
            db_value: "not_applicable".to_string(),
            archive_value: error.path.display().to_string(),
            detail: error.detail,
        });
    }

    let db_by_key = db_reservations
        .iter()
        .map(|reservation| {
            (
                (reservation.project_slug.clone(), reservation.reservation_id),
                reservation,
            )
        })
        .collect::<BTreeMap<_, _>>();
    // SQLite reservation ids are global; map each id to every project that owns it
    // in the DB so an archive-only artifact can be classified as a genuine missing
    // row vs. a cross-project global-id collision (GH#167). Built from the rows
    // already loaded — no extra query.
    let mut db_projects_by_id: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for reservation in &db_reservations {
        db_projects_by_id
            .entry(reservation.reservation_id)
            .or_default()
            .insert(reservation.project_slug.clone());
    }
    let archive_by_key = archive_reservations
        .iter()
        .map(|reservation| {
            (
                (reservation.project_slug.clone(), reservation.reservation_id),
                reservation,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let keys = db_by_key
        .keys()
        .chain(archive_by_key.keys())
        .cloned()
        .collect::<BTreeSet<_>>();

    for (project_slug, reservation_id) in keys {
        match (
            db_by_key.get(&(project_slug.clone(), reservation_id)),
            archive_by_key.get(&(project_slug.clone(), reservation_id)),
        ) {
            (Some(db), Some(archive)) => {
                compare_reservation_pair(db, archive, &mut drift, &mut examples);
            }
            (Some(_), None) => {
                drift.missing_archive_artifacts += 1;
                examples.push(ReservationParityExample {
                    reservation_id,
                    project_slug,
                    field: "archive_artifact".to_string(),
                    db_value: "present".to_string(),
                    archive_value: "missing".to_string(),
                    detail: format!("reservation_id={reservation_id} missing stable id artifact"),
                });
            }
            (None, Some(_)) => {
                // The archive artifact at (this project, id) has no DB row. If the
                // id exists in SQLite under a *different* project it is a stale
                // duplicate (global id reuse), not a missing row — quarantine, do
                // not reconstruct (GH#167). Any matching id here is necessarily a
                // different project, since a same-project row would land in the
                // (Some, Some) arm above.
                if let Some(db_projects) = db_projects_by_id.get(&reservation_id) {
                    drift.archive_id_collisions += 1;
                    let db_projects_label =
                        db_projects.iter().cloned().collect::<Vec<_>>().join(",");
                    let archive_project = project_slug.clone();
                    examples.push(ReservationParityExample {
                        reservation_id,
                        project_slug,
                        field: "archive_id_collision".to_string(),
                        db_value: db_projects_label.clone(),
                        archive_value: "present".to_string(),
                        detail: format!(
                            "reservation_id={reservation_id} archive artifact under project={archive_project} collides with a DB row owned by project(s)=[{db_projects_label}] (global reservation id reused); stale duplicate archive artifact — quarantine, do not reconstruct"
                        ),
                    });
                } else {
                    drift.archive_without_db_rows += 1;
                    examples.push(ReservationParityExample {
                        reservation_id,
                        project_slug,
                        field: "db_row".to_string(),
                        db_value: "missing".to_string(),
                        archive_value: "present".to_string(),
                        detail: format!(
                            "reservation_id={reservation_id} archive artifact has no DB row"
                        ),
                    });
                }
            }
            (None, None) => {}
        }
    }

    let ok = drift.total() == 0;
    Ok(ReservationParityReport {
        schema_version: RESERVATION_PARITY_SCHEMA_VERSION,
        ok,
        db_reservations: db_reservations.len(),
        archive_reservations: archive_reservations.len(),
        drift,
        examples,
    })
}

fn compare_reservation_pair(
    db: &DbReservationState,
    archive: &ArchiveReservationState,
    drift: &mut ReservationParityDriftSummary,
    examples: &mut Vec<ReservationParityExample>,
) {
    if db.agent_name != archive.agent_name {
        drift.agent_id_mismatches += 1;
        examples.push(ReservationParityExample {
            reservation_id: db.reservation_id,
            project_slug: db.project_slug.clone(),
            field: "agent_id".to_string(),
            db_value: db.agent_name.clone(),
            archive_value: archive.agent_name.clone(),
            detail: format!(
                "reservation_id={} db_agent={} archive_agent={}",
                db.reservation_id, db.agent_name, archive.agent_name
            ),
        });
    }

    let db_released_ts = db.effective_released_ts();
    if db_released_ts != archive.released_ts {
        drift.released_ts_mismatches += 1;
        examples.push(ReservationParityExample {
            reservation_id: db.reservation_id,
            project_slug: db.project_slug.clone(),
            field: "released_ts".to_string(),
            db_value: ts_label(db_released_ts),
            archive_value: ts_label(archive.released_ts),
            detail: format!(
                "reservation_id={} db_released_ts={} archive_released_ts={}",
                db.reservation_id,
                ts_label(db_released_ts),
                ts_label(archive.released_ts)
            ),
        });
    }

    if db.active_status() != archive.active_status() {
        drift.active_status_mismatches += 1;
        examples.push(ReservationParityExample {
            reservation_id: db.reservation_id,
            project_slug: db.project_slug.clone(),
            field: "active_status".to_string(),
            db_value: db.active_status().to_string(),
            archive_value: archive.active_status().to_string(),
            detail: format!(
                "reservation_id={} db_status={} archive_status={}",
                db.reservation_id,
                db.active_status(),
                archive.active_status()
            ),
        });
    }

    if db.reason != archive.thread_provenance {
        drift.thread_provenance_mismatches += 1;
        examples.push(ReservationParityExample {
            reservation_id: db.reservation_id,
            project_slug: db.project_slug.clone(),
            field: "thread_provenance".to_string(),
            db_value: db.reason.clone(),
            archive_value: archive.thread_provenance.clone(),
            detail: format!(
                "reservation_id={} db_thread_provenance={} archive_thread_provenance={}",
                db.reservation_id, db.reason, archive.thread_provenance
            ),
        });
    }
}

#[derive(Debug)]
struct ArchiveScan {
    reservations: Vec<ArchiveReservationState>,
    parse_errors: Vec<ArchiveParseError>,
}

#[derive(Debug)]
struct ArchiveParseError {
    path: PathBuf,
    detail: String,
}

fn scan_archive_reservations(storage_root: &Path) -> ArchiveScan {
    let projects_dir = storage_root.join("projects");
    let mut reservations = Vec::new();
    let mut parse_errors = Vec::new();
    let Ok(project_entries) = std::fs::read_dir(&projects_dir) else {
        return ArchiveScan {
            reservations,
            parse_errors,
        };
    };

    for project_entry in project_entries.flatten() {
        let project_path = project_entry.path();
        if path_is_symlink(&project_path)
            || !project_entry
                .file_type()
                .is_ok_and(|file_type| file_type.is_dir())
        {
            continue;
        }
        let Some(project_slug) = project_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let reservation_dir = project_path.join("file_reservations");
        let Ok(entries) = std::fs::read_dir(&reservation_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path_is_symlink(&path)
                || !entry.file_type().is_ok_and(|file_type| file_type.is_file())
                || path.extension().is_none_or(|extension| extension != "json")
            {
                continue;
            }
            let Some(raw_id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("id-"))
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            let Ok(reservation_id) = raw_id.parse::<i64>() else {
                continue;
            };
            if reservation_id <= 0 {
                continue;
            }
            match parse_archive_reservation(&path, &project_slug, reservation_id) {
                Ok(reservation) => reservations.push(reservation),
                Err(detail) => parse_errors.push(ArchiveParseError { path, detail }),
            }
        }
    }

    reservations.sort_by(|left, right| {
        left.project_slug
            .cmp(&right.project_slug)
            .then(left.reservation_id.cmp(&right.reservation_id))
    });
    ArchiveScan {
        reservations,
        parse_errors,
    }
}

fn parse_archive_reservation(
    path: &Path,
    project_slug: &str,
    reservation_id: i64,
) -> Result<ArchiveReservationState, String> {
    let content = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let json: serde_json::Value =
        serde_json::from_str(&content).map_err(|error| error.to_string())?;
    let json_id = json
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| "id is missing or not an integer".to_string())?;
    if json_id != reservation_id {
        return Err(format!(
            "file name id {reservation_id} does not match JSON id {json_id}"
        ));
    }
    let agent_name =
        json_string(&json, "agent").ok_or_else(|| "agent is missing or blank".to_string())?;
    let thread_provenance = json_string(&json, "thread_id")
        .or_else(|| json_string(&json, "thread"))
        .or_else(|| json_string(&json, "reason"))
        .unwrap_or_default();
    let released_ts = parse_json_micros(&json, "released_ts");

    Ok(ArchiveReservationState {
        reservation_id,
        project_slug: project_slug.to_string(),
        agent_name,
        thread_provenance,
        released_ts,
    })
}

fn json_string(json: &serde_json::Value, key: &str) -> Option<String> {
    json.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_json_micros(json: &serde_json::Value, key: &str) -> Option<i64> {
    match json.get(key)? {
        serde_json::Value::Number(number) => number.as_i64(),
        serde_json::Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                trimmed
                    .parse::<i64>()
                    .ok()
                    .or_else(|| mcp_agent_mail_db::iso_to_micros(trimmed))
            }
        }
        _ => None,
    }
}

fn path_is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}
