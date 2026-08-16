//! Read-only reservation DB/archive parity checks.

use mcp_agent_mail_db::sqlmodel_core::{Row, Value};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub const RESERVATION_PARITY_SCHEMA_VERSION: &str = "reservation_db_archive_parity.v1";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReservationParityDriftSummary {
    pub missing_archive_artifacts: usize,
    pub archive_without_db_rows: usize,
    /// Archive `id-<id>.json` artifacts for *released* reservations that have no
    /// DB row. These are EXPECTED, not drift: the retention prune
    /// (`prune_released_file_reservations`) hard-deletes released reservations
    /// from `SQLite` while the git archive retains the full audit history
    /// independently. Tracked for visibility but deliberately excluded from
    /// `total()` so that routine retention never reports parity drift (br-5xbua).
    pub pruned_released_archived: usize,
    /// *Released* DB reservation rows with no archive artifact for the live DB
    /// generation. These are EXPECTED bookkeeping, not drift (GH#244): a
    /// released row is not a lock hazard (the release healer's own rule,
    /// br-74sxo, is "a missing artifact needs no heal — there is nothing for
    /// the guard to honor"), the retention prune will hard-delete the row from
    /// `SQLite` in due course, and the artifact frequently still exists under a
    /// *prior* generation stamp (`id-<id>-g<gen>.json`) after
    /// reconstruct-from-archive imported the row into a freshly-minted
    /// generation. Counting these as hard drift made `am doctor health` fail
    /// permanently on a healthy mailbox with no fixer able to clear it (the
    /// mirror of GH#173's archive-side rule, br-5xbua). Tracked for visibility
    /// and deliberately excluded from `total()`. Only an *active* row missing
    /// its artifact remains drift — and that direction self-heals via F1
    /// reconcile-on-read on the next reservation read.
    pub released_missing_archive: usize,
    /// Archive `id-<id>.json` artifacts whose reservation id exists in `SQLite` only
    /// under a *different* project.
    ///
    /// `SQLite` reservation ids are global while the archive parity key is
    /// `(project_slug, id)`, so these are stale duplicate artifacts left behind by an
    /// id that was later reused — NOT missing DB rows to insert (GH#167). They are
    /// safe to quarantine, never to reconstruct into `SQLite`.
    pub archive_id_collisions: usize,
    /// Archive `id-<id>-g<generation>.json` artifacts whose embedded generation
    /// token does NOT match the live database's generation (br-n8qh6). These are
    /// debris from a *prior* DB generation (the DB was wiped and re-created); the
    /// stable generation-stamped name guarantees they can never overwrite or
    /// collide with the current generation's artifacts, so they are NOT drift and
    /// must never be reconstructed into the live DB. Tracked for visibility and
    /// deliberately excluded from `total()` — a clean parity run may still list
    /// prior-generation artifacts. The archive-normalize reservation fixer
    /// quarantines them.
    pub foreign_generation_artifacts: usize,
    pub agent_id_mismatches: usize,
    pub released_ts_mismatches: usize,
    pub active_status_mismatches: usize,
    /// The reserved path glob diverges between the DB row and its archive
    /// artifact (GH#112's core concern — the reserved *path* is the subject of
    /// reservation DB↔archive divergence). Only counted when the archive
    /// artifact actually carries a `path_pattern`; a legacy artifact that omits
    /// it is not drift (br-xyy95), mirroring the conservative comparison used
    /// for `released_ts`/`reason` so absence never manufactures a false drift.
    pub path_pattern_mismatches: usize,
    /// The `exclusive` flag diverges between the DB row and its archive
    /// artifact. Like `path_pattern_mismatches`, only counted when the archive
    /// artifact carries an `exclusive` value (br-xyy95).
    pub exclusive_mismatches: usize,
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
            + self.path_pattern_mismatches
            + self.exclusive_mismatches
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
            // `pruned_released_archived` (retention), `released_missing_archive`
            // (released rows awaiting retention, GH#244) and
            // `foreign_generation_artifacts` (prior-DB-generation debris,
            // br-n8qh6) are expected, not drift, so they are reported only as
            // informational suffixes when non-zero.
            let mut suffix = String::new();
            if self.drift.pruned_released_archived > 0 {
                let _ = write!(
                    suffix,
                    " pruned_released_archived={}",
                    self.drift.pruned_released_archived
                );
            }
            if self.drift.released_missing_archive > 0 {
                let _ = write!(
                    suffix,
                    " released_missing_archive={}",
                    self.drift.released_missing_archive
                );
            }
            if self.drift.foreign_generation_artifacts > 0 {
                let _ = write!(
                    suffix,
                    " foreign_generation_artifacts={}",
                    self.drift.foreign_generation_artifacts
                );
            }
            return format!(
                "reservation_parity: ok db={} archive={} drift=0{suffix}",
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
        if self.drift.path_pattern_mismatches > 0 {
            fields.push(format!(
                "path_pattern={}",
                self.drift.path_pattern_mismatches
            ));
        }
        if self.drift.exclusive_mismatches > 0 {
            fields.push(format!("exclusive={}", self.drift.exclusive_mismatches));
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
    path_pattern: String,
    exclusive: bool,
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
    /// The DB generation token parsed from the artifact filename
    /// (`id-<id>-g<generation>.json`), or `None` for a legacy `id-<id>.json`
    /// artifact that predates generation stamping (br-n8qh6).
    generation: Option<String>,
    agent_name: String,
    thread_provenance: String,
    /// `None` when the archive artifact omits `path_pattern`/`path` entirely
    /// (legacy or hand-authored). Absence is NOT drift — only a present-but-
    /// divergent value is (br-xyy95).
    path_pattern: Option<String>,
    /// `None` when the archive artifact omits `exclusive` (br-xyy95).
    exclusive: Option<bool>,
    released_ts: Option<i64>,
    /// `None` when the archive artifact omits `expires_ts` (absence is never
    /// drift, br-xyy95). Consumed by reconcile-on-read so a stale pre-renew
    /// artifact heals; the parity drift report intentionally does not count it.
    expires_ts: Option<i64>,
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
                COALESCE(fr.path_pattern, '') AS path_pattern,
                fr.exclusive AS exclusive,
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
                path_pattern: row
                    .get_named::<String>("path_pattern")
                    .map_err(|error| error.to_string())?,
                exclusive: row
                    .get_named::<i64>("exclusive")
                    .map_err(|error| error.to_string())?
                    != 0,
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

/// Read the live database's generation token (br-n8qh6) via the parity query
/// closure. Returns `None` when `db_identity` is absent/unseeded or the read
/// fails — the caller then treats every archive artifact by legacy (project, id)
/// semantics rather than attributing it to a generation.
fn read_current_generation<F>(query: &mut F) -> Option<String>
where
    F: FnMut(&str, &[Value]) -> Result<Vec<Row>, String>,
{
    let rows = query(
        "SELECT generation_id FROM db_identity WHERE singleton = 0",
        &[],
    )
    .ok()?;
    rows.first()?
        .get_named::<String>("generation_id")
        .ok()
        .filter(|generation| !generation.is_empty())
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
    mut query: F,
    storage_root: &Path,
) -> Result<ReservationParityReport, String>
where
    F: FnMut(&str, &[Value]) -> Result<Vec<Row>, String>,
{
    // The live database's generation token (br-n8qh6). Archive artifacts stamped
    // with a *different* generation are prior-generation debris, not drift. A
    // missing/unseeded token (`None`) means we can't attribute generations, so we
    // fall back to treating every artifact by its legacy (project, id) semantics.
    let current_generation = read_current_generation(&mut query);
    let db_reservations = query_db_reservations_with(&mut query)?;
    let archive_scan = scan_archive_reservations(storage_root);
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

    // Informational (non-drift) examples are collected separately and appended
    // AFTER the drift examples, so the health line's leading examples always
    // describe the drift being reported. Previously the foreign-generation
    // examples were pushed first and crowded out the drift examples entirely,
    // producing health lines like `fields=[missing_archive=101]
    // examples=[...:foreign_generation_artifact,...]` (GH#244).
    let mut info_examples = Vec::new();

    // Partition scanned artifacts: any stamped with a generation other than the
    // live DB's is prior-generation debris — counted for visibility but excluded
    // from parity comparison (and from `total()`), so a re-created DB writing to
    // the same archive produces zero id collisions and clean parity.
    let mut archive_reservations: Vec<ArchiveReservationState> = Vec::new();
    for reservation in archive_scan.reservations {
        let is_foreign = match (&current_generation, &reservation.generation) {
            (Some(current), Some(generation)) => generation != current,
            _ => false,
        };
        if is_foreign {
            drift.foreign_generation_artifacts += 1;
            if info_examples.len() < 32 {
                info_examples.push(ReservationParityExample {
                    reservation_id: reservation.reservation_id,
                    project_slug: reservation.project_slug.clone(),
                    field: "foreign_generation_artifact".to_string(),
                    db_value: current_generation.clone().unwrap_or_default(),
                    archive_value: reservation.generation.clone().unwrap_or_default(),
                    detail: format!(
                        "reservation_id={} archive artifact from prior DB generation (archive_generation={}, live_generation={}); not drift — quarantine candidate",
                        reservation.reservation_id,
                        reservation.generation.clone().unwrap_or_default(),
                        current_generation.clone().unwrap_or_default(),
                    ),
                });
            }
        } else {
            archive_reservations.push(reservation);
        }
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
            (Some(db), None) => {
                if positive_ts(db.effective_released_ts()) {
                    // A *released* DB row with no current-generation artifact is
                    // expected bookkeeping, not drift (GH#244): it is not a lock
                    // hazard (br-74sxo — "a missing artifact needs no heal"),
                    // the retention prune will delete the row, and after
                    // reconstruct-from-archive the artifact typically still
                    // exists under the prior generation's stamp. The mirror of
                    // the archive-side rule br-5xbua.
                    drift.released_missing_archive += 1;
                    if info_examples.len() < 32 {
                        info_examples.push(ReservationParityExample {
                            reservation_id,
                            project_slug,
                            field: "released_missing_archive".to_string(),
                            db_value: "released".to_string(),
                            archive_value: "missing".to_string(),
                            detail: format!(
                                "reservation_id={reservation_id} released row has no current-generation archive artifact; not drift — awaiting retention prune"
                            ),
                        });
                    }
                } else {
                    // An *active* row missing its artifact is genuine drift: the
                    // pre-commit guard reads the archive, so the holder is
                    // invisible to it (under-blocking). Self-heals via F1
                    // reconcile-on-read on the next reservation read.
                    drift.missing_archive_artifacts += 1;
                    examples.push(ReservationParityExample {
                        reservation_id,
                        project_slug,
                        field: "archive_artifact".to_string(),
                        db_value: "present".to_string(),
                        archive_value: "missing".to_string(),
                        detail: format!(
                            "reservation_id={reservation_id} missing stable id artifact"
                        ),
                    });
                }
            }
            (None, Some(archive)) => {
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
                } else if positive_ts(archive.released_ts) {
                    // A *released* reservation with an archive artifact but no DB
                    // row is the expected steady state once the retention prune
                    // has deleted it from SQLite — the git archive keeps the full
                    // audit record. Count it for visibility, but it is NOT drift
                    // and must not provoke a reconstruct (which would re-hydrate
                    // the dead row). br-5xbua.
                    drift.pruned_released_archived += 1;
                } else {
                    // An *active* reservation present in the archive but missing
                    // from SQLite is genuine drift worth reconstructing.
                    drift.archive_without_db_rows += 1;
                    examples.push(ReservationParityExample {
                        reservation_id,
                        project_slug,
                        field: "db_row".to_string(),
                        db_value: "missing".to_string(),
                        archive_value: "present".to_string(),
                        detail: format!(
                            "reservation_id={reservation_id} active archive artifact has no DB row"
                        ),
                    });
                }
            }
            (None, None) => {}
        }
    }

    // Drift examples first, informational examples after — the health line's
    // leading examples must describe the reported drift (GH#244).
    examples.append(&mut info_examples);

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

    // The reserved path glob is GH#112's core divergence subject. Only compare
    // when the archive carries a value — a legacy artifact that omits
    // `path_pattern` must not be reported as drift (br-xyy95), mirroring the
    // conservative handling of `released_ts`. `json_string` already trims the
    // archive side, so trim the DB side too.
    if let Some(archive_path) = archive.path_pattern.as_deref()
        && db.path_pattern.trim() != archive_path.trim()
    {
        drift.path_pattern_mismatches += 1;
        examples.push(ReservationParityExample {
            reservation_id: db.reservation_id,
            project_slug: db.project_slug.clone(),
            field: "path_pattern".to_string(),
            db_value: db.path_pattern.clone(),
            archive_value: archive_path.to_string(),
            detail: format!(
                "reservation_id={} db_path_pattern={} archive_path_pattern={}",
                db.reservation_id, db.path_pattern, archive_path
            ),
        });
    }

    // The exclusive flag — same conservative rule: skip when the archive omits
    // it (br-xyy95).
    if let Some(archive_exclusive) = archive.exclusive
        && db.exclusive != archive_exclusive
    {
        drift.exclusive_mismatches += 1;
        examples.push(ReservationParityExample {
            reservation_id: db.reservation_id,
            project_slug: db.project_slug.clone(),
            field: "exclusive".to_string(),
            db_value: db.exclusive.to_string(),
            archive_value: archive_exclusive.to_string(),
            detail: format!(
                "reservation_id={} db_exclusive={} archive_exclusive={}",
                db.reservation_id, db.exclusive, archive_exclusive
            ),
        });
    }

    // The archive reader trims this field (json_string) while the DB stores the
    // reason verbatim, so compare both trimmed — otherwise a reason with
    // surrounding/only whitespace produces spurious parity drift on an
    // otherwise-identical pair.
    if db.reason.trim() != archive.thread_provenance.trim() {
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
        scan_reservation_dir(
            &reservation_dir,
            &project_slug,
            &mut reservations,
            &mut parse_errors,
        );
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

/// Scan a single project's `file_reservations/` directory, appending every
/// well-formed `id-<id>.json` artifact to `reservations` and any parse failure
/// to `parse_errors`. Symlink-safe (skips symlinked entries, never derefs). A
/// missing directory is silently treated as "no artifacts".
fn scan_reservation_dir(
    reservation_dir: &Path,
    project_slug: &str,
    reservations: &mut Vec<ArchiveReservationState>,
    parse_errors: &mut Vec<ArchiveParseError>,
) {
    let Ok(entries) = std::fs::read_dir(reservation_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path_is_symlink(&path)
            || !entry.file_type().is_ok_and(|file_type| file_type.is_file())
            || path.extension().is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let Some(parsed) = path.file_name().and_then(|name| name.to_str()).and_then(
            mcp_agent_mail_core::reservation_artifact::parse_reservation_artifact_filename,
        ) else {
            continue;
        };
        match parse_archive_reservation(&path, project_slug, parsed.id, parsed.generation) {
            Ok(reservation) => reservations.push(reservation),
            Err(detail) => parse_errors.push(ArchiveParseError { path, detail }),
        }
    }
}

/// A read-only view of one archive reservation artifact, exposed for the F1
/// reconcile-on-read healing path (`reservations::reconcile_active_reservation_archive`).
///
/// Mirrors the fields the parity check compares. `path_pattern`/`exclusive` are
/// `None` when the artifact omits them (legacy / hand-authored) — absence is not
/// divergence (br-xyy95), matching the conservative comparison used elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveReservationView {
    pub reservation_id: i64,
    pub agent_name: String,
    pub reason: String,
    pub path_pattern: Option<String>,
    pub exclusive: Option<bool>,
    pub released_ts: Option<i64>,
    /// `None` when the artifact omits `expires_ts` (absence is never drift).
    /// Lets reconcile-on-read heal a stale pre-renew artifact whose only
    /// divergence is the expiry.
    pub expires_ts: Option<i64>,
}

impl From<ArchiveReservationState> for ArchiveReservationView {
    fn from(state: ArchiveReservationState) -> Self {
        Self {
            reservation_id: state.reservation_id,
            agent_name: state.agent_name,
            reason: state.thread_provenance,
            path_pattern: state.path_pattern,
            exclusive: state.exclusive,
            released_ts: state.released_ts,
            expires_ts: state.expires_ts,
        }
    }
}

/// Read a single project's archive reservation artifact `id-<id>.json`, if it
/// exists and parses (symlink-safe — a symlinked artifact is never dereferenced).
///
/// This is the F1 reconcile-on-read primitive: the reservation read path looks up
/// only the *active* reservations' artifacts (bounded by the active set, never the
/// project's full reservation history), so detecting a missing/stale artifact and
/// healing it on next access stays cheap even on a long-lived mailbox. A missing
/// or malformed artifact returns `None` (it must never block a reservation call);
/// the caller treats `None` as "needs healing".
#[must_use]
pub fn read_project_archive_reservation(
    storage_root: &Path,
    project_slug: &str,
    reservation_id: i64,
) -> Option<ArchiveReservationView> {
    if reservation_id <= 0 {
        return None;
    }
    // Locate the stable artifact by id, matching either the generation-stamped
    // (`id-<id>-g<generation>.json`) or legacy (`id-<id>.json`) name (br-n8qh6),
    // so reconcile-on-read finds a stamped artifact and never spuriously re-heals.
    let reservation_dir = storage_root
        .join("projects")
        .join(project_slug)
        .join("file_reservations");
    let path = mcp_agent_mail_core::reservation_artifact::find_reservation_artifact(
        &reservation_dir,
        reservation_id,
    )?;
    if path_is_symlink(&path) {
        return None;
    }
    // Attribute the generation from the located filename (stamped or legacy).
    let generation = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(mcp_agent_mail_core::reservation_artifact::parse_reservation_artifact_filename)
        .and_then(|parsed| parsed.generation);
    parse_archive_reservation(&path, project_slug, reservation_id, generation)
        .ok()
        .map(ArchiveReservationView::from)
}

fn parse_archive_reservation(
    path: &Path,
    project_slug: &str,
    reservation_id: i64,
    generation: Option<String>,
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
    // Prefer the generation parsed from the filename (authoritative for naming);
    // fall back to the artifact's own `db_generation` field so a legacy-named
    // file that nonetheless carries a token is still attributed (br-n8qh6).
    let generation = generation.or_else(|| json_string(&json, "db_generation"));
    let agent_name =
        json_string(&json, "agent").ok_or_else(|| "agent is missing or blank".to_string())?;
    let thread_provenance = json_string(&json, "thread_id")
        .or_else(|| json_string(&json, "thread"))
        .or_else(|| json_string(&json, "reason"))
        .unwrap_or_default();
    // The canonical archive key is `path_pattern`; older artifacts may have used
    // `path`. Absent entirely -> None (not drift). br-xyy95.
    let path_pattern = json_string(&json, "path_pattern").or_else(|| json_string(&json, "path"));
    let exclusive = json.get("exclusive").and_then(serde_json::Value::as_bool);
    let released_ts = parse_json_micros(&json, "released_ts");
    let expires_ts = parse_json_micros(&json, "expires_ts");

    Ok(ArchiveReservationState {
        reservation_id,
        project_slug: project_slug.to_string(),
        generation,
        agent_name,
        thread_provenance,
        path_pattern,
        exclusive,
        released_ts,
        expires_ts,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn db_state(path: &str, exclusive: bool) -> DbReservationState {
        DbReservationState {
            reservation_id: 1,
            project_slug: "proj".to_string(),
            agent_name: "Agent".to_string(),
            reason: "r".to_string(),
            path_pattern: path.to_string(),
            exclusive,
            reservation_released_ts: None,
            ledger_released_ts: None,
        }
    }

    fn archive_state(path: Option<&str>, exclusive: Option<bool>) -> ArchiveReservationState {
        ArchiveReservationState {
            reservation_id: 1,
            project_slug: "proj".to_string(),
            generation: None,
            agent_name: "Agent".to_string(),
            thread_provenance: "r".to_string(),
            path_pattern: path.map(str::to_string),
            exclusive,
            released_ts: None,
            expires_ts: None,
        }
    }

    /// Write a reservation archive artifact at
    /// `<storage_root>/projects/<slug>/file_reservations/<name>`, where the name
    /// is generation-stamped iff `generation` is `Some`. Content mirrors what the
    /// tools layer emits so parity comparison is faithful.
    fn write_reservation_artifact(
        storage_root: &Path,
        slug: &str,
        id: i64,
        generation: Option<&str>,
        agent: &str,
        path_pattern: &str,
        exclusive: bool,
    ) {
        let dir = storage_root
            .join("projects")
            .join(slug)
            .join("file_reservations");
        std::fs::create_dir_all(&dir).expect("create reservations dir");
        let mut value = serde_json::json!({
            "id": id,
            "project": format!("/{slug}"),
            "agent": agent,
            "path_pattern": path_pattern,
            "exclusive": exclusive,
            "reason": "br-n8qh6",
            "created_ts": 100_i64,
            "expires_ts": 9_999_999_999_999_999_i64,
        });
        if let Some(generation) = generation {
            value["db_generation"] = serde_json::Value::String(generation.to_string());
        }
        let name = mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
            generation, id,
        );
        std::fs::write(dir.join(name), serde_json::to_vec_pretty(&value).unwrap())
            .expect("write reservation artifact");
    }

    /// Build an in-memory DB with the base schema, a single project + agent, one
    /// active reservation row (`id = 1`, project `proj-a`), and generation
    /// `generation` seeded into `db_identity`.
    fn seed_single_reservation_db(generation: &str) -> mcp_agent_mail_db::DbConn {
        let conn = mcp_agent_mail_db::DbConn::open_memory().expect("open memory db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("create schema");
        conn.execute_raw(&format!(
            "INSERT INTO db_identity (singleton, generation_id) VALUES (0, '{generation}')"
        ))
        .expect("seed generation");
        conn.execute_raw(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'proj-a', '/proj-a', 0)",
        )
        .expect("insert project");
        conn.execute_raw(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts) \
             VALUES (1, 1, 'BlueLake', 'codex', 'gpt', '', 0, 0)",
        )
        .expect("insert agent");
        conn.execute_raw(
            "INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
             VALUES (1, 1, 1, 'src/**', 1, 'br-n8qh6', 100, 9999999999999999, NULL)",
        )
        .expect("insert reservation");
        conn
    }

    #[test]
    fn two_generations_produce_zero_collisions_and_clean_parity() {
        // br-n8qh6 acceptance: a re-created DB (generation G2) writing reservations
        // to the same archive as a prior generation (G1) — including the exact
        // cross-project global-id reuse that produced 607 archive_id_collisions —
        // must yield zero collisions and clean parity.
        let storage = tempfile::tempdir().expect("tempdir");
        let conn = seed_single_reservation_db("aaaa2222");

        // Current-generation artifact for the live row (proj-a, id 1).
        write_reservation_artifact(
            storage.path(),
            "proj-a",
            1,
            Some("aaaa2222"),
            "BlueLake",
            "src/**",
            true,
        );
        // Prior-generation debris: SAME global id 1 under a DIFFERENT project,
        // written by generation G1. Pre-fix this was an archive_id_collision.
        write_reservation_artifact(
            storage.path(),
            "proj-b",
            1,
            Some("bbbb1111"),
            "OldHolder",
            "legacy/**",
            true,
        );
        // Prior-generation debris under the SAME project as the live row — the
        // generation stamp keeps it at a distinct filename, so it never overwrote
        // the current artifact.
        write_reservation_artifact(
            storage.path(),
            "proj-a",
            1,
            Some("bbbb1111"),
            "OldHolder",
            "legacy/**",
            true,
        );

        let report = check_reservation_parity_with_db_conn(&conn, storage.path()).expect("parity");
        assert_eq!(
            report.drift.archive_id_collisions, 0,
            "no cross-generation collisions: {:?}",
            report.examples
        );
        assert_eq!(
            report.drift.total(),
            0,
            "clean parity: {:?}",
            report.examples
        );
        assert_eq!(
            report.drift.foreign_generation_artifacts, 2,
            "both prior-generation artifacts are recognized as foreign debris"
        );
        assert!(report.ok);
    }

    /// GH#244: a *released* DB row with no current-generation archive artifact
    /// is bookkeeping awaiting the retention prune, not drift. Pre-fix this
    /// counted as `missing_archive` and made `am doctor health` return rc=1
    /// permanently on a healthy mailbox, with no fixer able to clear it.
    #[test]
    fn released_row_missing_archive_is_informational_not_drift() {
        let storage = tempfile::tempdir().expect("tempdir");
        let conn = seed_single_reservation_db("aaaa2222");
        conn.execute_raw("UPDATE file_reservations SET released_ts = 200 WHERE id = 1")
            .expect("release reservation");
        // No archive artifact written at all.

        let report = check_reservation_parity_with_db_conn(&conn, storage.path()).expect("parity");
        assert_eq!(
            report.drift.missing_archive_artifacts, 0,
            "released row must not count as missing_archive drift: {:?}",
            report.examples
        );
        assert_eq!(report.drift.released_missing_archive, 1);
        assert_eq!(report.drift.total(), 0);
        assert!(report.ok);
        let line = report.health_line();
        assert!(line.starts_with("reservation_parity: ok"), "{line}");
        assert!(line.contains("released_missing_archive=1"), "{line}");
    }

    /// GH#244 counter-guard: an *active* row missing its artifact stays drift —
    /// the pre-commit guard reads the archive, so the holder would be invisible
    /// to it (under-blocking). This direction self-heals via reconcile-on-read.
    #[test]
    fn active_row_missing_archive_is_still_drift() {
        let storage = tempfile::tempdir().expect("tempdir");
        let conn = seed_single_reservation_db("aaaa2222");
        // Active row (released_ts NULL), no artifact.

        let report = check_reservation_parity_with_db_conn(&conn, storage.path()).expect("parity");
        assert_eq!(report.drift.missing_archive_artifacts, 1);
        assert_eq!(report.drift.released_missing_archive, 0);
        assert_eq!(report.drift.total(), 1);
        assert!(!report.ok);
    }

    /// GH#244's exact field shape: reconstruct-from-archive imported released
    /// reservations into a freshly-minted DB generation, so their artifacts
    /// exist only under the *prior* generation stamp. Parity must report ok
    /// (informational released_missing_archive + foreign_generation_artifacts),
    /// not a permanent `missing_archive` drift.
    #[test]
    fn reconstructed_released_rows_with_foreign_generation_artifacts_report_ok() {
        let storage = tempfile::tempdir().expect("tempdir");
        let conn = seed_single_reservation_db("aaaa2222");
        conn.execute_raw("UPDATE file_reservations SET released_ts = 200 WHERE id = 1")
            .expect("release reservation");
        // The row's artifact exists, but only stamped with the prior DB
        // generation (what reconstruct imported it from).
        write_reservation_artifact(
            storage.path(),
            "proj-a",
            1,
            Some("bbbb1111"),
            "BlueLake",
            "src/**",
            true,
        );

        let report = check_reservation_parity_with_db_conn(&conn, storage.path()).expect("parity");
        assert_eq!(report.drift.total(), 0, "{:?}", report.examples);
        assert!(report.ok);
        assert_eq!(report.drift.released_missing_archive, 1);
        assert_eq!(report.drift.foreign_generation_artifacts, 1);
        assert_eq!(report.drift.missing_archive_artifacts, 0);
    }

    /// GH#244 diagnostics: drift examples must precede informational examples,
    /// so the health line's `examples=[...]` (first 3) describes the drift
    /// being reported instead of foreign-generation debris.
    #[test]
    fn drift_examples_precede_informational_examples() {
        let storage = tempfile::tempdir().expect("tempdir");
        let conn = seed_single_reservation_db("aaaa2222");
        // Informational: prior-generation debris under an unrelated project/id.
        write_reservation_artifact(
            storage.path(),
            "proj-b",
            7,
            Some("bbbb1111"),
            "OldHolder",
            "legacy/**",
            true,
        );
        // Drift: the active row (proj-a, id 1) has no artifact.

        let report = check_reservation_parity_with_db_conn(&conn, storage.path()).expect("parity");
        assert!(!report.ok);
        assert_eq!(report.drift.missing_archive_artifacts, 1);
        assert_eq!(report.drift.foreign_generation_artifacts, 1);
        assert_eq!(
            report.examples.first().map(|e| e.field.as_str()),
            Some("archive_artifact"),
            "drift example must come first: {:?}",
            report.examples
        );
        assert_eq!(
            report.examples.last().map(|e| e.field.as_str()),
            Some("foreign_generation_artifact"),
            "informational example must come last: {:?}",
            report.examples
        );
        let line = report.health_line();
        assert!(line.contains("examples=[proj-a:1:archive_artifact"), "{line}");
    }

    #[test]
    fn legacy_cross_project_id_reuse_is_still_a_collision() {
        // Regression guard: an UN-stamped (legacy) artifact whose global id is
        // reused across projects is still detected as a collision (GH#167). The
        // generation-aware path must only exempt *stamped* foreign artifacts.
        let storage = tempfile::tempdir().expect("tempdir");
        let conn = seed_single_reservation_db("aaaa2222");

        write_reservation_artifact(
            storage.path(),
            "proj-a",
            1,
            Some("aaaa2222"),
            "BlueLake",
            "src/**",
            true,
        );
        // Legacy (no generation) artifact under a different project with the same id.
        write_reservation_artifact(
            storage.path(),
            "proj-b",
            1,
            None,
            "OldHolder",
            "legacy/**",
            true,
        );

        let report = check_reservation_parity_with_db_conn(&conn, storage.path()).expect("parity");
        assert_eq!(report.drift.archive_id_collisions, 1);
        assert_eq!(report.drift.foreign_generation_artifacts, 0);
    }

    fn run_compare(
        db: &DbReservationState,
        archive: &ArchiveReservationState,
    ) -> (ReservationParityDriftSummary, Vec<ReservationParityExample>) {
        let mut drift = ReservationParityDriftSummary::default();
        let mut examples = Vec::new();
        compare_reservation_pair(db, archive, &mut drift, &mut examples);
        (drift, examples)
    }

    #[test]
    fn path_pattern_divergence_is_drift() {
        // GH#112 / br-xyy95: a DB row and archive artifact that agree on agent,
        // released_ts, and reason but reserve DIFFERENT paths must be drift —
        // previously this passed parity clean (the reserved path was ignored).
        let (drift, examples) = run_compare(
            &db_state("src/a.rs", true),
            &archive_state(Some("src/b.rs"), Some(true)),
        );
        assert_eq!(drift.path_pattern_mismatches, 1);
        assert_eq!(drift.exclusive_mismatches, 0);
        assert_eq!(drift.total(), 1);
        assert!(examples.iter().any(|e| e.field == "path_pattern"
            && e.detail.contains("db_path_pattern=src/a.rs")
            && e.detail.contains("archive_path_pattern=src/b.rs")));
    }

    #[test]
    fn exclusive_divergence_is_drift() {
        let (drift, examples) = run_compare(
            &db_state("src/a.rs", true),
            &archive_state(Some("src/a.rs"), Some(false)),
        );
        assert_eq!(drift.exclusive_mismatches, 1);
        assert_eq!(drift.path_pattern_mismatches, 0);
        assert_eq!(drift.total(), 1);
        assert!(examples.iter().any(|e| e.field == "exclusive"));
    }

    #[test]
    fn matching_path_and_exclusive_is_clean_trimmed() {
        // json_string trims the archive side; the comparison trims the DB side
        // too, so surrounding whitespace must not manufacture drift.
        let (drift, _) = run_compare(
            &db_state("src/a.rs", true),
            &archive_state(Some("  src/a.rs  "), Some(true)),
        );
        assert_eq!(drift.total(), 0);
    }

    #[test]
    fn archive_omitting_path_or_exclusive_is_not_drift() {
        // The false-positive guard (br-xyy95, the A2 lesson): a legacy/hand-
        // authored artifact that omits path_pattern/exclusive must NOT be
        // reported as drift — absence is not divergence.
        let (drift, _) = run_compare(&db_state("src/a.rs", true), &archive_state(None, None));
        assert_eq!(
            drift.total(),
            0,
            "absent archive path_pattern/exclusive must not manufacture drift"
        );
    }

    #[test]
    fn health_line_surfaces_path_and_exclusive_fields() {
        let report = ReservationParityReport {
            schema_version: RESERVATION_PARITY_SCHEMA_VERSION,
            ok: false,
            db_reservations: 1,
            archive_reservations: 1,
            drift: ReservationParityDriftSummary {
                path_pattern_mismatches: 1,
                exclusive_mismatches: 1,
                ..ReservationParityDriftSummary::default()
            },
            examples: Vec::new(),
        };
        let line = report.health_line();
        assert!(line.contains("path_pattern=1"), "{line}");
        assert!(line.contains("exclusive=1"), "{line}");
        assert!(line.contains("total=2"), "{line}");
    }
}
