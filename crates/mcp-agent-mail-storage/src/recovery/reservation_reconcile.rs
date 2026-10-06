//! Bounded repair of terminal reservation releases from the live mailbox.
//!
//! A lost release artifact must not require another client to visit the project
//! before its pre-commit guard stops honoring the old holder. This pass only
//! publishes terminal releases: it never recreates an ACTIVE lease, changes a
//! DB row, or replaces another generation's reservation history.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use asupersync::{Cx, Outcome};
use fastmcp_core::block_on;
use git2::{ObjectType, Oid, Repository};
use mcp_agent_mail_core::{Config, reservation_artifact::reservation_artifact_filename};
use mcp_agent_mail_db::{DbError, DbPool, FileReservationRow, corruption_circuit_breaker};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest as _, Sha1};

use super::agent_reconcile::{Artifact, committed_artifact, head_tree, read_artifact};
use crate::{ProjectArchive, StorageError};

const IDS_PER_BATCH: i64 = 32;
const MAX_REPAIRS_PER_BATCH: usize = 4;
const PRIORITY_IDS_PER_BATCH: i64 = 16;
const PRIORITY_REPAIRS_PER_BATCH: usize = 2;
const MAX_SOURCE_BYTES: i64 = 32 * 1024;
const MAX_ARTIFACT_BYTES: usize = 128 * 1024;

/// Default-on for live file-backed mailboxes; invalid overrides fail disabled.
#[must_use]
pub fn enabled(config: &Config) -> bool {
    mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url).is_some()
        && mcp_agent_mail_core::config::process_env_value(
            "AM_RESERVATION_ARCHIVE_RECONCILE_ENABLED",
        )
        .is_none_or(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

/// Finite rounds revisit old releases even while new reservations arrive.
#[derive(Debug, Default)]
pub struct ReservationReconcileCursor {
    source_identity: String,
    priority: ScanPosition,
    history: ScanPosition,
}

#[derive(Debug, Default)]
struct ScanPosition {
    after: i64,
    ceiling: Option<i64>,
}

/// Counts cover this pass only; deferred evidence remains visible to doctor.
#[derive(Debug, Default, Serialize)]
pub struct ReservationReconcileReport {
    pub scanned: usize,
    pub unchanged: usize,
    /// Publication attempts, including writes followed by a failed Git commit.
    pub attempted: usize,
    pub repaired: usize,
    pub deferred: usize,
    pub interrupted: bool,
    pub budget_exhausted: bool,
}

const PROGRESS_FILE: &str = "reservation-reconcile.json";
const MAX_PROGRESS_BYTES: u64 = 4096;
/// How far in the future a recorded progress time may lie and still count:
/// beyond it the clock moved backwards, and a future time must not read as
/// recent progress (or survive the max-merge) indefinitely.
pub const PROGRESS_CLOCK_SKEW_US: i64 = 60 * 1_000_000;

/// What `am doctor health` needs to tell a converging reconciler from a stuck
/// or absent one: when the last pass completed and when one last republished
/// a release. The mailbox owner records it after every pass (br-kp1in.19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationReconcileProgress {
    pub pid: u32,
    /// Completion of the last pass, microseconds since the epoch.
    pub pass_us: i64,
    /// The last pass, by any owner of this mailbox, that republished at least
    /// one release.
    pub last_repair_us: Option<i64>,
}

#[must_use]
pub fn progress_path(storage_root: &Path) -> PathBuf {
    storage_root.join("doctor").join(PROGRESS_FILE)
}

/// Replace the progress record atomically. When several processes own
/// reconcilers for one mailbox, the latest repair any of them recorded is
/// kept, so one idle owner cannot hide another's progress. A `doctor` path
/// that is not a plain directory is refused, never followed.
pub fn record_progress(
    storage_root: &Path,
    progress: &ReservationReconcileProgress,
) -> std::io::Result<()> {
    let dir = storage_root.join("doctor");
    match fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(std::io::Error::other(format!(
                "{} is not a plain directory",
                dir.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&dir)?,
        Err(error) => return Err(error),
    }
    let mut merged = progress.clone();
    if let Some(previous) = read_progress(storage_root) {
        let previous_repair = previous
            .last_repair_us
            .filter(|ts_us| *ts_us <= progress.pass_us.saturating_add(PROGRESS_CLOCK_SKEW_US));
        merged.last_repair_us = merged.last_repair_us.max(previous_repair);
    }
    let body = serde_json::to_vec(&merged).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!(".{PROGRESS_FILE}.{}.tmp", std::process::id()));
    fs::write(&tmp, body)?;
    fs::rename(&tmp, progress_path(storage_root))
}

/// The last recorded progress; `None` when absent, oversized, or unreadable.
#[must_use]
pub fn read_progress(storage_root: &Path) -> Option<ReservationReconcileProgress> {
    let path = progress_path(storage_root);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_PROGRESS_BYTES {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

#[derive(Debug, PartialEq)]
struct ReleaseSource {
    id: i64,
    project_id: i64,
    agent_id: i64,
    project_slug: String,
    generation: Option<String>,
    artifact: Value,
}

fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::InvalidPath(message.into())
}

fn source_error(error: impl std::fmt::Display) -> String {
    let error = DbError::Sqlite(error.to_string());
    corruption_circuit_breaker().observe_error(&error);
    format!("reservation reconciliation source query failed: {error}")
}

fn outcome<T, E: std::fmt::Display>(value: Outcome<T, E>) -> Result<T, String> {
    match value {
        Outcome::Ok(value) => Ok(value),
        Outcome::Err(error) => Err(source_error(error)),
        Outcome::Cancelled(_) => Err("reservation reconciliation cancelled".into()),
        Outcome::Panicked(_) => Err("reservation reconciliation source read panicked".into()),
    }
}

fn timestamp(value: &Value) -> Option<i64> {
    match value {
        Value::Number(value) => value.as_i64(),
        Value::String(value) => value
            .trim()
            .parse()
            .ok()
            .or_else(|| mcp_agent_mail_core::iso_to_micros(value.trim())),
        _ => None,
    }
}

fn validate_source(pool: &DbPool, config: &Config) -> Result<(), String> {
    let selected =
        mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url)
            .ok_or("reservation reconciliation requires a file-backed source")?;
    if fs::canonicalize(selected).map_err(|error| error.to_string())?
        != fs::canonicalize(pool.sqlite_path()).map_err(|error| error.to_string())?
        || pool.search_identity_path() != pool.sqlite_path()
    {
        return Err("reservation reconciliation requires the configured live database".into());
    }
    if fs::canonicalize(pool.storage_root()).map_err(|error| error.to_string())?
        != fs::canonicalize(&config.storage_root).map_err(|error| error.to_string())?
    {
        return Err("reservation reconciliation pool and archive roots differ".into());
    }
    Ok(())
}

