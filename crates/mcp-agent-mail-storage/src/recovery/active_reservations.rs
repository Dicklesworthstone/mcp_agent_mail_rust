//! Reconstruct missing stable ACTIVE reservation artifacts without client traffic.
//!
//! Publication is create-only: ordinary reservation writers do not all take the
//! project lock, so an active repair must never replace an existing file. A
//! concurrent release/renewal wins the destination instead. Only generation-
//! stamped stable paths are repaired; reusable digest aliases are never written.
//! Transient failures retain bounded ID-only retry debt, independently of the
//! finite discovery round. Retrying never restores a captured ACTIVE payload.

mod retry;

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use asupersync::{Cx, Outcome};
use fastmcp_core::block_on;
use git2::{ObjectType, Oid, Repository};
use mcp_agent_mail_core::{Config, reservation_artifact::reservation_artifact_filename};
use mcp_agent_mail_db::{DbConn, DbError, DbPool, corruption_circuit_breaker};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::agent_reconcile::{committed_artifact, head_tree, read_artifact};
use retry::RetryQueue;

const IDS_PER_PASS: i64 = 32;
const WRITES_PER_PASS: usize = 4;
const SOURCE_BYTES: i64 = 32 * 1024;
const ARTIFACT_BYTES: usize = 128 * 1024;
const GRANT_GRACE_US: i64 = 30 * 1_000_000;
const PROGRESS_FILE: &str = "active-reservation-reconcile.json";
const MAX_PROGRESS_BYTES: u64 = 4096;
/// How far in the future a recorded progress time may lie and still count:
/// beyond it the clock moved backwards, and a future time must not read as
/// recent progress (or survive the max-merge) indefinitely.
pub const PROGRESS_CLOCK_SKEW_US: i64 = 60 * 1_000_000;

/// What `am doctor health` needs to tell a converging active-reservation
/// reconciler from a stuck or absent one: when the last pass completed and
/// when one last republished a missing active artifact (br-kp1in.19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveReservationProgress {
    pub pid: u32,
    /// Completion of the last pass, microseconds since the epoch.
    pub pass_us: i64,
    /// The last pass, by any owner of this mailbox, that republished at least
    /// one missing active artifact.
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
    progress: &ActiveReservationProgress,
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
pub fn read_progress(storage_root: &Path) -> Option<ActiveReservationProgress> {
    let path = progress_path(storage_root);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_PROGRESS_BYTES {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Read-only candidate view used by `reservation_parity` to check whether a
/// missing active artifact row satisfies the exact source and destination
/// invariants required by [`reconcile_active_reservations`] (br-kp1in.19).
#[derive(Debug, Clone, Copy)]
pub struct ActiveReservationCandidate<'a> {
    pub id: i64,
    pub project_id: i64,
    pub agent_id: i64,
    pub slug: &'a str,
    pub human_key: &'a str,
    pub resolved_agent_name: Option<&'a str>,
    pub path_pattern: &'a str,
    pub reason: &'a str,
    pub exclusive: i64,
    pub created_ts: i64,
    pub expires_ts: i64,
    pub hot_release_ts: Option<i64>,
    pub ledger_release_ts: Option<i64>,
    pub live_generation: Option<&'a str>,
}

/// Whether [`reconcile_active_reservations`] can reconstruct the missing
/// generation-stamped artifact for `candidate` once its 30s grace window
/// elapses. Rows with missing generation authority, missing holder agents,
/// invalid archive components/patterns, expired leases, or an occupied/symlinked
/// destination are permanently deferred by the reconciler and must not be
/// counted as converging.
#[must_use]
pub fn is_reconcilable_missing_active_reservation(
    candidate: &ActiveReservationCandidate<'_>,
    storage_root: &Path,
    now_us: i64,
) -> bool {
    let Some(generation) = candidate.live_generation else {
        return false;
    };
    if generation.is_empty()
        || generation.len() > 128
        || !generation.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return false;
    }
    let Some(agent_name) = candidate.resolved_agent_name else {
        return false;
    };
    if candidate.hot_release_ts.is_some_and(|ts| ts > 0) || candidate.ledger_release_ts.is_some() {
        return false;
    }
    if candidate.id <= 0 || candidate.project_id <= 0 || candidate.agent_id <= 0 {
        return false;
    }
    if crate::validate_archive_component("project slug", candidate.slug).is_err()
        || crate::validate_archive_component("agent name", agent_name).is_err()
    {
        return false;
    }
    let pattern = candidate.path_pattern.trim();
    let compiled = mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(pattern);
    if !Path::new(candidate.human_key).is_absolute()
        || pattern.is_empty()
        || candidate.path_pattern.contains("..")
        || Path::new(pattern).is_absolute()
        || !compiled.is_matchable()
        || !matches!(candidate.exclusive, 0 | 1)
        || candidate.created_ts <= 0
        || candidate.expires_ts <= candidate.created_ts
        || candidate.expires_ts <= now_us
        || [candidate.created_ts, candidate.expires_ts]
            .into_iter()
            .any(|ts| chrono::DateTime::from_timestamp_micros(ts).is_none())
    {
        return false;
    }
    let source_bytes = candidate
        .path_pattern
        .len()
        .saturating_add(candidate.reason.len())
        .saturating_add(candidate.slug.len())
        .saturating_add(candidate.human_key.len())
        .saturating_add(agent_name.len())
        .saturating_add(generation.len())
        .saturating_add(64);
    if i64::try_from(source_bytes).is_err_and(|_| true)
        || i64::try_from(source_bytes).unwrap_or(i64::MAX) > SOURCE_BYTES
    {
        return false;
    }
    let target = storage_root
        .join("projects")
        .join(candidate.slug)
        .join("file_reservations")
        .join(reservation_artifact_filename(
            Some(generation),
            candidate.id,
        ));
    !crate::path_existing_prefix_has_symlink(&target).unwrap_or(true)
        && fs::symlink_metadata(&target).is_err()
}

/// Finite ID rounds keep older missing artifacts reachable under new grants.
/// Retry IDs belong to the same source identity and share the original pass
/// budgets. Rebinding a replacement source discards both discovery and retry
/// hints; no stored hint grants authority to publish an artifact.
#[derive(Debug, Default)]
pub struct ActiveReservationCursor {
    identity: String,
    after: i64,
    ceiling: Option<i64>,
    retries: RetryQueue,
    retry_first: bool,
}

/// A bounded pass, not a certificate that every active lease is archived.
#[derive(Debug, Default, Serialize)]
pub struct ActiveReservationReport {
    pub scanned: usize,
    pub unchanged: usize,
    pub attempted: usize,
    pub repaired: usize,
    pub deferred: usize,
    pub deferred_transient: usize,
    pub retry_len: usize,
    /// Discovery has more candidates, or retained retry debt still exists.
    /// Cooling retries remain debt even when this pass scans no payloads.
    pub more: bool,
    pub interrupted: bool,
}

#[derive(Debug)]
struct RepairFailure {
    detail: String,
    transient: bool,
}

impl RepairFailure {
    fn source_unavailable(error: impl std::fmt::Display) -> Self {
        Self {
            detail: source_error(error),
            transient: true,
        }
    }
}

impl From<String> for RepairFailure {
    fn from(detail: String) -> Self {
        Self {
            detail,
            transient: false,
        }
    }
}

impl From<&str> for RepairFailure {
    fn from(detail: &str) -> Self {
        detail.to_string().into()
    }
}