fn select_ids(
    cx: &Cx,
    pool: &DbPool,
    cursor: &mut ScanPosition,
    limit: i64,
    unexpired_only: bool,
) -> Result<Vec<i64>, String> {
    let conn = outcome(block_on(pool.acquire(cx)))?;
    let mode = conn
        .query_sync("PRAGMA query_only", &[])
        .map_err(source_error)?;
    if mode.first().and_then(|row| row.get_as::<i64>(0).ok()) != Some(0) {
        return Err("query-only snapshots cannot authorize reservation archive repair".into());
    }
    if cursor.ceiling.is_none() {
        let rows = conn
            .query_sync("SELECT COALESCE(MAX(id), 0) FROM file_reservations", &[])
            .map_err(source_error)?;
        cursor.ceiling = Some(
            rows.first()
                .ok_or("reservation ID aggregate is missing")?
                .get_as::<i64>(0)
                .map_err(source_error)?,
        );
        cursor.after = 0;
    }
    // History walks a bounded primary-key window, including active rows as
    // unchanged, so a sparse release history cannot force a full table scan.
    // Priority filters for still-unexpired terminal rows to reach guard-blocking
    // releases promptly; its LIMIT bounds selected IDs, not SQL rows examined.
    let rows = conn
        .query_sync(
            "SELECT fr.id FROM file_reservations fr WHERE fr.id > ? AND fr.id <= ? \
         AND (? = 0 OR (fr.expires_ts > ? AND (fr.released_ts IS NOT NULL OR EXISTS \
             (SELECT 1 FROM file_reservation_releases rr WHERE rr.reservation_id = fr.id)))) \
         ORDER BY fr.id LIMIT ?",
            &[
                cursor.after.into(),
                cursor.ceiling.unwrap_or(0).into(),
                i64::from(unexpired_only).into(),
                mcp_agent_mail_db::now_micros().into(),
                limit.into(),
            ],
        )
        .map_err(source_error)?;
    if rows.is_empty() {
        cursor.ceiling = None;
    }
    rows.iter()
        .map(|row| row.get_named("id").map_err(source_error))
        .collect()
}

fn read_source(cx: &Cx, pool: &DbPool, id: i64) -> Result<Option<ReleaseSource>, String> {
    let conn = outcome(block_on(pool.acquire(cx)))?;
    // Identity, effective release and generation belong to one SQL snapshot.
    // Bound raw strings before materializing them; never seed a generation.
    let rows = conn.query_sync(
        "SELECT fr.id, fr.project_id, fr.agent_id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                fr.created_ts, fr.expires_ts, CAST(fr.released_ts AS TEXT) AS hot_release, \
                rr.released_ts AS ledger_release, p.slug, p.human_key, a.name, d.generation_id \
         FROM file_reservations fr JOIN projects p ON p.id = fr.project_id \
         JOIN agents a ON a.id = fr.agent_id AND a.project_id = fr.project_id \
         LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id \
         LEFT JOIN db_identity d ON d.singleton = 0 \
         WHERE fr.id = ? AND length(CAST(fr.path_pattern AS BLOB)) + length(CAST(fr.reason AS BLOB)) + \
               length(CAST(p.slug AS BLOB)) + length(CAST(p.human_key AS BLOB)) + length(CAST(a.name AS BLOB)) + \
               length(CAST(fr.created_ts AS BLOB)) + length(CAST(fr.expires_ts AS BLOB)) + \
               length(CAST(fr.\"exclusive\" AS BLOB)) + \
               COALESCE(length(CAST(fr.released_ts AS BLOB)), 0) + \
               COALESCE(length(CAST(rr.released_ts AS BLOB)), 0) + \
               COALESCE(length(CAST(d.generation_id AS BLOB)), 0) <= ?",
        &[id.into(), MAX_SOURCE_BYTES.into()],
    ).map_err(source_error)?;
    let [row] = rows.as_slice() else {
        return Err(
            "reservation source missing, oversized, or lacking project/holder authority".into(),
        );
    };
    let integer = |key| row.get_named::<i64>(key).map_err(source_error);
    let text = |key| row.get_named::<String>(key).map_err(source_error);
    let hot = row
        .get_named::<Option<String>>("hot_release")
        .map_err(source_error)?;
    let ledger = row
        .get_named::<Option<i64>>("ledger_release")
        .map_err(source_error)?;
    let release = ledger.or_else(|| hot.and_then(|value| timestamp(&Value::String(value))));
    let Some(release) = release.filter(|value| *value > 0) else {
        return Ok(None);
    };
    let generation = row
        .get_named::<Option<String>>("generation_id")
        .map_err(source_error)?;
    if generation.as_ref().is_some_and(|value| {
        value.is_empty() || value.len() > 128 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err("reservation database generation is invalid".into());
    }
    let slug = text("slug")?;
    let name = text("name")?;
    crate::validate_archive_component("project slug", &slug).map_err(|error| error.to_string())?;
    crate::validate_archive_component("agent name", &name).map_err(|error| error.to_string())?;
    let key = text("human_key")?;
    let pattern = text("path_pattern")?;
    let created = integer("created_ts")?;
    let expires = integer("expires_ts")?;
    let exclusive = integer("exclusive")?;
    if id <= 0
        || integer("id")? != id
        || !Path::new(&key).is_absolute()
        || pattern.trim().is_empty()
        || !matches!(exclusive, 0 | 1)
        || [created, expires, release]
            .into_iter()
            .any(|value| chrono::DateTime::from_timestamp_micros(value).is_none())
    {
        return Err("reservation source identity or timestamps are invalid".into());
    }
    let mut artifact = json!({
        "id": id, "project": key, "agent": name, "path_pattern": pattern.trim(),
        "exclusive": exclusive != 0, "reason": text("reason")?,
        "created_ts": mcp_agent_mail_db::micros_to_iso(created),
        "expires_ts": mcp_agent_mail_db::micros_to_iso(expires),
        "released_ts": mcp_agent_mail_db::micros_to_iso(release),
    });
    if let Some(generation) = &generation {
        artifact["db_generation"] = json!(generation);
    }
    Ok(Some(ReleaseSource {
        id,
        project_id: integer("project_id")?,
        agent_id: integer("agent_id")?,
        project_slug: slug,
        generation,
        artifact,
    }))
}

fn validate_artifact(source: &ReleaseSource, value: &Value) -> crate::Result<()> {
    let generation = match value.get("db_generation") {
        None => None,
        Some(Value::String(generation))
            if !generation.is_empty()
                && generation.len() <= 128
                && generation.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            Some(generation.as_str())
        }
        Some(_) => {
            return Err(invalid(
                "reservation generation metadata is malformed; preserved",
            ));
        }
    };
    if generation.is_some_and(|generation| Some(generation) != source.generation.as_deref()) {
        return Err(invalid(
            "reservation artifact belongs to a foreign generation; preserved",
        ));
    }
    for field in ["id", "project", "agent", "path_pattern", "exclusive"] {
        if value.get(field) != source.artifact.get(field) {
            return Err(invalid(format!(
                "reservation {field} conflicts with live identity; preserved"
            )));
        }
    }
    if timestamp(&value["created_ts"]) != timestamp(&source.artifact["created_ts"]) {
        return Err(invalid(
            "reservation creation identity conflicts; preserved",
        ));
    }
    let release = &value["released_ts"];
    let absent = release.is_null()
        || release.as_f64() == Some(0.0)
        || release.as_str().is_some_and(|text| {
            matches!(
                text.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "null" | "none"
            )
        });
    if !absent && timestamp(release).is_none() {
        return Err(invalid(
            "reservation release metadata is malformed; preserved",
        ));
    }
    // A different surviving terminal timestamp is evidence for manual review.
    // The automatic pass only fills a missing release, never moves one later.
    if !absent && timestamp(release) != timestamp(&source.artifact["released_ts"]) {
        return Err(invalid("reservation terminal release conflicts; preserved"));
    }
    Ok(())
}