impl From<crate::StorageError> for RepairFailure {
    fn from(error: crate::StorageError) -> Self {
        let transient = matches!(
            &error,
            crate::StorageError::LockContention { .. }
                | crate::StorageError::LockTimeout(_)
                | crate::StorageError::GitIndexLock { .. }
        ) || matches!(&error, crate::StorageError::Git(error)
            if error.code() == git2::ErrorCode::Locked)
            || matches!(&error, crate::StorageError::Io(error)
                if matches!(error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::AlreadyExists
                        | std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::ResourceBusy
                        | std::io::ErrorKind::StorageFull
                        | std::io::ErrorKind::QuotaExceeded
                        | std::io::ErrorKind::OutOfMemory));
        // AlreadyExists can be a concurrent create-only publisher. A retry
        // revalidates its file; it never changes noclobber into replacement.
        // Resource pressure likewise proves no conflicting lease. Every
        // retained ID must reread its current grant/release state on retry.
        Self {
            detail: error.to_string(),
            transient,
        }
    }
}

#[derive(Debug, PartialEq)]
struct Source {
    id: i64,
    project_id: i64,
    agent_id: i64,
    slug: String,
    generation: String,
    artifact: Value,
}

fn invalid(message: impl Into<String>) -> crate::StorageError {
    crate::StorageError::InvalidPath(message.into())
}

fn source_error(error: impl std::fmt::Display) -> String {
    let error = DbError::Sqlite(error.to_string());
    corruption_circuit_breaker().observe_error(&error);
    format!("active reservation source unavailable: {error}")
}

fn outcome<T, E: std::fmt::Display>(value: Outcome<T, E>) -> Result<T, String> {
    match value {
        Outcome::Ok(value) => Ok(value),
        Outcome::Err(error) => Err(source_error(error)),
        Outcome::Cancelled(_) => Err("active reservation repair cancelled".into()),
        Outcome::Panicked(_) => Err("active reservation source panicked".into()),
    }
}

fn timestamp(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        value.as_str().and_then(|text| {
            text.trim()
                .parse()
                .ok()
                .or_else(|| mcp_agent_mail_core::iso_to_micros(text.trim()))
        })
    })
}

fn validate_source(cx: &Cx, pool: &DbPool, config: &Config) -> Result<(), String> {
    let selected =
        mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url)
            .ok_or("active reservation repair requires a file-backed mailbox")?;
    if fs::canonicalize(selected).map_err(source_error)?
        != fs::canonicalize(pool.sqlite_path()).map_err(source_error)?
        || pool.search_identity_path() != pool.sqlite_path()
        || fs::canonicalize(pool.storage_root()).map_err(source_error)?
            != fs::canonicalize(&config.storage_root).map_err(source_error)?
    {
        return Err("active reservation repair refuses a foreign source or archive root".into());
    }
    let conn = outcome(block_on(pool.acquire(cx)))?;
    let rows = conn
        .query_sync("PRAGMA query_only", &[])
        .map_err(source_error)?;
    if rows.first().and_then(|row| row.get_as::<i64>(0).ok()) != Some(0) {
        return Err("query-only snapshots cannot authorize active reservation repair".into());
    }
    Ok(())
}

fn read_source(cx: &Cx, pool: &DbPool, id: i64) -> Result<Option<Source>, RepairFailure> {
    let conn = outcome(block_on(pool.acquire(cx))).map_err(|detail| RepairFailure {
        detail,
        transient: true,
    })?;
    let result = read_source_connection(&conn, id);
    if result.as_ref().is_err_and(|error| error.transient) {
        // Retrying the same failed pager in the worker's single idle slot can
        // wedge otherwise valid grants indefinitely. Successful reads that
        // expose conflicting lease evidence must not churn the pool instead.
        mcp_agent_mail_db::close_db_conn(conn.detach(), "failed active reservation source read");
    }
    result
}

fn read_source_connection(conn: &DbConn, id: i64) -> Result<Option<Source>, RepairFailure> {
    // One SQL snapshot supplies the holder, generation and both release stores.
    // The size predicate bounds materialized strings, not the SQL engine's work.
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
        &[id.into(), SOURCE_BYTES.into()],
    ).map_err(RepairFailure::source_unavailable)?;
    let [row] = rows.as_slice() else {
        return Err(
            "active reservation source missing, oversized, or without holder authority".into(),
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
    // A malformed release is uncertainty, never proof that this lease is active.
    if let Some(hot) = hot {
        let hot =
            timestamp(&Value::String(hot)).ok_or("malformed reservation release timestamp")?;
        if hot > 0 {
            return Ok(None);
        }
    }
    if let Some(ledger) = ledger {
        if ledger > 0 {
            return Ok(None);
        }
        return Err("malformed reservation release ledger timestamp".into());
    }
    let created = integer("created_ts")?;
    let expires = integer("expires_ts")?;
    let now = mcp_agent_mail_db::now_micros();
    if expires <= now || created > now.saturating_sub(GRANT_GRACE_US) {
        return Ok(None);
    }
    let generation = row
        .get_named::<Option<String>>("generation_id")
        .map_err(source_error)?
        .ok_or("active reservation repair requires a database generation")?;
    if generation.is_empty()
        || generation.len() > 128
        || !generation.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("active reservation database generation is invalid".into());
    }
    let slug = text("slug")?;
    let name = text("name")?;
    crate::validate_archive_component("project slug", &slug).map_err(source_error)?;
    crate::validate_archive_component("agent name", &name).map_err(source_error)?;
    let key = text("human_key")?;
    let pattern = text("path_pattern")?;
    let compiled = mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(pattern.trim());
    let exclusive = integer("exclusive")?;
    if id <= 0
        || integer("id")? != id
        || integer("project_id")? <= 0
        || integer("agent_id")? <= 0
        || !Path::new(&key).is_absolute()
        || pattern.trim().is_empty()
        || pattern.contains("..")
        || Path::new(pattern.trim()).is_absolute()
        || !compiled.is_matchable()
        || !matches!(exclusive, 0 | 1)
        || created <= 0
        || expires <= created
        || [created, expires]
            .into_iter()
            .any(|ts| chrono::DateTime::from_timestamp_micros(ts).is_none())
    {
        return Err("active reservation identity or timestamps are invalid".into());
    }
    Ok(Some(Source {
        id,
        project_id: integer("project_id")?,
        agent_id: integer("agent_id")?,
        slug,
        generation: generation.clone(),
        artifact: json!({
            "id": id, "project": key, "agent": name, "path_pattern": pattern.trim(),
            "exclusive": exclusive != 0, "reason": text("reason")?,
            "created_ts": mcp_agent_mail_db::micros_to_iso(created),
            "expires_ts": mcp_agent_mail_db::micros_to_iso(expires),
            "db_generation": generation,
        }),
    }))
}

fn validate_evidence(source: &Source, value: &Value) -> crate::Result<()> {
    for field in [
        "id",
        "project",
        "agent",
        "path_pattern",
        "exclusive",
        "db_generation",
    ] {
        if value.get(field) != source.artifact.get(field) {
            return Err(invalid(format!(
                "active reservation {field} conflicts; evidence preserved"
            )));
        }
    }
    if timestamp(&value["created_ts"]) != timestamp(&source.artifact["created_ts"]) {
        return Err(invalid(
            "active reservation creation identity conflicts; preserved",
        ));
    }
    if !value["released_ts"].is_null() && timestamp(&value["released_ts"]).is_none_or(|ts| ts > 0) {
        return Err(invalid(
            "terminal or malformed archive release cannot become active",
        ));
    }
    if timestamp(&value["expires_ts"])
        .is_none_or(|ts| Some(ts) > timestamp(&source.artifact["expires_ts"]))
    {
        return Err(invalid(
            "archive expiry is missing or newer than the live source; preserved",
        ));
    }
    Ok(())
}

/// Publish complete bytes only to an absent destination. A competing writer, including
/// one that publishes a release after our last DB read, always keeps its file.
fn publish_missing(path: &Path, bytes: &[u8]) -> crate::Result<()> {
    crate::ensure_parent_dir(path)?;
    if crate::path_existing_prefix_has_symlink(path)? {
        return Err(invalid("active reservation destination contains a symlink"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("reservation has no parent directory"))?;
    let mut staged = tempfile::Builder::new()
        .prefix(".active-reservation-")
        .tempfile_in(parent)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    let file = staged
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    file.sync_all()?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn repair_one(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    original: &Source,
    attempted: &mut bool,
    stop: &AtomicBool,
) -> Result<bool, RepairFailure> {
    checkpoint(cx, stop)?;
    // An exhausted pool may be owned by a borrower that needs this archive
    // fence before returning its connection. Checkout must precede BOTH
    // archive locks; all source rechecks below reuse this same connection.
    let checkout = pool.acquire(cx);
    #[cfg(test)]
    let checkout = tests::observe_checkout_pending(checkout);
    let conn = outcome(block_on(checkout)).map_err(|detail| RepairFailure {
        detail,
        transient: true,
    })?;
    let mut conn = Some(conn);
    // Keep the source lease out of an unbounded archive wait. Use the same
    // native fence and project locks as terminal/message recovery, not a
    // second publication protocol or a detached writer.
    let mutation = crate::ArchiveMutationGuard::try_begin_repair(&config.storage_root, || {
        stop.load(Ordering::Acquire) || cx.checkpoint().is_err()
    })
    .map_err(RepairFailure::from)?;
    let archive = crate::ensure_archive(config, &original.slug).map_err(RepairFailure::from)?;
    // The project-lock closure returns StorageError. Retain the classification
    // of an unavailable source recheck instead of disguising it as conflicting
    // archive evidence when crossing that boundary.
    let mut source_transient = false;
    let publish = || {
        checkpoint(cx, stop)?;
        if read_source_connection(
            conn.as_ref().expect("retained source connection"),
            original.id,
        )
        .map_err(|error| {
            source_transient |= error.transient;
            invalid(error.detail)
        })?
        .as_ref()
            != Some(original)
        {
            return Err(invalid(
                "active reservation source changed before publication",
            ));
        }
        let root = crate::archive_project_root_checked(&archive)?;
        let repo_root = crate::archive_repo_root_checked(&archive)?;
        let repo = Repository::open(repo_root)?;
        let tree = head_tree(&repo)?;
        let project_path = root.join("project.json");
        let project_rel = crate::rel_path_cached(&archive.canonical_repo_root, &project_path)?;
        for artifact in [
            read_artifact(&project_path)?,
            committed_artifact(&repo, tree.as_ref(), &project_rel)?,
        ]
        .into_iter()
        .flatten()
        {
            if artifact.value["slug"] != original.slug
                || artifact.value["human_key"] != original.artifact["project"]
            {
                return Err(invalid(
                    "archived project conflicts with active reservation source",
                ));
            }
        }
        let path = root
            .join("file_reservations")
            .join(reservation_artifact_filename(
                Some(&original.generation),
                original.id,
            ));
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, &path)?;
        let working = read_artifact(&path)?;
        let committed = committed_artifact(&repo, tree.as_ref(), &relative)?;
        for artifact in [working.as_ref(), committed.as_ref()].into_iter().flatten() {
            validate_evidence(original, &artifact.value)?;
        }
        let bytes = if let Some(working) = &working {
            // Existing files are never rewritten, even to extend an old expiry.
            for field in ["expires_ts", "reason"] {
                if working.value[field] != original.artifact[field] {
                    return Err(invalid(
                        "existing active artifact differs; create-only repair refused",
                    ));
                }
            }
            working.bytes.clone()
        } else {
            let mut value = committed
                .as_ref()
                .map_or_else(|| json!({}), |a| a.value.clone());
            let object = value.as_object_mut().expect("validated artifact object");
            // Source absence is authoritative. Do not retain a historical
            // nonpositive release spelling that another reader could misread.
            object.remove("released_ts");
            object.extend(
                original
                    .artifact
                    .as_object()
                    .expect("source object")
                    .clone(),
            );
            serde_json::to_vec_pretty(&value)?
        };
        if bytes.len() > ARTIFACT_BYTES {
            return Err(invalid("active reservation artifact exceeds byte bound"));
        }
        if working.is_some() && committed.as_ref().is_some_and(|a| a.bytes == bytes) {
            return Ok(false);
        }
        if read_source_connection(
            conn.as_ref().expect("retained source connection"),
            original.id,
        )
        .map_err(|error| {
            source_transient |= error.transient;
            invalid(error.detail)
        })?
        .as_ref()
            != Some(original)
        {
            return Err(invalid(
                "active reservation source changed during verification",
            ));
        }
        // Publication needs the outer generation lease, not a pool slot.
        // Release capacity before filesystem syncs and Git commit work.
        drop(conn.take());
        checkpoint(cx, stop)?;
        *attempted = true;
        if working.is_none() {
            publish_missing(&path, &bytes)?;
        }
        // The commit helper reads the current file, not the captured ACTIVE
        // payload. A racing terminal writer can therefore only supersede it;
        // no stale active WriteOp is ever queued for a later overwrite.
        checkpoint(cx, stop)?;
        crate::commit_paths_with_retry(
            repo_root,
            config,
            &format!("repair: missing active reservation {}", original.id),
            &[relative.as_str()],
        )?;
        let verified = Repository::open(repo_root)?;
        let tree = head_tree(&verified)?
            .ok_or_else(|| invalid("active reservation commit has no HEAD"))?;
        let entry = tree.get_path(Path::new(&relative))?;
        if entry.kind() != Some(ObjectType::Blob)
            || !matches!(entry.filemode(), 0o100644 | 0o100755)
            || entry.id()
                != Oid::hash_object_ext(ObjectType::Blob, &bytes, entry.id().object_format())?
            || verified.find_blob(entry.id())?.content() != bytes
            || read_artifact(&path)?.is_none_or(|a| a.bytes != bytes)
        {
            return Err(invalid(
                "active reservation changed during commit; current evidence retained",
            ));
        }
        Ok(true)
    };
    let result = archive.with_repair_lock(
        || stop.load(Ordering::Acquire) || cx.checkpoint().is_err(),
        publish,
    );
    drop(mutation);
    if source_transient && let Some(conn) = conn.take() {
        mcp_agent_mail_db::close_db_conn(
            conn.detach(),
            "failed active reservation publication source read",
        );
    }
    result.map_err(|error| {
        let mut failure = RepairFailure::from(error);
        failure.transient |= source_transient;
        failure
    })
}

fn checkpoint(cx: &Cx, stop: &AtomicBool) -> crate::Result<()> {
    if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "active reservation repair cancelled",
        )
        .into());
    }
    Ok(())
}

// The caller owns database admission throughout source binding and selection.
// Keep the existing finite ID window and all active/grace/release predicates.
fn select_ids(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ActiveReservationCursor,
) -> Result<Vec<i64>, String> {
    validate_source(cx, pool, config)?;
    let identity = pool.sqlite_identity_key();
    if cursor.identity != identity {
        *cursor = ActiveReservationCursor {
            identity,
            ..Default::default()
        };
    }
    let conn = outcome(block_on(pool.acquire(cx)))?;
    if cursor.ceiling.is_none() {
        let rows = conn
            .query_sync("SELECT COALESCE(MAX(id), 0) FROM file_reservations", &[])
            .map_err(source_error)?;
        cursor.ceiling = Some(
            rows.first()
                .ok_or("reservation ID aggregate missing")?
                .get_as::<i64>(0)
                .map_err(source_error)?,
        );
        cursor.after = 0;
    }
    let rows = conn.query_sync(
        "SELECT id FROM file_reservations WHERE id > ? AND id <= ? AND expires_ts > ? \
         AND created_ts <= ? AND (released_ts IS NULL OR released_ts <= 0) \
         AND NOT EXISTS (SELECT 1 FROM file_reservation_releases rr WHERE rr.reservation_id = file_reservations.id) \
         ORDER BY id LIMIT ?",
        &[cursor.after.into(), cursor.ceiling.unwrap_or(0).into(),
          mcp_agent_mail_db::now_micros().into(),
          mcp_agent_mail_db::now_micros().saturating_sub(GRANT_GRACE_US).into(), IDS_PER_PASS.into()],
    ).map_err(source_error)?;
    rows.iter()
        .map(|row| row.get_named::<i64>("id").map_err(source_error))
        .collect()
}