struct Target {
    path: PathBuf,
    relative: String,
    working: Option<Artifact>,
    bytes: Vec<u8>,
    committed_matches: bool,
}

fn prepare_target(
    archive: &ProjectArchive,
    repo: &Repository,
    tree: Option<&git2::Tree<'_>>,
    source: &ReleaseSource,
    path: PathBuf,
    optional: bool,
) -> crate::Result<Option<Target>> {
    let relative = crate::rel_path_cached(&archive.canonical_repo_root, &path)?;
    let working = read_artifact(&path)?;
    let committed = committed_artifact(repo, tree, &relative)?;
    if optional && working.is_none() && committed.is_none() {
        return Ok(None);
    }
    let belongs = |artifact: &Artifact| {
        artifact.value["id"] == source.artifact["id"]
            && artifact
                .value
                .get("db_generation")
                .and_then(Value::as_str)
                .is_none_or(|generation| Some(generation) == source.generation.as_deref())
    };
    if optional
        && working
            .as_ref()
            .or(committed.as_ref())
            .is_some_and(|artifact| !belongs(artifact))
    {
        return Ok(None);
    }
    // The digest is reused by later leases. Its working copy, when present,
    // decides current ownership; a previous lease's committed digest remains
    // history and is not merged into this lease's release record.
    let committed = committed.filter(|artifact| !optional || belongs(artifact));
    for artifact in [working.as_ref(), committed.as_ref()].into_iter().flatten() {
        validate_artifact(source, &artifact.value)?;
    }
    let mut value = committed
        .as_ref()
        .map_or_else(|| json!({}), |artifact| artifact.value.clone());
    let object = value.as_object_mut().expect("validated metadata object");
    if let Some(working) = &working {
        object.extend(
            working
                .value
                .as_object()
                .expect("validated metadata object")
                .clone(),
        );
    }
    // Preserve unknown fields while restoring the complete DB-authored record.
    object.extend(source.artifact.as_object().expect("source object").clone());
    let bytes = serde_json::to_vec_pretty(&value)?;
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(invalid("reservation artifact exceeds byte bound"));
    }
    let committed_matches = committed
        .as_ref()
        .is_some_and(|artifact| artifact.bytes == bytes);
    Ok(Some(Target {
        path,
        relative,
        working,
        bytes,
        committed_matches,
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReleaseTargets {
    StableOnly,
    WithOwnedAliases,
}

fn publish_release(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    original: &ReleaseSource,
    targets: ReleaseTargets,
    attempted: &mut bool,
) -> Result<bool, String> {
    let _mutation = crate::ArchiveMutationGuard::begin_at(&config.storage_root);
    let archive =
        crate::ensure_archive(config, &original.project_slug).map_err(|error| error.to_string())?;
    crate::with_project_lock(&archive, || {
        let source = read_source(cx, pool, original.id).map_err(invalid)?;
        if source.as_ref() != Some(original) {
            return Err(invalid(
                "reservation source/generation changed before publication; deferred",
            ));
        }
        let root = crate::archive_project_root_checked(&archive)?;
        let repo_root = crate::archive_repo_root_checked(&archive)?;
        let repo = Repository::open(repo_root)?;
        let tree = head_tree(&repo)?;
        let project_path = root.join("project.json");
        let project_rel = crate::rel_path_cached(&archive.canonical_repo_root, &project_path)?;
        for metadata in [
            read_artifact(&project_path)?,
            committed_artifact(&repo, tree.as_ref(), &project_rel)?,
        ]
        .into_iter()
        .flatten()
        {
            if metadata.value["slug"] != original.project_slug
                || metadata.value["human_key"] != original.artifact["project"]
            {
                return Err(invalid(
                    "archived project identity conflicts with reservation source; preserved",
                ));
            }
        }
        let dir = root.join("file_reservations");
        let stable = dir.join(reservation_artifact_filename(
            original.generation.as_deref(),
            original.id,
        ));
        let mut paths = vec![(stable, false)];
        if targets == ReleaseTargets::WithOwnedAliases {
            let digest = hex::encode(Sha1::digest(
                original.artifact["path_pattern"]
                    .as_str()
                    .expect("source path")
                    .as_bytes(),
            ));
            paths.push((dir.join(format!("{digest}.json")), true));
            // Only the history reconciler visits reusable aliases, checking
            // their current owner. A closeout replay never writes these paths.
            if original.generation.is_some() {
                paths.push((dir.join(format!("id-{}.json", original.id)), true));
            }
        }
        let mut targets = Vec::new();
        for (path, optional) in paths {
            if let Some(target) =
                prepare_target(&archive, &repo, tree.as_ref(), original, path, optional)?
            {
                targets.push(target);
            }
        }
        if targets.iter().all(|target| {
            target.committed_matches
                && target
                    .working
                    .as_ref()
                    .is_some_and(|working| working.bytes == target.bytes)
        }) {
            return Ok(false);
        }
        // Recheck both live source and exact working bytes immediately before
        // replacing anything. The outer source lease prevents DB promotion;
        // terminal release is monotone within that generation.
        if read_source(cx, pool, original.id)
            .map_err(invalid)?
            .as_ref()
            != Some(original)
        {
            return Err(invalid(
                "reservation source changed during artifact verification; deferred",
            ));
        }
        for target in &targets {
            if read_artifact(&target.path)?
                .as_ref()
                .map(|artifact| &artifact.bytes)
                != target.working.as_ref().map(|artifact| &artifact.bytes)
            {
                return Err(invalid(
                    "reservation artifact changed during verification; preserved",
                ));
            }
        }
        *attempted = true;
        for target in &targets {
            if target
                .working
                .as_ref()
                .is_none_or(|working| working.bytes != target.bytes)
            {
                crate::ensure_parent_dir(&target.path)?;
                crate::atomic_write_bytes(&target.path, &target.bytes, true)?;
            }
        }
        let paths = targets
            .iter()
            .map(|target| target.relative.as_str())
            .collect::<Vec<_>>();
        crate::commit_paths_with_retry(
            repo_root,
            config,
            &format!("repair: reservation {} release", original.id),
            &paths,
        )?;
        let verified = Repository::open(repo_root)?;
        let tree = head_tree(&verified)?
            .ok_or_else(|| invalid("reservation release commit lacks HEAD"))?;
        for target in targets {
            let entry = tree.get_path(Path::new(&target.relative))?;
            if entry.kind() != Some(ObjectType::Blob)
                || !matches!(entry.filemode(), 0o100644 | 0o100755)
                || entry.id() != Oid::hash_object(ObjectType::Blob, &target.bytes)?
                || verified.find_blob(entry.id())?.content() != target.bytes
            {
                return Err(invalid(
                    "reservation release Git verification failed; files retained",
                ));
            }
        }
        Ok(true)
    })
    .map_err(|error| error.to_string())
}

/// Reconcile one released row's generation-stamped artifact from live state.
///
/// `expected_generation` must be captured from the same mailbox before the
/// release mutation. The returned row supplies immutable identity, not an
/// archive payload: the effective release and current metadata are re-read.
/// Missing generation authority, a replaced identity, and an active live row
/// cannot authorize publication. No digest or unstamped alias is read or written.
///
/// This is synchronous, bounded to one reservation, and never queues captured
/// bytes. On failure the DB release remains authoritative for the background
/// history reconciler. `Ok(false)` means the verified artifact already matches;
/// `Ok(true)` means its exact bytes were verified in Git after publication.
/// Recovery promotion or admission contention defers repair before DB/archive
/// access. In particular, a replay caller's existing writer lease must never
/// wait for a promotion that is itself trying to drain that caller.
pub fn reconcile_released_reservation(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    reservation: &FileReservationRow,
    expected_generation: &str,
) -> Result<bool, String> {
    cx.checkpoint()
        .map_err(|_| "reservation reconciliation cancelled".to_string())?;
    if expected_generation.is_empty()
        || expected_generation.len() > 128
        || !expected_generation
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("targeted release repair requires a valid captured generation".into());
    }
    let id = reservation
        .id
        .filter(|id| *id > 0)
        .ok_or("targeted release repair requires a positive reservation identity")?;
    if mcp_agent_mail_core::global_metrics()
        .system
        .disk_pressure_level
        .load()
        >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64()
    {
        return Err("disk pressure defers targeted reservation archive repair".into());
    }
    let _write_activity =
        mcp_agent_mail_db::write_barrier::try_begin_write_activity().ok_or_else(|| {
            "reservation reconciliation deferred: recovery promotion or admission contention"
                .to_string()
        })?;
    validate_source(pool, config)?;
    if corruption_circuit_breaker().is_tripped() {
        return Err("reservation reconciliation refused: corruption breaker open".into());
    }
    {
        let conn = outcome(block_on(pool.acquire(cx)))?;
        let mode = conn
            .query_sync("PRAGMA query_only", &[])
            .map_err(source_error)?;
        if mode.first().and_then(|row| row.get_as::<i64>(0).ok()) != Some(0) {
            return Err("query-only snapshots cannot authorize reservation archive repair".into());
        }
    }
    let source = read_source(cx, pool, id)?
        .ok_or("captured reservation has no live terminal release authority")?;
    if source.generation.as_deref() != Some(expected_generation)
        || source.project_id != reservation.project_id
        || source.agent_id != reservation.agent_id
        || timestamp(&source.artifact["created_ts"]) != Some(reservation.created_ts)
        || source.artifact["path_pattern"].as_str() != Some(reservation.path_pattern.trim())
        || source.artifact["exclusive"].as_bool().map(i64::from) != Some(reservation.exclusive)
    {
        return Err("captured release identity or generation changed; archive preserved".into());
    }
    publish_release(
        cx,
        pool,
        config,
        &source,
        ReleaseTargets::StableOnly,
        &mut false,
    )
}

/// Repair terminal releases without client reads or reservation mutations.
///
/// A pass selects at most 32 reservation IDs and attempts at most four releases.
/// Up to 16 unexpired candidates receive the first two repair slots; a separate
/// finite history cursor receives the remaining selection and repair budget,
/// advancing through active rows without writing them.
/// Old missing history therefore cannot postpone guard unblocking indefinitely,
/// and a stream of recent releases cannot starve historical convergence.
/// Generation/identity conflicts, oversized artifacts and symlinks are retained
/// for manual review. A finite cursor revisits failures on later rounds. Source
/// connections are released before Git I/O; a write-activity lease prevents
/// recovery promotion from replacing the source generation during the pass.
/// Closed or contended admission defers the pass without advancing either cursor.
pub fn reconcile_reservation_releases(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ReservationReconcileCursor,
    stop: &AtomicBool,
) -> Result<ReservationReconcileReport, String> {
    let mut report = ReservationReconcileReport::default();
    if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
        report.interrupted = true;
        return Ok(report);
    }
    let _write_activity =
        mcp_agent_mail_db::write_barrier::try_begin_write_activity().ok_or_else(|| {
            "reservation reconciliation deferred: recovery promotion or admission contention"
                .to_string()
        })?;
    validate_source(pool, config)?;
    if corruption_circuit_breaker().is_tripped() {
        return Err("reservation reconciliation refused: corruption breaker open".into());
    }
    let identity = pool.sqlite_identity_key();
    if identity != cursor.source_identity {
        *cursor = ReservationReconcileCursor {
            source_identity: identity,
            ..Default::default()
        };
    }
    let mut selected_count = 0;
    for unexpired_only in [true, false] {
        let position = if unexpired_only {
            &mut cursor.priority
        } else {
            &mut cursor.history
        };
        let id_limit = if unexpired_only {
            PRIORITY_IDS_PER_BATCH
        } else {
            IDS_PER_BATCH - selected_count
        };
        let repair_limit = if unexpired_only {
            PRIORITY_REPAIRS_PER_BATCH
        } else {
            MAX_REPAIRS_PER_BATCH
        };
        let ids = select_ids(cx, pool, position, id_limit, unexpired_only)?;
        selected_count += i64::try_from(ids.len()).expect("bounded selection");
        for id in ids {
            if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
                report.interrupted = true;
                return Ok(report);
            }
            if report.attempted >= repair_limit {
                report.budget_exhausted = true;
                break;
            }
            if corruption_circuit_breaker().is_tripped() {
                return Err(
                    "reservation reconciliation stopped: source corruption observed".into(),
                );
            }
            let mut attempted = false;
            let result = read_source(cx, pool, id).and_then(|source| {
                source.map_or(Ok(false), |source| {
                    let result = publish_release(
                        cx,
                        pool,
                        config,
                        &source,
                        ReleaseTargets::WithOwnedAliases,
                        &mut attempted,
                    );
                    tracing::info!(target: "maintenance", event = "reservation_release_reconcile",
                        reservation_id = id, project = %source.project_slug,
                        generation = ?source.generation, db_state = "released",
                        action = "verify_terminal_release", outcome = ?result,
                        "reservation archive release reconciliation decision");
                    result
                })
            });
            report.scanned += 1;
            report.attempted += usize::from(attempted);
            match result {
                Ok(true) => report.repaired += 1,
                Ok(false) => report.unchanged += 1,
                Err(reason) => {
                    report.deferred += 1;
                    tracing::warn!(target: "maintenance", event = "reservation_release_reconcile_deferred",
                        reservation_id = id, reason = %reason, "reservation source and conflicting archive evidence retained");
                }
            }
            position.after = id;
            if position.ceiling == Some(id) {
                position.ceiling = None;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_round_trips_and_keeps_the_latest_repair_across_owners() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_progress(root.path()), None);
        let repaired = ReservationReconcileProgress {
            pid: 1,
            pass_us: 100,
            last_repair_us: Some(90),
        };
        record_progress(root.path(), &repaired).unwrap();
        assert_eq!(read_progress(root.path()), Some(repaired));
        // A second owner whose passes repair nothing must not erase the
        // first one's evidence of progress.
        let idle = ReservationReconcileProgress {
            pid: 2,
            pass_us: 200,
            last_repair_us: None,
        };
        record_progress(root.path(), &idle).unwrap();
        assert_eq!(
            read_progress(root.path()),
            Some(ReservationReconcileProgress {
                last_repair_us: Some(90),
                ..idle
            })
        );
        // A repair time from the future (written before the clock was
        // stepped back) is dropped, not kept "recent" forever by the merge.
        let future = ReservationReconcileProgress {
            pid: 3,
            pass_us: 300 + 10 * PROGRESS_CLOCK_SKEW_US,
            last_repair_us: Some(300 + 10 * PROGRESS_CLOCK_SKEW_US),
        };
        record_progress(root.path(), &future).unwrap();
        let after_step_back = ReservationReconcileProgress {
            pid: 4,
            pass_us: 400,
            last_repair_us: None,
        };
        record_progress(root.path(), &after_step_back).unwrap();
        assert_eq!(read_progress(root.path()), Some(after_step_back));
        // Garbage reads as absent rather than as progress.
        fs::write(progress_path(root.path()), b"not json").unwrap();
        assert_eq!(read_progress(root.path()), None);
    }

    #[cfg(unix)]
    #[test]
    fn progress_refuses_a_symlinked_doctor_directory() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("doctor")).unwrap();
        let progress = ReservationReconcileProgress {
            pid: 1,
            pass_us: 1,
            last_repair_us: None,
        };
        assert!(record_progress(root.path(), &progress).is_err());
        assert!(fs::read_dir(elsewhere.path()).unwrap().next().is_none());
    }

    fn fixture(test: impl FnOnce(&Cx, &DbPool, &Config)) {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let config = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                    &temp.path().join("mail.sqlite3"),
                ),
                storage_root: temp.path().join("archive"),
                ..Default::default()
            };
            fs::create_dir_all(&config.storage_root).unwrap();
            let pool = mcp_agent_mail_db::create_pool(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let cx = Cx::for_testing();
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'project', '/project', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1)").unwrap();
            conn.execute_raw(
                "INSERT OR REPLACE INTO db_identity(singleton, generation_id) VALUES(0, 'aabb')",
            )
            .unwrap();
            conn.execute_raw("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) \
                VALUES(401, 71, 81, 'src/*.rs', 1, 'release-test', 1000000, 9000000, NULL)").unwrap();
            // The ledger is authoritative even if the hot released_ts is NULL.
            conn.execute_raw("INSERT INTO file_reservation_releases(reservation_id, released_ts) VALUES(401, 5000000)").unwrap();
            drop(conn);
            test(&cx, &pool, &config);
        });
    }

    fn seed_artifact(
        config: &Config,
        source: &ReleaseSource,
        generation: &str,
        corrupt_identity: bool,
    ) -> (PathBuf, Vec<u8>) {
        let archive = crate::ensure_archive(config, &source.project_slug).unwrap();
        let mut value = source.artifact.clone();
        value["released_ts"] = Value::Null;
        value["db_generation"] = json!(generation);
        value["operator_note"] = json!("preserve this note");
        if corrupt_identity {
            value["created_ts"] = json!("1970-01-01T00:00:02Z");
        }
        let path = archive
            .root
            .join("file_reservations")
            .join(reservation_artifact_filename(Some(generation), source.id));
        crate::ensure_parent_dir(&path).unwrap();
        let bytes = serde_json::to_vec_pretty(&value).unwrap();
        fs::write(&path, &bytes).unwrap();
        (path, bytes)
    }

    #[test]
    fn released_ledger_repairs_expired_artifact_and_commits_idempotently() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let (path, _) = seed_artifact(config, &source, "aabb", false);
            let mut cursor = ReservationReconcileCursor::default();
            let report = reconcile_reservation_releases(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(report.repaired, 1);
            assert_eq!(report.deferred, 0);
            let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(value["released_ts"], source.artifact["released_ts"]);
            assert_eq!(value["operator_note"], "preserve this note");
            let repo = Repository::open(&config.storage_root).unwrap();
            let head = repo.head().unwrap().target().unwrap();
            let again = reconcile_reservation_releases(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(again.unchanged, 1);
            assert_eq!(again.repaired, 0);
            assert_eq!(repo.head().unwrap().target().unwrap(), head);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let rows = conn
                .query_sync(
                    "SELECT released_ts FROM file_reservations WHERE id=401",
                    &[],
                )
                .unwrap();
            assert_eq!(
                rows[0].get_as::<Option<i64>>(0).unwrap(),
                None,
                "repair does not rewrite mailbox rows"
            );
        });
    }

    #[test]
    fn foreign_generation_and_conflicting_identity_remain_byte_identical() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let (foreign, foreign_bytes) = seed_artifact(config, &source, "0011", false);
            let (current, conflict_bytes) = seed_artifact(config, &source, "aabb", true);
            let mut cursor = ReservationReconcileCursor::default();
            let report = reconcile_reservation_releases(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(report.deferred, 1);
            assert_eq!(report.repaired, 0);
            assert_eq!(fs::read(&foreign).unwrap(), foreign_bytes);
            assert_eq!(fs::read(&current).unwrap(), conflict_bytes);
            fs::rename(&current, current.with_extension("held")).unwrap();
            let repaired = reconcile_reservation_releases(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(repaired.repaired, 1);
            assert_eq!(fs::read(&foreign).unwrap(), foreign_bytes);
            assert_eq!(
                read_artifact(&current).unwrap().unwrap().value["released_ts"],
                source.artifact["released_ts"]
            );
        });
    }

    #[test]
    fn changed_source_generation_cannot_publish_a_captured_release() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let (path, before) = seed_artifact(config, &source, "aabb", false);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE db_identity SET generation_id='ccdd' WHERE singleton=0")
                .unwrap();
            drop(conn);
            assert!(
                publish_release(
                    cx,
                    pool,
                    config,
                    &source,
                    ReleaseTargets::WithOwnedAliases,
                    &mut false,
                )
                .unwrap_err()
                .contains("source/generation changed")
            );
            assert_eq!(fs::read(path).unwrap(), before);
        });
    }

    #[test]
    fn malformed_generation_metadata_is_preserved_byte_for_byte() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let (path, _) = seed_artifact(config, &source, "aabb", false);
            for generation in [
                Value::Null,
                json!(7),
                json!(true),
                json!(""),
                json!("not-hex"),
            ] {
                let mut artifact = source.artifact.clone();
                artifact["db_generation"] = generation;
                artifact["released_ts"] = Value::Null;
                let before = serde_json::to_vec_pretty(&artifact).unwrap();
                fs::write(&path, &before).unwrap();
                let report = reconcile_reservation_releases(
                    cx,
                    pool,
                    config,
                    &mut ReservationReconcileCursor::default(),
                    &AtomicBool::new(false),
                )
                .unwrap();
                assert_eq!(report.repaired, 0);
                assert_eq!(report.deferred, 1);
                assert_eq!(fs::read(&path).unwrap(), before);
            }
        });
    }

    #[test]
    fn present_empty_source_generation_cannot_authorize_legacy_repair() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let (path, before) = seed_artifact(config, &source, "aabb", false);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE db_identity SET generation_id='' WHERE singleton=0")
                .unwrap();
            drop(conn);
            let report = reconcile_reservation_releases(
                cx,
                pool,
                config,
                &mut ReservationReconcileCursor::default(),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(report.repaired, 0);
            assert_eq!(report.deferred, 1);
            assert_eq!(fs::read(&path).unwrap(), before);
            assert!(!path.parent().unwrap().join("id-401.json").exists());
        });
    }

    #[test]
    fn cancellation_and_readonly_sources_do_not_authorize_repair() {
        fixture(|cx, pool, config| {
            let mut cursor = ReservationReconcileCursor::default();
            let stopped = reconcile_reservation_releases(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(true),
            )
            .unwrap();
            assert!(stopped.interrupted);
            assert_eq!(stopped.scanned, 0);
            let readonly = DbPool::new_query_only(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            assert!(
                reconcile_reservation_releases(
                    cx,
                    &readonly,
                    config,
                    &mut cursor,
                    &AtomicBool::new(false)
                )
                .unwrap_err()
                .contains("query-only")
            );
            assert!(
                !config
                    .storage_root
                    .join("projects/project/file_reservations")
                    .exists()
            );
        });
    }

    #[test]
    fn terminal_timestamp_conflicts_and_malformed_release_metadata_are_preserved() {
        fixture(|cx, pool, _| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            for released in [
                json!("nonsense"),
                json!(true),
                json!(4000000),
                json!(-1),
                json!("1969-12-31T23:59:59Z"),
            ] {
                let mut artifact = source.artifact.clone();
                artifact["released_ts"] = released;
                assert!(validate_artifact(&source, &artifact).is_err());
            }
            for released in [Value::Null, json!(0), json!("none"), json!("0")] {
                let mut artifact = source.artifact.clone();
                artifact["released_ts"] = released;
                validate_artifact(&source, &artifact).unwrap();
            }
        });
    }

    #[test]
    fn unexpired_releases_receive_priority_without_starving_old_history() {
        fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            for id in 402..442 {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, 'old/{id}.rs', 1, 'history', 1000000, 9000000, 5000000)")).unwrap();
            }
            let future = mcp_agent_mail_db::now_micros() + 86_400_000_000;
            for id in 900..904 {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, 'recent/{id}.rs', 1, 'priority', 1000000, {future}, 5000000)")).unwrap();
            }
            drop(conn);
            let mut recent = Vec::new();
            for id in 900..904 {
                let source = read_source(cx, pool, id).unwrap().unwrap();
                recent.push(seed_artifact(config, &source, "aabb", false).0);
            }
            let report = reconcile_reservation_releases(
                cx,
                pool,
                config,
                &mut ReservationReconcileCursor::default(),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(report.repaired, 4, "{report:?}");
            assert_eq!(report.deferred, 0, "{report:?}");
            assert!(report.scanned <= usize::try_from(IDS_PER_BATCH).unwrap());
            for (index, path) in recent.iter().enumerate() {
                let value = read_artifact(path).unwrap().unwrap().value;
                assert_eq!(
                    value["released_ts"].is_null(),
                    index >= 2,
                    "two priority repairs reserve progress for history"
                );
            }
            for id in [401, 402] {
                let path = config
                    .storage_root
                    .join("projects/project/file_reservations")
                    .join(reservation_artifact_filename(Some("aabb"), id));
                assert!(read_artifact(&path).unwrap().unwrap().value["released_ts"].is_string());
            }
        });
    }

    #[test]
    fn history_advances_a_bounded_id_window_through_active_rows() {
        fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            for id in 1..41 {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, 'active/{id}.rs', 1, 'unchanged', 1000000, 9000000, NULL)")).unwrap();
            }
            drop(conn);
            let mut cursor = ReservationReconcileCursor::default();
            let first = reconcile_reservation_releases(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(first.scanned, 32);
            assert_eq!(first.unchanged, 32);
            assert_eq!(first.repaired, 0);
            assert_eq!(cursor.history.after, 32);
            assert!(
                !config
                    .storage_root
                    .join("projects/project/file_reservations")
                    .exists()
            );
            let second = reconcile_reservation_releases(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(second.scanned, 9);
            assert_eq!(second.unchanged, 8);
            assert_eq!(second.repaired, 1);
            assert_eq!(second.deferred, 0);
            assert!(
                cursor.history.ceiling.is_none(),
                "finite history round ends"
            );
        });
    }

    #[test]
    fn failed_git_publications_consume_the_repair_attempt_budget() {
        fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            for id in 402..410 {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, 'failed/{id}.rs', 1, 'retry', 1000000, 9000000, 5000000)")).unwrap();
            }
            drop(conn);
            let archive = crate::ensure_archive(config, "project").unwrap();
            let mut invalid_signature = config.clone();
            invalid_signature.git_author_name = "invalid\0author".into();
            let mut cursor = ReservationReconcileCursor::default();
            let failed = reconcile_reservation_releases(
                cx,
                pool,
                &invalid_signature,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(failed.attempted, 4, "{failed:?}");
            assert_eq!(failed.repaired, 0);
            assert_eq!(failed.deferred, 4);
            assert!(failed.budget_exhausted);
            for id in 401..410 {
                let path = archive
                    .root
                    .join("file_reservations")
                    .join(reservation_artifact_filename(Some("aabb"), id));
                assert_eq!(
                    path.exists(),
                    id < 405,
                    "failed commits must still consume write budget"
                );
            }
            // Failure advances the finite round; a later healthy round can
            // commit the already materialized releases as well as the rest.
            let mut recovered = 0;
            for _ in 0..4 {
                recovered += reconcile_reservation_releases(
                    cx,
                    pool,
                    config,
                    &mut cursor,
                    &AtomicBool::new(false),
                )
                .unwrap()
                .repaired;
            }
            assert_eq!(recovered, 9);
        });
    }

    fn captured_row(cx: &Cx, pool: &DbPool, id: i64) -> FileReservationRow {
        outcome(block_on(
            mcp_agent_mail_db::queries::get_reservations_by_ids(cx, pool, &[id]),
        ))
        .unwrap()
        .into_iter()
        .next()
        .expect("fixture reservation")
    }

    #[test]
    fn targeted_release_preserves_reused_aliases_and_commits_only_stable_identity() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let captured = captured_row(cx, pool, 401);
            let (stable, _) = seed_artifact(config, &source, "aabb", false);
            let (foreign, foreign_bytes) = seed_artifact(config, &source, "0011", false);
            let directory = stable.parent().unwrap();
            let digest = hex::encode(Sha1::digest(b"src/*.rs"));
            let alias = directory.join(format!("{digest}.json"));
            let legacy = directory.join("id-401.json");
            let mut newer = source.artifact.clone();
            let created = mcp_agent_mail_db::now_micros();
            let expires = created + 3_600_000_000;
            newer["id"] = json!(402);
            newer["created_ts"] = json!(mcp_agent_mail_db::micros_to_iso(created));
            newer["expires_ts"] = json!(mcp_agent_mail_db::micros_to_iso(expires));
            newer["released_ts"] = Value::Null;
            let newer_bytes = serde_json::to_vec_pretty(&newer).unwrap();
            fs::write(&alias, &newer_bytes).unwrap();
            fs::write(&legacy, &foreign_bytes).unwrap();
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES(402, 71, 81, 'src/*.rs', 1, 'new lease', {created}, {expires}, NULL)")).unwrap();
            drop(conn);

            assert!(reconcile_released_reservation(cx, pool, config, &captured, "aabb").unwrap());
            let artifact = read_artifact(&stable).unwrap().unwrap();
            assert_eq!(
                artifact.value["released_ts"],
                source.artifact["released_ts"]
            );
            assert_eq!(artifact.value["operator_note"], "preserve this note");
            assert_eq!(fs::read(&alias).unwrap(), newer_bytes);
            assert_eq!(fs::read(&legacy).unwrap(), foreign_bytes);
            assert_eq!(fs::read(&foreign).unwrap(), foreign_bytes);
            assert_eq!(captured_row(cx, pool, 402).released_ts, None);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let hot = conn
                .query_sync(
                    "SELECT released_ts FROM file_reservations WHERE id=401",
                    &[],
                )
                .unwrap();
            assert_eq!(hot[0].get_as::<Option<i64>>(0).unwrap(), None);
            drop(conn);

            let repo = Repository::open(&config.storage_root).unwrap();
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            let tree = head.tree().unwrap();
            let parent_tree = head.parent(0).unwrap().tree().unwrap();
            let diff = repo
                .diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)
                .unwrap();
            let changed: Vec<_> = diff
                .deltas()
                .map(|delta| delta.new_file().path().unwrap().to_path_buf())
                .collect();
            assert_eq!(
                changed,
                vec![PathBuf::from(
                    "projects/project/file_reservations/id-401-gaabb.json"
                )]
            );
            assert!(!reconcile_released_reservation(cx, pool, config, &captured, "aabb").unwrap());
            assert_eq!(repo.head().unwrap().target(), Some(head.id()));
        });
    }

    #[test]
    fn targeted_release_rejects_missing_generation_and_recycled_identity() {
        fixture(|cx, pool, config| {
            let captured = captured_row(cx, pool, 401);
            for generation in ["", "ccdd", "not-a-generation"] {
                assert!(
                    reconcile_released_reservation(cx, pool, config, &captured, generation)
                        .is_err()
                );
            }
            for field in 0..5 {
                let mut changed = captured.clone();
                match field {
                    0 => changed.project_id += 1,
                    1 => changed.agent_id += 1,
                    2 => changed.created_ts += 1,
                    3 => changed.path_pattern = "different/*.rs".into(),
                    _ => changed.exclusive = 0,
                }
                assert!(
                    reconcile_released_reservation(cx, pool, config, &changed, "aabb").is_err()
                );
            }
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE db_identity SET generation_id='ccdd' WHERE singleton=0")
                .unwrap();
            drop(conn);
            assert!(reconcile_released_reservation(cx, pool, config, &captured, "aabb").is_err());
            assert!(!config.storage_root.join("projects").exists());
        });
    }

    #[test]
    fn targeted_release_requires_live_release_even_with_a_captured_terminal_row() {
        fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES(402, 71, 81, 'active.rs', 1, 'active', 1000000, 9000000, NULL)").unwrap();
            drop(conn);
            let mut captured = captured_row(cx, pool, 402);
            captured.released_ts = Some(5_000_000);
            assert!(reconcile_released_reservation(cx, pool, config, &captured, "aabb").is_err());
            assert_eq!(captured_row(cx, pool, 402).released_ts, None);
            assert!(!config.storage_root.join("projects").exists());
        });
    }

    #[test]
    fn targeted_release_preserves_conflicting_stable_evidence() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let captured = captured_row(cx, pool, 401);
            let (stable, before) = seed_artifact(config, &source, "aabb", true);
            assert!(reconcile_released_reservation(cx, pool, config, &captured, "aabb").is_err());
            assert_eq!(fs::read(&stable).unwrap(), before);
            let mut foreign = source.artifact.clone();
            foreign["db_generation"] = json!("ccdd");
            let before = serde_json::to_vec_pretty(&foreign).unwrap();
            fs::write(&stable, &before).unwrap();
            assert!(reconcile_released_reservation(cx, pool, config, &captured, "aabb").is_err());
            assert_eq!(fs::read(&stable).unwrap(), before);
        });
    }

    #[test]
    fn targeted_release_retry_reads_current_metadata_after_git_failure() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let captured = captured_row(cx, pool, 401);
            let (stable, _) = seed_artifact(config, &source, "aabb", false);
            let mut bad_signature = config.clone();
            bad_signature.git_author_name = "invalid\0author".into();
            assert!(
                reconcile_released_reservation(cx, pool, &bad_signature, &captured, "aabb")
                    .is_err()
            );
            assert_eq!(
                read_artifact(&stable).unwrap().unwrap().value["released_ts"],
                source.artifact["released_ts"]
            );
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(
                "UPDATE file_reservations SET reason='current database metadata' WHERE id=401",
            )
            .unwrap();
            drop(conn);
            assert!(reconcile_released_reservation(cx, pool, config, &captured, "aabb").unwrap());
            let artifact = read_artifact(&stable).unwrap().unwrap();
            assert_eq!(artifact.value["reason"], "current database metadata");
            assert_eq!(artifact.value["operator_note"], "preserve this note");
            let repo = Repository::open(&config.storage_root).unwrap();
            let tree = head_tree(&repo).unwrap().unwrap();
            let relative = "projects/project/file_reservations/id-401-gaabb.json";
            assert_eq!(
                committed_artifact(&repo, Some(&tree), relative)
                    .unwrap()
                    .unwrap()
                    .bytes,
                artifact.bytes
            );
        });
    }

    #[test]
    fn targeted_release_refuses_readonly_or_mismatched_sources() {
        fixture(|cx, pool, config| {
            let captured = captured_row(cx, pool, 401);
            let readonly = DbPool::new_query_only(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            assert!(
                reconcile_released_reservation(cx, &readonly, config, &captured, "aabb")
                    .unwrap_err()
                    .contains("query-only")
            );
            let mut different_root = config.clone();
            different_root.storage_root = config.storage_root.join("different");
            fs::create_dir_all(&different_root.storage_root).unwrap();
            assert!(
                reconcile_released_reservation(cx, pool, &different_root, &captured, "aabb")
                    .is_err()
            );
            assert!(!config.storage_root.join("projects").exists());
        });
    }

    fn isolated_admission_test() -> bool {
        const CHILD: &str = "AM_TEST_RESERVATION_ADMISSION_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named libtest thread");
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .output()
            .expect("run isolated reservation admission test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "isolated {name} failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        true
    }

    fn exercise_closed_admission(parent_writer: bool) {
        use mcp_agent_mail_db::write_barrier::{
            DrainOutcome, acquire_promotion_barrier_draining, active_writer_count,
            begin_write_activity, try_acquire_promotion_barrier_if_idle,
        };
        use std::time::Duration;

        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let captured = captured_row(cx, pool, 401);
            let (stable, before) = seed_artifact(config, &source, "aabb", false);
            crate::flush_async_commits();
            let repo = Repository::open(&config.storage_root).unwrap();
            let head_before = repo.head().unwrap().target();
            let parent = parent_writer.then(begin_write_activity);
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let recovery = std::thread::spawn(move || {
                let owner = if parent_writer {
                    let (owner, result) = acquire_promotion_barrier_draining(Duration::ZERO);
                    assert!(matches!(
                        result,
                        DrainOutcome::TimedOut {
                            remaining_writers: 1
                        }
                    ));
                    owner
                } else {
                    try_acquire_promotion_barrier_if_idle().expect("idle promotion")
                };
                ready_tx.send(()).unwrap();
                // Cleanup bound only: the assertions require explicit release
                // after repair returns, not expiry of this timeout.
                let released = release_rx.recv_timeout(Duration::from_secs(20));
                drop(owner);
                released
            });
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut cursor = ReservationReconcileCursor {
                source_identity: "retained-round".to_string(),
                priority: ScanPosition {
                    after: 9,
                    ceiling: Some(99),
                },
                history: ScanPosition {
                    after: 7,
                    ceiling: Some(401),
                },
            };
            let targeted = reconcile_released_reservation(cx, pool, config, &captured, "aabb");
            let historical = reconcile_reservation_releases(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            );
            let held_writers = active_writer_count();
            let bytes_before_release = fs::read(&stable).unwrap();
            let head_before_release = repo.head().unwrap().target();
            drop(parent);
            let _ = release_tx.send(());
            let explicitly_released = recovery.join().unwrap();
            assert!(
                explicitly_released.is_ok(),
                "repair waited for the promotion owner's cleanup timeout"
            );
            assert!(targeted.unwrap_err().contains("admission contention"));
            assert!(historical.unwrap_err().contains("admission contention"));
            assert_eq!(held_writers, usize::from(parent_writer));
            assert_eq!(active_writer_count(), 0);
            assert_eq!(bytes_before_release, before);
            assert_eq!(head_before_release, head_before);
            assert_eq!(cursor.source_identity, "retained-round");
            assert_eq!(
                (cursor.priority.after, cursor.priority.ceiling),
                (9, Some(99))
            );
            assert_eq!(
                (cursor.history.after, cursor.history.ceiling),
                (7, Some(401))
            );
            assert_eq!(read_source(cx, pool, 401).unwrap().unwrap(), source);

            // Once admission reopens, the same durable source repairs normally.
            assert!(reconcile_released_reservation(cx, pool, config, &captured, "aabb").unwrap());
            assert_eq!(
                read_artifact(&stable).unwrap().unwrap().value["released_ts"],
                source.artifact["released_ts"]
            );
            let resumed = reconcile_reservation_releases(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(resumed.deferred, 0);
            assert!(resumed.unchanged > 0);
            assert_eq!(read_source(cx, pool, 401).unwrap().unwrap(), source);
        });
    }

    #[test]
    fn reservation_repair_defers_under_promotion_without_mutating_files_or_cursors() {
        if isolated_admission_test() {
            return;
        }
        exercise_closed_admission(false);
    }

    #[test]
    fn nested_release_repair_does_not_wait_on_promotion_draining_its_parent() {
        if isolated_admission_test() {
            return;
        }
        exercise_closed_admission(true);
    }
}