/// Repair missing generation-stamped stable artifacts for settled active leases.
/// The pass never modifies database rows or overwrites an occupied destination.
/// Thirty-second-old grants are eligible; expired, released, unversioned and
/// ambiguous sources remain untouched. Row selection and writes are bounded,
/// but this is not a hard deadline on SQLite, Git or filesystem operations.
///
/// Closed database admission defers before consuming a candidate. A busy global
/// archive fence or project lock retains that lease in a bounded retry lane so
/// it need not wait behind the rest of a discovery round. Retry and discovery
/// share the original 32-source/four-write limits and alternate across passes;
/// repeated contention backs off. Conflicts never enter the retry queue. Worker
/// shutdown reaches lock admission and publication checkpoints; completed files
/// remain evidence for a safe retry. All hints remain process-local.
pub fn reconcile_active_reservations(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ActiveReservationCursor,
    stop: &AtomicBool,
) -> Result<ActiveReservationReport, String> {
    let mut report = ActiveReservationReport::default();
    if checkpoint(cx, stop).is_err() {
        report.interrupted = true;
        return Ok(report);
    }
    if corruption_circuit_breaker().is_tripped() {
        return Err("active reservation repair refused: corruption breaker open".into());
    }
    let ids = {
        let _selection = mcp_agent_mail_db::write_barrier::try_begin_write_activity().ok_or(
            "active reservation repair deferred: recovery promotion or admission contention",
        )?;
        select_ids(cx, pool, config, cursor)?
    };
    let selected_identity = cursor.identity.clone();
    let mut scan_more = ids.len() == usize::try_from(IDS_PER_PASS).expect("small ID bound");
    let mut scan = ids.into_iter().peekable();
    let mut retries = cursor.retries.due(Instant::now()).into_iter();
    let mut seen = HashSet::new();
    loop {
        // At capacity only retained IDs can free space. Otherwise alternate
        // lanes so sustained contention cannot starve unrelated discoveries.
        let next = if cursor.retry_first || cursor.retries.is_full() {
            retries
                .next()
                .map(|id| (id, true))
                .or_else(|| scan.next().map(|id| (id, false)))
        } else {
            scan.next()
                .map(|id| (id, false))
                .or_else(|| retries.next().map(|id| (id, true)))
        };
        let Some((id, is_retry)) = next else {
            break;
        };
        if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
            report.interrupted = true;
            scan_more = true;
            break;
        }
        // A scan duplicate may advance its position but cannot bypass a
        // retained ID's cooldown or consume the retry lane's payload budget.
        if !is_retry && (cursor.retries.contains(id) || seen.contains(&id)) {
            cursor.after = id;
            continue;
        }
        if !seen.insert(id) {
            continue;
        }
        if report.attempted >= WRITES_PER_PASS
            || report.scanned >= usize::try_from(IDS_PER_PASS).expect("small ID bound")
        {
            scan_more |= !is_retry || scan.peek().is_some();
            break;
        }
        if cursor.retries.is_full() && !cursor.retries.contains(id) {
            // Never advance past an ID whose transient failure could not be
            // retained. Existing retries keep their FIFO positions and bounds.
            scan_more = true;
            break;
        }
        if corruption_circuit_breaker().is_tripped() {
            return Err("active reservation repair stopped: source corruption observed".into());
        }
        // A promotion may have run after selection. Never apply a retained ID
        // page to an observed replacement source, and never wait while closed.
        let activity = mcp_agent_mail_db::write_barrier::try_begin_write_activity().ok_or(
            "active reservation repair deferred: recovery promotion or admission contention",
        )?;
        if pool.sqlite_identity_key() != selected_identity {
            return Err(
                "active reservation source changed after selection; rescan required".into(),
            );
        }
        let mut attempted = false;
        let result = read_source(cx, pool, id).and_then(|source| {
            source.map_or(Ok(false), |source| {
                repair_one(cx, pool, config, &source, &mut attempted, stop)
            })
        });
        // Do not retain source/promotion authority across outer diagnostics.
        drop(activity);
        report.attempted += usize::from(attempted);
        if checkpoint(cx, stop).is_err() {
            report.interrupted = true;
            scan_more = true;
            break;
        }
        report.scanned += 1;
        cursor.retry_first = !is_retry;
        match result {
            Ok(repaired) => {
                if repaired {
                    report.repaired += 1;
                } else {
                    report.unchanged += 1;
                }
                cursor.retries.forget(id);
            }
            Err(failure) => {
                report.deferred += 1;
                tracing::warn!(target: "maintenance", reservation_id = id,
                    reason = %failure.detail, transient = failure.transient,
                    "active reservation repair deferred; existing evidence preserved");
                if failure.transient {
                    report.deferred_transient += 1;
                    if !cursor.retries.defer(id, Instant::now()) {
                        // Defensive fallback: retain this scan position if a
                        // future caller ever reaches admission at capacity.
                        scan_more = true;
                        break;
                    }
                } else {
                    cursor.retries.forget(id);
                }
            }
        }
        if !is_retry {
            cursor.after = id;
        }
    }
    // Discovery wraps independently of retry debt. A cooling retry must not
    // pin a completed round or restart it early while there are unvisited IDs.
    if !scan_more && !report.interrupted {
        cursor.ceiling = None;
    }
    report.retry_len = cursor.retries.len();
    report.more = scan_more || report.retry_len > 0;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    std::thread_local! {
        static CHECKOUT_PENDING: std::cell::RefCell<Option<mpsc::Sender<()>>> =
            const { std::cell::RefCell::new(None) };
    }

    // Observe a real blocked checkout without substituting its outcome.
    pub(super) async fn observe_checkout_pending<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        std::future::poll_fn(|context| {
            let result = std::future::Future::poll(future.as_mut(), context);
            if result.is_pending() {
                CHECKOUT_PENDING.with(|slot| {
                    if let Some(sender) = slot.borrow_mut().take() {
                        let _ = sender.send(());
                    }
                });
            }
            result
        })
        .await
    }

    fn checkout_contention(change: Option<&str>, cancel: bool) {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let held = outcome(block_on(pool.acquire(cx))).unwrap();
            let worker_pool = pool.clone();
            let worker_config = config.clone();
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let worker_stop = std::sync::Arc::clone(&stop);
            let (pending_tx, pending_rx) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                CHECKOUT_PENDING.with(|slot| *slot.borrow_mut() = Some(pending_tx));
                let _activity = mcp_agent_mail_db::write_barrier::try_begin_write_activity()
                    .expect("source admission");
                let mut attempted = false;
                let result = repair_one(
                    &Cx::for_testing(),
                    &worker_pool,
                    &worker_config,
                    &source,
                    &mut attempted,
                    &worker_stop,
                );
                (result, attempted)
            });
            let pending = pending_rx.recv_timeout(Duration::from_secs(5));
            // The borrower needs the same archive fence before it can return
            // its sole connection. Never let a broken candidate hang the test:
            // observe the actual repair admission, then unblock and join it.
            let borrower_archive =
                crate::ArchiveMutationGuard::try_begin_repair(&config.storage_root, || false);
            let archive_available = borrower_archive.is_ok();
            let archive_untouched = !config.storage_root.join(".git").exists();
            drop(borrower_archive);
            if let Some(sql) = change {
                held.execute_raw(sql).unwrap();
            }
            if cancel {
                stop.store(true, Ordering::Release);
            }
            drop(held);
            let (result, attempted) = worker.join().unwrap();
            pending.expect("observe the exhausted real one-connection pool");
            assert!(
                archive_available,
                "active repair holds archive admission while awaiting the borrower's database connection"
            );
            assert!(
                archive_untouched,
                "source checkout must precede archive initialization"
            );
            if change.is_some() || cancel {
                assert!(
                    result.is_err(),
                    "stale or interrupted active payload was published"
                );
                assert!(!attempted);
                assert!(!target(config).exists());
            } else {
                assert!(result.unwrap());
                assert!(attempted);
                assert_committed(cx, pool, config, 401);
                assert_eq!(pass(cx, pool, config).repaired, 0);
            }
        });
    }

    #[test]
    fn active_checkout_contention_leaves_archive_admission_available() {
        checkout_contention(None, false);
    }

    #[test]
    fn active_checkout_contention_revalidates_release_renewal_and_generation() {
        for change in [
            "INSERT INTO file_reservation_releases(reservation_id,released_ts) VALUES(401,5000000)",
            "UPDATE file_reservations SET expires_ts=expires_ts+1000000 WHERE id=401",
            "UPDATE db_identity SET generation_id='ccdd' WHERE singleton=0",
        ] {
            checkout_contention(Some(change), false);
        }
    }

    #[test]
    fn active_checkout_contention_observes_shutdown_before_publication() {
        checkout_contention(None, true);
    }

    // These tests deliberately hold process-global admission locks. Isolate
    // them from unrelated libtests, while bounding a regressed child's life.
    fn isolated() -> bool {
        const CHILD: &str = "AM_TEST_ACTIVE_RESERVATION_ADMISSION_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named libtest thread");
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn isolated active reservation test");
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut expired = false;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                expired = true;
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !expired && output.status.success() && stdout.contains("1 passed; 0 failed"),
            "isolated {name} failed (timeout={expired}): {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        true
    }

    fn fixture(test: impl FnOnce(&Cx, &DbPool, &Config)) {
        if isolated() {
            return;
        }
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
            let expires = mcp_agent_mail_db::now_micros() + 3_600_000_000;
            conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES(401, 71, 81, 'src/*.rs', 1, 'active-test', 1000000, {expires}, NULL)")).unwrap();
            drop(conn);
            test(&cx, &pool, &config);
        });
    }

    fn target(config: &Config) -> std::path::PathBuf {
        config
            .storage_root
            .join("projects/project/file_reservations/id-401-gaabb.json")
    }

    fn pass(cx: &Cx, pool: &DbPool, config: &Config) -> ActiveReservationReport {
        reconcile_active_reservations(
            cx,
            pool,
            config,
            &mut ActiveReservationCursor::default(),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    fn query_only(conn: &DbConn) -> i64 {
        conn.query_sync("PRAGMA query_only", &[]).unwrap()[0]
            .get_as::<i64>(0)
            .unwrap()
    }

    #[test]
    fn active_failed_source_queries_replace_reader_and_resume_real_publication() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            for during_publication in [false, true] {
                let before = fs::read(target(config)).ok();
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                // Make the actual source SELECT fail, without changing any
                // lease row. A connection-local marker proves that recovery
                // does not just hand the failed one-slot reader back again.
                conn.execute_raw("BEGIN IMMEDIATE").unwrap();
                conn.execute_raw(
                    "ALTER TABLE file_reservation_releases RENAME TO retained_release_ledger",
                )
                .unwrap();
                conn.execute_raw("COMMIT").unwrap();
                conn.execute_raw("PRAGMA query_only = ON").unwrap();
                assert_eq!(query_only(&conn), 1);
                drop(conn);
                let mut attempted = false;
                let failure = if during_publication {
                    repair_one(
                        cx,
                        pool,
                        config,
                        &source,
                        &mut attempted,
                        &AtomicBool::new(false),
                    )
                    .unwrap_err()
                } else {
                    read_source(cx, pool, 401).unwrap_err()
                };
                assert!(failure.transient, "{failure:?}");
                assert!(!attempted, "a failed live source cannot publish a lease");
                assert_eq!(fs::read(target(config)).ok(), before);

                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                assert_eq!(
                    query_only(&conn),
                    0,
                    "the failed reader must be detached in both source-read phases"
                );
                conn.execute_raw("BEGIN IMMEDIATE").unwrap();
                conn.execute_raw(
                    "ALTER TABLE retained_release_ledger RENAME TO file_reservation_releases",
                )
                .unwrap();
                conn.execute_raw("COMMIT").unwrap();
                drop(conn);
                assert_eq!(read_source(cx, pool, 401).unwrap().unwrap(), source);
                let report = pass(cx, pool, config);
                assert_eq!(report.deferred, 0);
                assert_eq!(report.repaired, usize::from(before.is_none()));
                assert_committed(cx, pool, config, 401);
            }
        });
    }

    #[test]
    fn active_successful_or_conflicting_source_keeps_healthy_reader() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            for conflicting in [false, true] {
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                if conflicting {
                    conn.execute_raw(
                        "UPDATE db_identity SET generation_id = 'invalid/generation' WHERE singleton = 0",
                    )
                    .unwrap();
                }
                conn.execute_raw("PRAGMA query_only = ON").unwrap();
                drop(conn);
                let result = read_source(cx, pool, 401);
                if conflicting {
                    let failure = result.unwrap_err();
                    assert!(
                        !failure.transient,
                        "invalid authority is not a reader failure"
                    );
                    assert!(failure.detail.contains("generation is invalid"));
                } else {
                    assert_eq!(result.unwrap().unwrap(), source);
                }
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                assert_eq!(
                    query_only(&conn),
                    1,
                    "successful SQL must not churn the pool"
                );
                conn.execute_raw("PRAGMA query_only = OFF").unwrap();
                if conflicting {
                    conn.execute_raw(
                        "UPDATE db_identity SET generation_id = 'aabb' WHERE singleton = 0",
                    )
                    .unwrap();
                }
                drop(conn);
                assert!(!target(config).exists());
            }
            assert_eq!(read_source(cx, pool, 401).unwrap().unwrap(), source);
            assert_eq!(pass(cx, pool, config).repaired, 1);
            assert_committed(cx, pool, config, 401);
        });
    }

    #[test]
    fn missing_active_lease_is_committed_without_mutating_the_database_or_aliases() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let archive = crate::ensure_archive(config, &source.slug).unwrap();
            // Existing aliases and other generations are not targets of repair.
            let foreign = archive.root.join("file_reservations/id-401-gccdd.json");
            crate::ensure_parent_dir(&foreign).unwrap();
            fs::write(&foreign, b"retained foreign-generation evidence").unwrap();
            let report = pass(cx, pool, config);
            assert_eq!(
                (report.scanned, report.repaired, report.deferred),
                (1, 1, 0)
            );
            let path = target(config);
            assert_eq!(
                read_artifact(&path).unwrap().unwrap().value,
                source.artifact
            );
            assert_eq!(
                fs::read(&foreign).unwrap(),
                b"retained foreign-generation evidence"
            );
            let files = fs::read_dir(path.parent().unwrap()).unwrap().count();
            assert_eq!(files, 2, "no digest or legacy alias is manufactured");
            let repo = Repository::open(&config.storage_root).unwrap();
            let head = repo.head().unwrap().target().unwrap();
            let relative = crate::rel_path_cached(&archive.canonical_repo_root, &path).unwrap();
            let tree = head_tree(&repo).unwrap().unwrap();
            assert_eq!(
                committed_artifact(&repo, Some(&tree), &relative)
                    .unwrap()
                    .unwrap()
                    .value,
                source.artifact
            );
            assert_eq!(read_source(cx, pool, 401).unwrap().unwrap(), source);
            let repeated = pass(cx, pool, config);
            assert_eq!((repeated.repaired, repeated.unchanged), (0, 1));
            assert_eq!(repo.head().unwrap().target().unwrap(), head);
        });
    }

    #[test]
    fn released_expired_and_in_flight_grants_are_not_recreated() {
        for change in [
            "UPDATE file_reservations SET released_ts=5000000 WHERE id=401",
            "INSERT INTO file_reservation_releases(reservation_id, released_ts) VALUES(401, 5000000)",
            "UPDATE file_reservations SET expires_ts=9000000 WHERE id=401",
            "UPDATE file_reservations SET created_ts={now} WHERE id=401",
        ] {
            fixture(|cx, pool, config| {
                // The in-flight grant is stamped as the fixture runs: stamped
                // before the earlier fixtures, a slow host aged it past
                // GRANT_GRACE_US and the pass repaired it.
                let change = change.replace("{now}", &mcp_agent_mail_db::now_micros().to_string());
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                conn.execute_raw(&change).unwrap();
                drop(conn);
                let report = pass(cx, pool, config);
                assert_eq!(report.repaired, 0, "{change}");
                assert!(!target(config).exists());
            });
        }
    }

    #[test]
    fn captured_active_state_cannot_override_a_release_renewal_or_new_generation() {
        for change in [
            "INSERT INTO file_reservation_releases(reservation_id, released_ts) VALUES(401, 5000000)",
            "UPDATE file_reservations SET expires_ts=expires_ts+1000000 WHERE id=401",
            "UPDATE db_identity SET generation_id='ccdd' WHERE singleton=0",
        ] {
            fixture(|cx, pool, config| {
                let captured = read_source(cx, pool, 401).unwrap().unwrap();
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                conn.execute_raw(change).unwrap();
                drop(conn);
                let mut attempted = false;
                assert!(
                    repair_one(
                        cx,
                        pool,
                        config,
                        &captured,
                        &mut attempted,
                        &AtomicBool::new(false)
                    )
                    .is_err()
                );
                assert!(!attempted);
                assert!(!target(config).exists());
            });
        }
    }

    #[test]
    fn committed_terminal_evidence_survives_worktree_loss() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let archive = crate::ensure_archive(config, &source.slug).unwrap();
            let path = target(config);
            crate::ensure_parent_dir(&path).unwrap();
            let mut terminal = source.artifact.clone();
            terminal["released_ts"] = json!("1970-01-01T00:00:05Z");
            let bytes = serde_json::to_vec_pretty(&terminal).unwrap();
            fs::write(&path, &bytes).unwrap();
            let relative = crate::rel_path_cached(&archive.canonical_repo_root, &path).unwrap();
            crate::commit_paths_with_retry(
                &archive.repo_root,
                config,
                "fixture: release",
                &[&relative],
            )
            .unwrap();
            let retained = config.storage_root.join("retained-terminal.json");
            fs::rename(&path, &retained).unwrap();
            let repo = Repository::open(&archive.repo_root).unwrap();
            let head = repo.head().unwrap().target().unwrap();
            let report = pass(cx, pool, config);
            assert_eq!((report.repaired, report.deferred), (0, 1));
            assert_eq!((report.deferred_transient, report.retry_len), (0, 0));
            assert!(!path.exists());
            assert_eq!(fs::read(retained).unwrap(), bytes);
            assert_eq!(repo.head().unwrap().target().unwrap(), head);
        });
    }

    #[test]
    fn create_only_publication_cannot_clobber_a_racing_terminal_writer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lease.json");
        let terminal = b"{\"released_ts\": 123}";
        // The writer arrives after the repair's earlier absence observation.
        assert!(!path.exists());
        fs::write(&path, terminal).unwrap();
        assert!(publish_missing(&path, b"{\"released_ts\": null}").is_err());
        assert_eq!(fs::read(path).unwrap(), terminal);
    }

    #[test]
    fn interrupted_publication_is_committed_but_existing_expiry_is_never_rewritten() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            crate::ensure_archive(config, &source.slug).unwrap();
            let path = target(config);
            let mut value = source.artifact.clone();
            value["operator_note"] = json!("retain exact surviving formatting");
            let bytes = serde_json::to_vec(&value).unwrap();
            publish_missing(&path, &bytes).unwrap();
            assert_eq!(pass(cx, pool, config).repaired, 1);
            assert_eq!(fs::read(&path).unwrap(), bytes);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(
                "UPDATE file_reservations SET expires_ts=expires_ts+1000000 WHERE id=401",
            )
            .unwrap();
            drop(conn);
            let report = pass(cx, pool, config);
            assert_eq!((report.repaired, report.deferred), (0, 1));
            assert_eq!(fs::read(path).unwrap(), bytes);
        });
    }

    #[test]
    fn bounded_round_progresses_past_conflicts_and_defers_new_arrivals_to_next_round() {
        fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            for id in 402..411 {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) SELECT {id}, project_id, agent_id, 'src/{id}.rs', \"exclusive\", reason, created_ts, expires_ts, NULL FROM file_reservations WHERE id=401")).unwrap();
            }
            drop(conn);
            let path = target(config);
            crate::ensure_parent_dir(&path).unwrap();
            fs::write(&path, b"malformed retained evidence").unwrap();
            let mut cursor = ActiveReservationCursor::default();
            let first = reconcile_active_reservations(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!((first.repaired, first.deferred), (WRITES_PER_PASS, 1));
            assert_eq!((first.deferred_transient, first.retry_len), (0, 0));
            assert!(first.more);
            let ceiling = cursor.ceiling;
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) SELECT 900, project_id, agent_id, 'new.rs', \"exclusive\", reason, created_ts, expires_ts, NULL FROM file_reservations WHERE id=401").unwrap();
            drop(conn);
            let second = reconcile_active_reservations(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(second.repaired, WRITES_PER_PASS);
            assert_eq!(cursor.ceiling, ceiling);
            assert!(cursor.after < 900);
            assert_eq!(fs::read(path).unwrap(), b"malformed retained evidence");
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_destination_is_preserved_and_does_not_redirect_repair() {
        fixture(|cx, pool, config| {
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            crate::ensure_archive(config, &source.slug).unwrap();
            let path = target(config);
            crate::ensure_parent_dir(&path).unwrap();
            let outside = config.storage_root.join("outside.json");
            fs::write(&outside, b"outside evidence").unwrap();
            std::os::unix::fs::symlink(&outside, &path).unwrap();
            let report = pass(cx, pool, config);
            assert_eq!((report.repaired, report.deferred), (0, 1));
            assert_eq!(fs::read_link(&path).unwrap(), outside);
            assert_eq!(fs::read(outside).unwrap(), b"outside evidence");
        });
    }

    #[test]
    fn shutdown_and_foreign_mailbox_binding_do_not_authorize_writes() {
        fixture(|cx, pool, config| {
            let mut cursor = ActiveReservationCursor::default();
            let report = reconcile_active_reservations(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(true),
            )
            .unwrap();
            assert!(report.interrupted);
            assert_eq!(report.scanned, 0);
            assert!(!target(config).exists());
            let other = config.storage_root.join("other.sqlite3");
            fs::write(&other, b"not this mailbox").unwrap();
            let foreign = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&other),
                ..config.clone()
            };
            assert!(
                reconcile_active_reservations(
                    cx,
                    pool,
                    &foreign,
                    &mut cursor,
                    &AtomicBool::new(false)
                )
                .is_err()
            );
            assert!(!target(config).exists());
            assert_eq!(fs::read(other).unwrap(), b"not this mailbox");
        });
    }

    fn assert_committed(cx: &Cx, pool: &DbPool, config: &Config, id: i64) {
        let source = read_source(cx, pool, id).unwrap().unwrap();
        let relative = format!(
            "projects/{}/file_reservations/{}",
            source.slug,
            reservation_artifact_filename(Some(&source.generation), id),
        );
        let path = config.storage_root.join(&relative);
        let artifact = read_artifact(&path).unwrap().unwrap();
        assert_eq!(artifact.value, source.artifact);
        let repo = Repository::open(&config.storage_root).unwrap();
        let tree = head_tree(&repo).unwrap().unwrap();
        assert_eq!(
            committed_artifact(&repo, Some(&tree), &relative)
                .unwrap()
                .unwrap()
                .bytes,
            artifact.bytes,
        );
        assert_eq!(read_source(cx, pool, id).unwrap().unwrap(), source);
    }

    #[test]
    fn promotion_defers_active_repair_without_consuming_the_round() {
        use mcp_agent_mail_db::write_barrier::{
            DrainOutcome, acquire_promotion_barrier_draining, active_writer_count,
            begin_write_activity, try_acquire_promotion_barrier_if_idle,
        };
        for parent_writer in [false, true] {
            fixture(|cx, pool, config| {
                let source = read_source(cx, pool, 401).unwrap().unwrap();
                assert_eq!(active_writer_count(), 0);
                let parent = parent_writer.then(begin_write_activity);
                let (ready_tx, ready_rx) = mpsc::channel();
                let (release_tx, release_rx) = mpsc::channel();
                let owner = std::thread::spawn(move || {
                    let gate = if parent_writer {
                        let (gate, outcome) = acquire_promotion_barrier_draining(Duration::ZERO);
                        assert!(matches!(
                            outcome,
                            DrainOutcome::TimedOut {
                                remaining_writers: 1
                            }
                        ));
                        gate
                    } else {
                        try_acquire_promotion_barrier_if_idle().expect("idle promotion")
                    };
                    ready_tx.send(()).unwrap();
                    let released = release_rx.recv_timeout(Duration::from_secs(10));
                    drop(gate);
                    released
                });
                ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let mut cursor = ActiveReservationCursor {
                    identity: "retained source".into(),
                    after: 17,
                    ceiling: Some(401),
                    ..Default::default()
                };
                let refused = reconcile_active_reservations(
                    cx,
                    pool,
                    config,
                    &mut cursor,
                    &AtomicBool::new(false),
                );
                let writers = active_writer_count();
                let initialized = config.storage_root.join("projects").exists();
                drop(parent);
                let _ = release_tx.send(());
                assert!(
                    owner.join().unwrap().is_ok(),
                    "repair waited for promotion expiry"
                );
                assert!(refused.unwrap_err().contains("admission contention"));
                assert_eq!(writers, usize::from(parent_writer));
                assert!(!initialized);
                assert_eq!(
                    (&*cursor.identity, cursor.after, cursor.ceiling),
                    ("retained source", 17, Some(401))
                );
                assert_eq!(read_source(cx, pool, 401).unwrap().unwrap(), source);
                let resumed = reconcile_active_reservations(
                    cx,
                    pool,
                    config,
                    &mut cursor,
                    &AtomicBool::new(false),
                )
                .unwrap();
                assert_eq!((resumed.repaired, resumed.deferred), (1, 0));
                assert_committed(cx, pool, config, 401);
                assert_eq!(active_writer_count(), 0);
            });
        }
    }

    #[test]
    fn busy_global_fence_does_not_initialize_an_archive_or_hold_database_admission() {
        use mcp_agent_mail_db::write_barrier::{
            active_writer_count, try_acquire_promotion_barrier_if_idle,
        };
        for existing_archive in [false, true] {
            fixture(|cx, pool, config| {
                if existing_archive {
                    crate::ensure_archive(config, "project").unwrap();
                }
                crate::flush_async_commits();
                let epoch_path = config
                    .storage_root
                    .join(".git")
                    .join(crate::ARCHIVE_EPOCH_FILE_NAME);
                let stamp = fs::read(&epoch_path).ok();
                let epoch = crate::archive_mutation_epoch();
                let source = read_source(cx, pool, 401).unwrap().unwrap();
                let (ready_tx, ready_rx) = mpsc::channel();
                let (release_tx, release_rx) = mpsc::channel();
                let owner = std::thread::spawn(move || {
                    crate::with_archive_snapshot_publication_fence(|| {
                        ready_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10))
                    })
                });
                ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let mut cursor = ActiveReservationCursor::default();
                let first = reconcile_active_reservations(
                    cx,
                    pool,
                    config,
                    &mut cursor,
                    &AtomicBool::new(false),
                );
                let writers = active_writer_count();
                let gate = try_acquire_promotion_barrier_if_idle();
                let can_promote = gate.is_some();
                drop(gate);
                let observations = (
                    target(config).exists(),
                    config.storage_root.join(".git").exists(),
                    fs::read(&epoch_path).ok(),
                    crate::archive_mutation_epoch(),
                    crate::archive_mutations_active(),
                );
                let _ = release_tx.send(());
                assert!(
                    owner.join().unwrap().is_ok(),
                    "repair waited for fence-owner expiry"
                );
                let first = first.unwrap();
                assert_eq!(
                    (
                        first.scanned,
                        first.deferred,
                        first.repaired,
                        first.attempted
                    ),
                    (1, 1, 0, 0)
                );
                assert_eq!((first.deferred_transient, first.retry_len), (1, 1));
                assert!(first.more);
                assert_eq!(observations, (false, existing_archive, stamp, epoch, 0));
                assert_eq!(writers, 0);
                assert!(can_promote);
                assert!(
                    cursor.ceiling.is_none(),
                    "the finite discovery round must finish independently of retry debt"
                );
                assert_eq!(read_source(cx, pool, 401).unwrap().unwrap(), source);
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                conn.execute_raw("UPDATE file_reservations SET reason='renewed while deferred', expires_ts=expires_ts+1000000 WHERE id=401").unwrap();
                drop(conn);
                let resumed = reconcile_active_reservations(
                    cx,
                    pool,
                    config,
                    &mut cursor,
                    &AtomicBool::new(false),
                )
                .unwrap();
                assert_eq!((resumed.repaired, resumed.deferred), (1, 0));
                assert_eq!(resumed.retry_len, 0);
                assert_committed(cx, pool, config, 401);
                assert_eq!(
                    read_artifact(&target(config)).unwrap().unwrap().value["reason"],
                    "renewed while deferred"
                );
                assert_eq!(active_writer_count(), 0);
            });
        }
    }

    #[test]
    fn busy_project_does_not_strand_other_leases_or_resurrect_a_later_release() {
        fixture(|cx, pool, config| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(72, 'other', '/other', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(82, 72, 'GreenStone', 'test', 'test', 1, 1)").unwrap();
            conn.execute_raw("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) SELECT 501, 72, 82, 'other.rs', 1, 'other lease', created_ts, expires_ts, NULL FROM file_reservations WHERE id=401").unwrap();
            drop(conn);
            let archive = crate::ensure_archive(config, "project").unwrap();
            crate::ensure_archive(config, "other").unwrap();
            crate::flush_async_commits();
            let process = crate::archive_process_lock(&archive).unwrap();
            let (ready_tx, ready_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let owner = std::thread::spawn(move || {
                let _held = process.lock().unwrap();
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10))
            });
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut cursor = ActiveReservationCursor::default();
            let first = reconcile_active_reservations(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            );
            let missing = !target(config).exists();
            let other = config
                .storage_root
                .join("projects/other/file_reservations/id-501-gaabb.json");
            let other_present = other.exists();
            let _ = release_tx.send(());
            assert!(
                owner.join().unwrap().is_ok(),
                "repair waited for project-owner expiry"
            );
            let first = first.unwrap();
            assert_eq!((first.scanned, first.deferred, first.repaired), (2, 1, 1));
            assert_eq!((first.deferred_transient, first.retry_len), (1, 1));
            assert!(missing && other_present);
            assert_committed(cx, pool, config, 501);
            // Release the previously active row while its repair is deferred.
            // The retry must consult both release stores, never a payload
            // captured before contention. Keep the same cursor, without reset.
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("INSERT INTO file_reservation_releases(reservation_id, released_ts) VALUES(401, 5000000)").unwrap();
            drop(conn);
            let resumed = reconcile_active_reservations(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(
                (resumed.repaired, resumed.unchanged, resumed.deferred),
                (0, 2, 0)
            );
            assert_eq!(
                resumed.retry_len, 0,
                "the released retry is settled, not replayed"
            );
            assert!(!target(config).exists());
            assert!(read_source(cx, pool, 401).unwrap().is_none());
            assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
        });
    }

    #[test]
    fn shutdown_during_project_contention_retains_the_candidate_for_resume() {
        fixture(|cx, pool, config| {
            let archive = crate::ensure_archive(config, "project").unwrap();
            crate::flush_async_commits();
            let source = read_source(cx, pool, 401).unwrap().unwrap();
            let process = crate::archive_process_lock(&archive).unwrap();
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let cancel = std::sync::Arc::clone(&stop);
            let (ready_tx, ready_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let owner = std::thread::spawn(move || {
                let _held = process.lock().unwrap();
                ready_tx.send(()).unwrap();
                // Observe the actual repair mutation window, not a timed guess
                // about when the worker will reach the held project mutex.
                let deadline = Instant::now() + Duration::from_secs(5);
                while crate::archive_mutations_active() == 0 && Instant::now() < deadline {
                    std::thread::yield_now();
                }
                let observed = crate::archive_mutations_active() > 0;
                cancel.store(true, Ordering::Release);
                let released = release_rx.recv_timeout(Duration::from_secs(10));
                (observed, released)
            });
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut cursor = ActiveReservationCursor::default();
            let interrupted = reconcile_active_reservations(cx, pool, config, &mut cursor, &stop);
            let absent = !target(config).exists();
            let _ = release_tx.send(());
            let (observed, released) = owner.join().unwrap();
            assert!(observed && released.is_ok());
            let interrupted = interrupted.unwrap();
            assert!(interrupted.interrupted && interrupted.more);
            assert_eq!(
                (interrupted.scanned, interrupted.attempted, cursor.after),
                (0, 0, 0)
            );
            assert!(absent);
            assert_eq!(read_source(cx, pool, 401).unwrap().unwrap(), source);
            assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
            let resumed = reconcile_active_reservations(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!((resumed.repaired, resumed.deferred), (1, 0));
            assert_committed(cx, pool, config, 401);
        });
    }

    #[test]
    fn contended_active_lease_is_retried_before_the_discovery_round_finishes() {
        fixture(|cx, pool, config| {
            let original = read_source(cx, pool, 401).unwrap().unwrap();
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            for id in 402..=480 {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) SELECT {id}, project_id, agent_id, 'src/{id}.rs', \"exclusive\", reason, created_ts, expires_ts, NULL FROM file_reservations WHERE id=401")).unwrap();
            }
            drop(conn);
            crate::ensure_archive(config, "project").unwrap();
            crate::flush_async_commits();
            let (ready_tx, ready_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let owner = std::thread::spawn(move || {
                crate::with_archive_snapshot_publication_fence(|| {
                    ready_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(10))
                })
            });
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut cursor = ActiveReservationCursor::default();
            let refused = reconcile_active_reservations(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            );
            let before = (cursor.after, cursor.ceiling, target(config).exists());
            let _ = release_tx.send(());
            assert!(
                owner.join().unwrap().is_ok(),
                "repair must not wait for the fence"
            );
            let refused = refused.unwrap();
            assert_eq!(
                (refused.scanned, refused.attempted, refused.repaired),
                (32, 0, 0)
            );
            assert_eq!((refused.deferred_transient, refused.retry_len), (32, 32));
            assert_eq!(before, (432, Some(480), false));

            let resumed = reconcile_active_reservations(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(resumed.repaired, WRITES_PER_PASS);
            assert_eq!(resumed.deferred, 0);
            assert!(resumed.scanned <= 32 && resumed.attempted <= WRITES_PER_PASS);
            assert_eq!(
                resumed.retry_len, 30,
                "two retry and two discovery repairs share the budget"
            );
            assert_eq!(cursor.ceiling, Some(480));
            assert!(
                cursor.after > 432 && cursor.after < 480,
                "discovery must also progress"
            );
            assert_committed(cx, pool, config, 401);
            assert_eq!(read_source(cx, pool, 401).unwrap().unwrap(), original);
            assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
        });
    }

    #[test]
    fn source_rebinding_discards_old_retry_debt_before_reading_new_ids() {
        fixture(|cx, pool, config| {
            let mut cursor = ActiveReservationCursor {
                identity: "retired mailbox generation".into(),
                after: 9_000,
                ceiling: Some(10_000),
                retry_first: true,
                ..Default::default()
            };
            assert!(cursor.retries.defer(9_001, Instant::now()));
            let report = reconcile_active_reservations(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(
                (report.scanned, report.repaired, report.deferred),
                (1, 1, 0)
            );
            assert_eq!(report.retry_len, 0);
            assert_eq!(cursor.identity, pool.sqlite_identity_key());
            assert_committed(cx, pool, config, 401);
        });
    }

    #[test]
    fn retry_classification_does_not_turn_conflicts_or_permissions_into_contention() {
        for kind in [
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::AlreadyExists,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::ResourceBusy,
            std::io::ErrorKind::StorageFull,
            std::io::ErrorKind::QuotaExceeded,
            std::io::ErrorKind::OutOfMemory,
        ] {
            let failure =
                RepairFailure::from(crate::StorageError::Io(std::io::Error::new(kind, "test")));
            assert!(failure.transient, "{kind:?}");
        }
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::InvalidInput,
            std::io::ErrorKind::NotFound,
        ] {
            let failure = RepairFailure::from(crate::StorageError::Io(std::io::Error::new(
                kind,
                "storage full or database locked is only text",
            )));
            assert!(!failure.transient, "{kind:?}");
        }
        assert!(!RepairFailure::from(invalid("terminal evidence must be preserved")).transient);
        assert!(!RepairFailure::from("malformed source identity").transient);
        assert!(
            RepairFailure::from(crate::StorageError::LockContention {
                message: "publication fence busy".to_string(),
            })
            .transient
        );
    }

    #[test]
    fn active_progress_round_trips_and_keeps_the_latest_repair_across_owners() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_progress(root.path()), None);
        let repaired = ActiveReservationProgress {
            pid: 1,
            pass_us: 100,
            last_repair_us: Some(90),
        };
        record_progress(root.path(), &repaired).unwrap();
        assert_eq!(read_progress(root.path()), Some(repaired));

        let idle_other_owner = ActiveReservationProgress {
            pid: 2,
            pass_us: 200,
            last_repair_us: None,
        };
        record_progress(root.path(), &idle_other_owner).unwrap();
        assert_eq!(
            read_progress(root.path()),
            Some(ActiveReservationProgress {
                pid: 2,
                pass_us: 200,
                last_repair_us: Some(90),
            })
        );

        let stepped_back_clock = ActiveReservationProgress {
            pid: 3,
            pass_us: 10,
            last_repair_us: None,
        };
        record_progress(root.path(), &stepped_back_clock).unwrap();
        assert_eq!(
            read_progress(root.path()),
            Some(ActiveReservationProgress {
                pid: 3,
                pass_us: 10,
                last_repair_us: Some(90),
            })
        );

        let far_future = ActiveReservationProgress {
            pid: 4,
            pass_us: 10 + PROGRESS_CLOCK_SKEW_US + 1_000,
            last_repair_us: Some(10 + PROGRESS_CLOCK_SKEW_US + 1_000),
        };
        record_progress(root.path(), &far_future).unwrap();
        record_progress(root.path(), &stepped_back_clock).unwrap();
        assert_eq!(read_progress(root.path()), Some(stepped_back_clock));
    }
}
