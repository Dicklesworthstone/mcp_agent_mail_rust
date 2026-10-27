//! Bounded live-DB to archive convergence for the maintenance worker.
//!
//! Repair never substitutes an archive snapshot for the live source or modifies
//! mailbox rows. Separately enabled retention deletes settled rows only after
//! verifying their complete archive and rechecking the live source. Both paths
//! require the server's writable pool for the same configured mailbox/root.

#[cfg(test)]
mod admission_tests;
mod checkpoint;
pub(crate) mod durability;
#[cfg(test)]
mod pressure_tests;
mod source;
mod staged;

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use asupersync::{Cx, Outcome};
use fastmcp_core::block_on;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::queries::{
    MessagePruneReport, capture_message_prune_candidate, list_prunable_message_ids,
    prune_verified_settled_messages,
};
use mcp_agent_mail_db::{DbError, DbPool, corruption_circuit_breaker};
use serde::Serialize;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

#[cfg(test)]
use self::source::prepare_message;
use self::source::prepare_message_classified;
use super::{ReconcileResult, RepairControl, read_surviving_message, reconcile_message_bundle};
use crate::{MessageBundleBatchEntry, ProjectArchive};

const IDS_PER_LANE: i64 = 16;
const MAX_REPAIRS_PER_BATCH: usize = 4;
const MAX_DB_PAYLOAD_BYTES: i64 = 4 * 1024 * 1024;
const MAX_BATCH_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const NORMAL_ARCHIVE_GRACE_US: i64 = 30 * 1_000_000;
const MAX_ARCHIVE_LAG_GRACE_US: u64 = 10 * 60 * 1_000_000;
const MAX_RETRY_IDS: usize = 1024;
const MAX_RETRIES_PER_PASS: usize = 16;
const MAX_IDS_PER_PASS: usize = 32;
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct DeferredRetry {
    id: i64,
    attempts: u32,
    ready_at: Instant,
}

#[derive(Debug)]
struct StartupTail {
    after: i64,
    ceiling: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanLane {
    Tail,
    History,
    StartupTail,
}

#[derive(Debug)]
struct ReconcileFailure {
    detail: String,
    transient: bool,
}

impl ReconcileFailure {
    fn source_unavailable(error: impl std::fmt::Display) -> Self {
        // A failed source read is not conflicting evidence. Keep its ID for
        // retry; the corruption observer still stops all repair when needed.
        Self {
            detail: source_error(error),
            transient: true,
        }
    }
}

impl From<String> for ReconcileFailure {
    fn from(detail: String) -> Self {
        Self {
            detail,
            transient: false,
        }
    }
}

impl From<crate::StorageError> for ReconcileFailure {
    fn from(error: crate::StorageError) -> Self {
        use std::io::ErrorKind;

        // Resource pressure cannot contradict message identity. Keep its ID
        // in the ordinary bounded retry lane instead of discarding retry debt
        // behind the history cursor. Do not infer retryability from error text
        // or broaden cancellation, permission, path, or opaque Git failures.
        let transient = matches!(
            error,
            crate::StorageError::LockContention { .. }
                | crate::StorageError::LockTimeout(_)
                | crate::StorageError::GitIndexLock { .. }
        ) || matches!(&error, crate::StorageError::Io(error)
            if matches!(error.kind(),
                ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::ResourceBusy
                    | ErrorKind::StorageFull | ErrorKind::QuotaExceeded | ErrorKind::OutOfMemory));
        Self {
            detail: error.to_string(),
            transient,
        }
    }
}

/// Separate new-message and rotating backfill cursors prevent old broken
/// records from pinning catch-up and continuous new mail from starving history.
/// Cursors are only hints: restarting or wrapping always safely checks again.
/// The startup-aware worker cursor checkpoints its hints; `Default` remains
/// in-memory for independent one-shot scans and callers without a startup anchor.
#[derive(Debug, Default)]
pub struct ReconcileCursor {
    source_identity: String,
    tail_after: Option<i64>,
    backfill_ceiling: Option<i64>,
    // A byte budget can admit only one message. Resume with the opposite lane
    // after the last consumed item, not unconditionally with new-mail catch-up.
    next_lane_is_history: bool,
    // Retry payloads share the same byte budget as new/history work. Preserve
    // the opposite lane's turn even when one admitted item fills a whole pass.
    next_lane_is_retry: bool,
    // Rows created before this instant (µs) predate the reconciling process's
    // own write-behind queue, so they cannot still be in flight there and need
    // no grace period. 0 = always apply the grace.
    settled_before_us: i64,
    // Finish the current descending history round at catch-up cadence after
    // worker startup, even when its first pages are already archived. This is
    // process-local: checkpoint restore keeps its scan position and retry debt,
    // but cannot claim that this worker has already completed that round.
    startup_backfill_complete: bool,
    // A restored forward cursor may lag far below the crash window while its
    // history cursor is already below it. Give that process's newest settled
    // rows a finite descending lane without throwing away either checkpoint.
    // These are process-local hints; every restart captures its own upper bound.
    startup_tail: Option<StartupTail>,
    startup_tail_initialized: bool,
    next_lane_is_startup: bool,
    // Only IDs are retained. Every retry rereads the current live source and
    // repeats the ordinary authority/no-clobber checks before publication.
    retries: VecDeque<DeferredRetry>,
    conflict_backoff: VecDeque<(i64, Instant)>,
    persist_progress: bool,
    // A failed checkpoint must not replace newer in-memory retry debt with
    // an older disk snapshot on the next pass. A successful save clears this.
    checkpoint_dirty: bool,
}

impl ReconcileCursor {
    /// Cursor for a reconciler whose process accepted no writes before
    /// `settled_before_us`. After a crash, the mail that the dead process
    /// committed but never archived is then eligible on the first pass instead
    /// of waiting behind a 16-ids-per-pass walk of healthy recent mail.
    ///
    /// Scan positions and retry debt are checkpointed together beside the live
    /// database. Restoring them never restores the old process's grace anchor
    /// or grants authority to repair a message without rereading its source.
    /// Until the current history round reaches its end, consumed pages request
    /// the worker's bounded catch-up cadence, not its once-per-minute idle poll.
    #[must_use]
    pub fn settled_before(settled_before_us: i64) -> Self {
        Self {
            settled_before_us,
            persist_progress: true,
            ..Self::default()
        }
    }

    fn advance(&mut self, id: i64, tail: bool) {
        if tail {
            self.tail_after = Some(self.tail_after.unwrap_or(0).max(id));
        } else {
            self.backfill_ceiling = Some(id.saturating_sub(1));
        }
        self.next_lane_is_history = tail;
    }

    fn advance_scan(&mut self, id: i64, lane: ScanLane) {
        match lane {
            ScanLane::Tail => self.advance(id, true),
            ScanLane::History => self.advance(id, false),
            ScanLane::StartupTail => {
                if let Some(range) = &mut self.startup_tail {
                    range.ceiling = id.saturating_sub(1);
                    if range.ceiling <= range.after {
                        self.startup_tail = None;
                    }
                }
            }
        }
        self.next_lane_is_startup = lane != ScanLane::StartupTail;
    }

    fn forget_retry(&mut self, id: i64) {
        self.retries.retain(|retry| retry.id != id);
    }

    fn defer_transient(&mut self, id: i64, now: Instant) {
        let previous = self
            .retries
            .iter()
            .position(|retry| retry.id == id)
            .and_then(|index| self.retries.remove(index));
        let attempts = previous.map_or(0, |retry| retry.attempts.saturating_add(1));
        // The first deferral is eligible in the very next pass. Repeated
        // contention backs off without exceeding the normal worker cadence.
        let backoff = if attempts == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(1_u64 << attempts.saturating_sub(1).min(6)).min(MAX_RETRY_BACKOFF)
        };
        self.retries.push_back(DeferredRetry {
            id,
            attempts,
            ready_at: now + backoff,
        });
    }

    fn defer_conflict(&mut self, id: i64, now: Instant) {
        self.forget_retry(id);
        self.conflict_backoff
            .retain(|(known, until)| *known != id && *until > now);
        if self.conflict_backoff.len() == MAX_RETRY_IDS {
            self.conflict_backoff.pop_front();
        }
        self.conflict_backoff
            .push_back((id, now + MAX_RETRY_BACKOFF));
    }
}

/// Counts describe this bounded pass, not whole-mailbox durability.
#[derive(Debug, Default, Serialize)]
pub struct ReconcileReport {
    pub scanned: usize,
    pub unchanged: usize,
    pub repaired: usize,
    pub files_created: usize,
    pub deferred: usize,
    pub deferred_transient: usize,
    pub deferred_conflict: usize,
    pub retry_len: usize,
    /// Serialized payload bytes admitted to archive work, excluding rejected
    /// oversized projections (whose SQL input has a separate byte bound).
    pub payload_bytes: usize,
    pub interrupted: bool,
    /// More bounded work should follow at catch-up cadence: an execution limit
    /// was reached, or a consumed startup history page has not yet proved the
    /// end of its round, or a consumed forward page has an eligible successor.
    /// This is not a count of missing archive messages.
    pub budget_exhausted: bool,
    /// Scheduling hints were restored, not archive authority or read/ack state.
    pub checkpoint_restored: bool,
    /// `None` for an in-memory or pre-cancelled pass; false means progress is
    /// currently memory-only and a restart may replay an older bounded pass.
    pub checkpoint_saved: Option<bool>,
}

/// Record only a completed failure after cancellation and capacity admission.
/// Resource retries retain IDs, never the failed payload or archive authority.
fn record_failure(
    id: i64,
    error: ReconcileFailure,
    cursor: &mut ReconcileCursor,
    report: &mut ReconcileReport,
    now: Instant,
) {
    report.deferred += 1;
    if error.transient {
        report.deferred_transient += 1;
        cursor.defer_transient(id, now);
    } else {
        report.deferred_conflict += 1;
        cursor.defer_conflict(id, now);
    }
    tracing::warn!(
        target: "maintenance", event = "message_archive_reconcile_deferred",
        message_id = id, reason = %error.detail, transient = error.transient,
        "message archive repair deferred; source and conflicting evidence retained"
    );
}

/// A finite retention round revisits unavailable archive evidence without
/// allowing one broken old message or continuous imports to pin progress.
#[derive(Debug, Default)]
pub struct ArchivePruneCursor {
    source_identity: String,
    after: i64,
    ceiling: Option<i64>,
}

/// Archive verification is separate from age/read/ack eligibility. A deferred
/// message remains in SQLite even when it is otherwise old enough to prune.
#[derive(Debug, Default)]
pub struct ArchivePruneReport {
    pub scanned: usize,
    pub deferred: usize,
    pub payload_bytes: usize,
    /// Unique bundle content admitted, including failed verification attempts.
    /// Git validation and disk comparison can read each artifact more than once.
    pub archive_bytes: usize,
    pub interrupted: bool,
    pub budget_exhausted: bool,
    pub pruned: MessagePruneReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayloadAdmission {
    Fits,
    NextBatch,
    Oversized,
}

fn payload_admission(used: usize, candidate: usize) -> PayloadAdmission {
    if candidate > MAX_BATCH_PAYLOAD_BYTES {
        PayloadAdmission::Oversized
    } else if candidate > MAX_BATCH_PAYLOAD_BYTES.saturating_sub(used) {
        PayloadAdmission::NextBatch
    } else {
        PayloadAdmission::Fits
    }
}

/// Default-on non-destructive repair for file-backed mailboxes, independently
/// switchable from destructive retention. Invalid overrides fail disabled.
#[must_use]
pub fn enabled(config: &Config) -> bool {
    mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url).is_some()
        && parse_enabled(
            mcp_agent_mail_core::config::process_env_value("AM_MESSAGE_ARCHIVE_RECONCILE_ENABLED")
                .as_deref(),
        )
}

fn parse_enabled(raw: Option<&str>) -> bool {
    raw.is_none_or(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn outcome<T, E: std::fmt::Display>(value: Outcome<T, E>) -> Result<T, String> {
    match value {
        Outcome::Ok(value) => Ok(value),
        Outcome::Err(error) => Err(source_error(error)),
        Outcome::Cancelled(_) => Err("message reconciliation source read cancelled".to_string()),
        Outcome::Panicked(_) => Err("message reconciliation source read panicked".to_string()),
    }
}

fn source_error(error: impl std::fmt::Display) -> String {
    let error = DbError::Sqlite(error.to_string());
    corruption_circuit_breaker().observe_error(&error);
    format!("message reconciliation source query failed: {error}")
}

fn validate_pool_binding(pool: &DbPool, config: &Config) -> Result<(), String> {
    let selected =
        mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&config.database_url)
            .ok_or_else(|| "message reconciliation requires a file-backed source".to_string())?;
    let selected = std::fs::canonicalize(selected).map_err(|error| error.to_string())?;
    let source = std::fs::canonicalize(pool.sqlite_path()).map_err(|error| error.to_string())?;
    if source != selected {
        return Err("message reconciliation pool is not the configured live database".to_string());
    }
    let pool_root =
        std::fs::canonicalize(pool.storage_root()).map_err(|error| error.to_string())?;
    let configured_root =
        std::fs::canonicalize(&config.storage_root).map_err(|error| error.to_string())?;
    if pool_root != configured_root {
        return Err("message reconciliation pool and archive roots do not match".to_string());
    }
    Ok(())
}

fn interleave_ids(tail: &[i64], history: &[i64], history_first: bool) -> Vec<(i64, bool)> {
    let mut selected = Vec::with_capacity(tail.len() + history.len());
    let lanes = if history_first {
        [(history, false), (tail, true)]
    } else {
        [(tail, true), (history, false)]
    };
    for index in 0..tail.len().max(history.len()) {
        for (ids, is_tail) in lanes {
            if let Some(id) = ids.get(index) {
                // Keep duplicate IDs: the caller processes the payload once but
                // advances both independent cursors when it consumes each lane.
                selected.push((*id, is_tail));
            }
        }
    }
    selected
}

fn interleave_startup_ids(
    normal: &[(i64, bool)],
    startup: &[i64],
    startup_first: bool,
) -> Vec<(i64, ScanLane)> {
    let mut selected = Vec::with_capacity(normal.len() + startup.len());
    for index in 0..normal.len().max(startup.len()) {
        let ordinary = normal.get(index).map(|&(id, tail)| {
            (
                id,
                if tail {
                    ScanLane::Tail
                } else {
                    ScanLane::History
                },
            )
        });
        let recent = startup.get(index).map(|&id| (id, ScanLane::StartupTail));
        selected.extend(if startup_first {
            [recent, ordinary].into_iter().flatten()
        } else {
            [ordinary, recent].into_iter().flatten()
        });
    }
    selected
}

fn normal_archive_cutoff(now_us: i64, settled_before_us: i64, archive_lag_us: u64) -> i64 {
    // A fixed grace mistakes still-queued mail for lost mail when WBQ falls
    // behind. Its repair then competes for the publication fence with the
    // very drain and reservation writers that need to make progress (br-9ott5).
    // Lag is a process-wide scheduling hint, never proof that a particular
    // message is queued. Bound its effect so unrelated or stalled archive
    // work cannot hide a genuinely lost message indefinitely.
    let lag_grace_us =
        i64::try_from(archive_lag_us.min(MAX_ARCHIVE_LAG_GRACE_US)).unwrap_or(i64::MAX);
    now_us
        .saturating_sub(NORMAL_ARCHIVE_GRACE_US)
        .saturating_sub(lag_grace_us)
        // A restarted worker cannot own the prior process's queued mail.
        // Known failures also bypass this cutoff through the retry lane.
        .max(settled_before_us)
}

struct SelectedIds {
    ids: Vec<(i64, ScanLane)>,
    tail_started_after: Option<i64>,
    // This one-ID lookahead is never admitted to payload or publication work.
    // It witnesses remaining eligible forward work after a consumed page.
    tail_next_id: Option<i64>,
}

fn select_ids(
    cx: &Cx,
    pool: &DbPool,
    cursor: &mut ReconcileCursor,
    cutoff: i64,
) -> Result<SelectedIds, String> {
    let identity = pool.sqlite_identity_key();
    if identity != cursor.source_identity {
        *cursor = ReconcileCursor {
            source_identity: identity,
            settled_before_us: cursor.settled_before_us,
            persist_progress: cursor.persist_progress,
            ..Default::default()
        };
    }
    let conn = outcome(block_on(pool.acquire(cx)))?;
    let mode = conn
        .query_sync("PRAGMA query_only", &[])
        .map_err(source_error)?;
    let query_only = mode
        .first()
        .ok_or_else(|| "source query-only mode was not reported".to_string())?
        .get_as::<i64>(0)
        .map_err(source_error)?;
    if query_only != 0 {
        return Err("query-only snapshots cannot authorize message archive repair".to_string());
    }
    let rows = conn
        .query_sync("SELECT COALESCE(MAX(id), 0) AS max_id FROM messages", &[])
        .map_err(source_error)?;
    let max_id = rows
        .first()
        .ok_or_else(|| "message ID aggregate returned no row".to_string())?
        .get_named::<i64>("max_id")
        .map_err(source_error)?;
    if cursor.tail_after.is_some_and(|after| after > max_id) {
        cursor.tail_after = None;
        cursor.backfill_ceiling = None;
        cursor.next_lane_is_history = false;
        cursor.startup_backfill_complete = false;
        cursor.startup_tail = None;
        cursor.startup_tail_initialized = false;
    }
    if !cursor.startup_tail_initialized {
        if cursor.persist_progress
            && cursor.settled_before_us > 0
            && let Some(after) = cursor.tail_after
        {
            let rows = conn
                .query_sync(
                    "SELECT id FROM messages WHERE id > ? AND created_ts <= ? ORDER BY id DESC LIMIT 1",
                    &[after.into(), cursor.settled_before_us.into()],
                )
                .map_err(source_error)?;
            cursor.startup_tail = rows
                .first()
                .map(|row| row.get_named::<i64>("id").map_err(source_error))
                .transpose()?
                .map(|ceiling| StartupTail { after, ceiling });
        }
        cursor.startup_tail_initialized = true;
        cursor.next_lane_is_startup = cursor.startup_tail.is_some();
    }
    let tail_started_after = cursor.tail_after;
    let mut tail = if let Some(after) = tail_started_after {
        conn.query_sync(
            "SELECT id FROM messages WHERE id > ? AND created_ts <= ? ORDER BY id ASC LIMIT ?",
            &[after.into(), cutoff.into(), (IDS_PER_LANE + 1).into()],
        )
    } else {
        conn.query_sync(
            "SELECT id FROM messages WHERE created_ts <= ? ORDER BY id DESC LIMIT ?",
            &[cutoff.into(), IDS_PER_LANE.into()],
        )
    }
    .map_err(source_error)?
    .into_iter()
    .map(|row| row.get_named::<i64>("id").map_err(source_error))
    .collect::<Result<Vec<_>, _>>()?;
    tail.sort_unstable();
    let tail_next_id = if tail_started_after.is_some()
        && i64::try_from(tail.len()).is_ok_and(|len| len > IDS_PER_LANE)
    {
        tail.pop()
    } else {
        None
    };
    let history = conn
        .query_sync(
            "SELECT id FROM messages WHERE id > 0 AND id <= ? AND created_ts <= ? ORDER BY id DESC LIMIT ?",
            &[
                cursor.backfill_ceiling.unwrap_or(max_id).into(),
                cutoff.into(),
                IDS_PER_LANE.into(),
            ],
        )
        .map_err(source_error)?
        .into_iter()
        .map(|row| row.get_named::<i64>("id").map_err(source_error))
        .collect::<Result<Vec<_>, _>>()?;
    if history.is_empty() {
        cursor.backfill_ceiling = None;
        // The descending round is finite even while new IDs keep arriving.
        // Keep its next ordinary wrap, but stop startup-only acceleration.
        cursor.startup_backfill_complete = true;
    }
    let startup = if let Some(range) = &cursor.startup_tail {
        conn.query_sync(
            "SELECT id FROM messages WHERE id > ? AND id <= ? AND created_ts <= ? ORDER BY id DESC LIMIT ?",
            &[
                range.after.into(),
                range.ceiling.into(),
                cursor.settled_before_us.into(),
                IDS_PER_LANE.into(),
            ],
        )
        .map_err(source_error)?
        .into_iter()
        .map(|row| row.get_named::<i64>("id").map_err(source_error))
        .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    if startup.is_empty() {
        cursor.startup_tail = None;
    }
    // Interleave within a pass AND retain lane priority across budget-limited
    // passes. Fixed tail-first ordering starves history at one payload per pass.
    // Startup inspection never advances tail_after: skipped young timestamps
    // and intervening old work still belong to the ordinary durable scan.
    Ok(SelectedIds {
        ids: interleave_startup_ids(
            &interleave_ids(&tail, &history, cursor.next_lane_is_history),
            &startup,
            cursor.next_lane_is_startup,
        ),
        tail_started_after,
        tail_next_id,
    })
}

struct PreparedMessage {
    message: Value,
    body: String,
    sender: String,
    project_slug: String,
    recipients: Vec<String>,
    payload_bytes: usize,
    // NULL legacy metadata cannot distinguish a reply from a threaded send.
    // Fresh writes persist either the exact parent or authoritative absence.
    archive_metadata_known: bool,
}

fn validate_surviving_message(
    expected: &PreparedMessage,
    observed: &Value,
    body: &str,
) -> Result<(), String> {
    if body != expected.body {
        return Err(
            "surviving archive body conflicts with the live message; preserved".to_string(),
        );
    }
    for key in [
        "id",
        "from",
        "subject",
        "project",
        "project_slug",
        "importance",
        "ack_required",
        "attachments",
    ] {
        if observed.get(key) != expected.message.get(key) {
            return Err(format!(
                "surviving archive {key} conflicts with live message; preserved"
            ));
        }
    }
    for key in ["topic", "thread_id"] {
        if observed.get(key).unwrap_or(&Value::Null) != &expected.message[key] {
            return Err(format!(
                "surviving archive {key} conflicts with live message; preserved"
            ));
        }
    }
    let timestamp = |value: &Value| {
        value
            .get("created")
            .and_then(Value::as_str)
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.timestamp_micros())
    };
    if timestamp(observed).is_none() || timestamp(observed) != timestamp(&expected.message) {
        return Err(
            "surviving archive creation timestamp conflicts with live message; preserved"
                .to_string(),
        );
    }
    for kind in ["to", "cc", "bcc"] {
        let names = |value: &Value| -> Option<Vec<String>> {
            let mut names = value
                .get(kind)?
                .as_array()?
                .iter()
                .map(|name| name.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?;
            names.sort_unstable();
            Some(names)
        };
        if names(observed).is_none() || names(observed) != names(&expected.message) {
            return Err(format!(
                "surviving archive {kind} routing conflicts with live message; preserved"
            ));
        }
    }
    if let Some(parent) = observed.get("reply_to")
        && parent
            .as_i64()
            .is_none_or(|parent| parent <= 0 || Some(parent) == observed["id"].as_i64())
    {
        return Err("surviving archive reply parent is invalid; preserved".to_string());
    }
    if expected.archive_metadata_known
        && observed.get("reply_to") != expected.message.get("reply_to")
    {
        return Err(
            "surviving archive reply parent conflicts with durable message metadata; preserved"
                .to_string(),
        );
    }
    Ok(())
}

/// Inbox copies retain reply/extension metadata but deliberately redact BCC.
/// Restore only that redacted field from the live DB, then apply every normal
/// identity/body/routing check. Never trust an inbox to supply private routing.
fn restore_inbox_metadata(
    prepared: &PreparedMessage,
    mut message: Value,
    body: &str,
) -> Result<Value, String> {
    if !message
        .get("bcc")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        return Err("surviving inbox has invalid BCC redaction; preserved".to_string());
    }
    message["bcc"] = prepared.message["bcc"].clone();
    validate_surviving_message(prepared, &message, body)?;
    Ok(message)
}

fn merge_surviving_metadata(
    surviving: &mut Option<Value>,
    message: Value,
    disagreement: &str,
) -> Result<(), String> {
    if let Some(previous) = surviving.as_ref() {
        if previous != &message {
            return Err(disagreement.to_string());
        }
    } else {
        *surviving = Some(message);
    }
    Ok(())
}

#[cfg(test)]
fn reconcile_prepared(
    config: &Config,
    prepared: &PreparedMessage,
) -> Result<ReconcileResult, String> {
    reconcile_prepared_inner(config, prepared, None, false).map_err(|error| error.detail)
}

fn reconcile_prepared_inner(
    config: &Config,
    prepared: &PreparedMessage,
    control: Option<RepairControl<'_>>,
    require_guarded_proof: bool,
) -> Result<ReconcileResult, ReconcileFailure> {
    if !require_guarded_proof && verify_prepared_without_mutation(config, prepared, control)? {
        return Ok(ReconcileResult::default());
    }
    // Admit before ensure_archive: even creating a missing archive must not
    // wait behind an unrelated publisher while retaining the DB source lease.
    // Keep mutable metadata collection in one native mutation window. Bundle
    // repair revalidates the captured bytes before publishing, so release this
    // window before its immutable Git-history proof: scanning old messages must
    // not exclude unrelated live archive writers.
    let metadata_mutation = control
        .map(|control| control.begin_at(&config.storage_root))
        .transpose()?;
    let archive = crate::ensure_archive(config, &prepared.project_slug)?;
    let paths = crate::message_paths_for_bundle(
        &archive,
        &prepared.message,
        &prepared.sender,
        &prepared.recipients,
    )?
    .0;
    let mut surviving = None;
    for path in [&paths.canonical, &paths.outbox] {
        super::checkpoint(control)?;
        if let Some((message, body)) = read_surviving_message(path)? {
            validate_surviving_message(prepared, &message, &body)?;
            merge_surviving_metadata(
                &mut surviving,
                message,
                "canonical and outbox metadata disagree; both preserved",
            )?;
        }
    }
    // Open Git lazily, once. Every fallback candidate comes from one pinned
    // tree even if an archive writer advances HEAD while we inspect copies.
    let committed = if surviving.is_none() {
        Some(CommittedMessages::open(&archive)?)
    } else {
        None
    };
    if let Some(committed) = &committed {
        for path in [&paths.canonical, &paths.outbox] {
            super::checkpoint(control)?;
            if let Some((message, body)) = committed.read(&archive, path)? {
                validate_surviving_message(prepared, &message, &body)?;
                merge_surviving_metadata(
                    &mut surviving,
                    message,
                    "committed canonical and outbox metadata disagree; both preserved",
                )?;
            }
        }
    }
    // A crash can remove both full copies before Git commits them while
    // leaving an inbox intact. It still carries the exact reply parent and
    // extension fields; BCC comes exclusively from the authoritative DB row.
    if surviving.is_none() {
        for path in &paths.inbox {
            super::checkpoint(control)?;
            if let Some((message, body)) = read_surviving_message(path)? {
                let message = restore_inbox_metadata(prepared, message, &body)?;
                merge_surviving_metadata(
                    &mut surviving,
                    message,
                    "surviving inbox metadata disagree; all copies preserved",
                )?;
            }
        }
    }
    // The working tree can lose every copy while Git still retains a redacted
    // inbox blob. It is just as useful as an on-disk inbox for recovering reply
    // metadata. Validate all committed inbox candidates, not only the first.
    if surviving.is_none()
        && let Some(committed) = &committed
    {
        for path in &paths.inbox {
            super::checkpoint(control)?;
            if let Some((message, body)) = committed.read(&archive, path)? {
                let message = restore_inbox_metadata(prepared, message, &body)?;
                merge_surviving_metadata(
                    &mut surviving,
                    message,
                    "committed inbox metadata disagree; all copies preserved",
                )?;
            }
        }
    }
    // Staging writes the exact blob before Git commits it. If every disk and
    // HEAD copy was lost, a bounded index snapshot can still retain reply and
    // extension metadata. It must agree with the live DB and with every other
    // staged copy; it never overrides unavailable/conflicting stronger evidence.
    if surviving.is_none()
        && let Some(committed) = &committed
    {
        super::checkpoint(control)?;
        surviving = staged::read_metadata(
            &committed.repo,
            &archive,
            prepared,
            &paths.canonical,
            &paths.outbox,
            &paths.inbox,
        )?;
    }
    let message = match surviving {
        Some(message) => message,
        None if prepared.archive_metadata_known || prepared.message["thread_id"].is_null() => {
            prepared.message.clone()
        }
        None => {
            // Legacy SQLite rows store only the thread. A fabricated parent
            // can collide with a delayed original WBQ write.
            return Err("threaded message has no surviving authoritative bundle; reply metadata cannot be inferred".to_string().into());
        }
    };
    super::checkpoint(control)?;
    let entry = MessageBundleBatchEntry {
        message: &message,
        body_md: &prepared.body,
        sender: &prepared.sender,
        recipients: &prepared.recipients,
        extra_paths: &[],
    };
    drop(metadata_mutation);
    match control {
        Some(control) => super::reconcile_message_bundle_cancellable(
            control.cx,
            &archive,
            config,
            entry,
            control.stop,
        ),
        None => reconcile_message_bundle(&archive, config, entry),
    }
    .map_err(ReconcileFailure::from)
}

/// Observe an already complete, committed bundle without taking the global
/// publication fence. This can only report unchanged: every missing, damaged,
/// uncommitted, or otherwise unproven bundle still uses guarded repair.
///
/// Equal stamps detect local mutation windows and observed remote epoch/HEAD
/// changes. A remote mutation window can span both samples, so this is not a
/// publication or deletion authorization. A stale unchanged observation merely
/// leaves repair for a later sweep, as can a write immediately after a fenced
/// observation. Keep all reads pinned to one HEAD and retain source revalidation.
fn verify_prepared_without_mutation(
    config: &Config,
    prepared: &PreparedMessage,
    control: Option<RepairControl<'_>>,
) -> Result<bool, ReconcileFailure> {
    super::checkpoint(control)?;
    let Some(archive) = crate::open_archive(config, &prepared.project_slug)? else {
        return Ok(false);
    };
    let Some(before) = crate::archive_epoch_read_stamp(&archive.repo_root)? else {
        return Ok(false);
    };
    let changed = || crate::StorageError::LockContention {
        message: "archive changed during read-only message verification; retry required".into(),
    };
    let committed = CommittedMessages::open(&archive)?;
    if before.generation.head != committed.head_id {
        return Err(changed().into());
    }
    if !matches!(
        verify_retention_archive_at(&archive, prepared, &mut 0, &committed, control),
        Ok(Some(_))
    ) {
        super::checkpoint(control)?;
        return Ok(false);
    }
    // A staged canonical collision remains conflicting evidence even when
    // every expected disk/HEAD copy is intact. Reuse the same bounded identity
    // gate as publication, bracketed by the pinned HEAD checks because its
    // own preflight opens HEAD independently.
    if !committed.same_head(&archive)? {
        return Err(changed().into());
    }
    let paths = crate::message_paths_for_bundle(
        &archive,
        &prepared.message,
        &prepared.sender,
        &prepared.recipients,
    )?
    .0;
    let canonical = crate::rel_path_cached(&archive.canonical_repo_root, &paths.canonical)?;
    let expected = committed.file_identity(&archive, &paths.canonical)?.0;
    let mut budget = super::CanonicalScanBudget {
        control,
        ..Default::default()
    };
    if !super::identity::preflight(
        &committed.repo,
        &archive,
        prepared.message["id"]
            .as_i64()
            .filter(|id| *id > 0)
            .ok_or_else(|| super::invalid("message has no positive archive identity"))?,
        &canonical,
        expected,
        false,
        &mut budget,
    )? {
        return Ok(false);
    }
    super::checkpoint(control)?;
    if !committed.same_head(&archive)?
        || crate::archive_epoch_read_stamp(&archive.repo_root)?.as_ref() != Some(&before)
    {
        return Err(changed().into());
    }
    Ok(true)
}

/// Immutable Git observation for all surviving copies of one message.
/// Holding the tree identity avoids mixing metadata from separate HEADs.
struct CommittedMessages {
    repo: git2::Repository,
    head_id: Option<git2::Oid>,
    tree_id: Option<git2::Oid>,
}

impl CommittedMessages {
    fn open(archive: &ProjectArchive) -> Result<Self, String> {
        let repo = git2::Repository::open(
            crate::archive_repo_root_checked(archive).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let (head_id, tree_id) = match repo.head() {
            Ok(head) => {
                let commit = head.peel_to_commit().map_err(|error| error.to_string())?;
                (Some(commit.id()), Some(commit.tree_id()))
            }
            Err(error)
                if matches!(
                    error.code(),
                    git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
                ) =>
            {
                (None, None)
            }
            Err(error) => return Err(error.to_string()),
        };
        Ok(Self {
            repo,
            head_id,
            tree_id,
        })
    }

    fn same_head(&self, archive: &ProjectArchive) -> Result<bool, String> {
        let current = Self::open(archive)?;
        Ok(self.head_id == current.head_id && self.tree_id == current.tree_id)
    }

    fn file_identity(
        &self,
        archive: &ProjectArchive,
        path: &std::path::Path,
    ) -> Result<(git2::Oid, usize), String> {
        let tree_id = self
            .tree_id
            .ok_or("retention requires a committed archive")?;
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, path)
            .map_err(|error| error.to_string())?;
        let tree = self
            .repo
            .find_tree(tree_id)
            .map_err(|error| error.to_string())?;
        let entry = tree
            .get_path(std::path::Path::new(&relative))
            .map_err(|error| format!("retention archive is missing {relative}: {error}"))?;
        if entry.kind() != Some(git2::ObjectType::Blob)
            || !matches!(entry.filemode(), 0o100644 | 0o100755)
        {
            return Err("retention archive evidence is not a regular-file blob".into());
        }
        let (size, kind) = self
            .repo
            .odb()
            .and_then(|odb| odb.read_header(entry.id()))
            .map_err(|error| error.to_string())?;
        if kind != git2::ObjectType::Blob {
            return Err("retention archive evidence is not a blob".into());
        }
        Ok((entry.id(), size))
    }

    fn read(
        &self,
        archive: &ProjectArchive,
        path: &std::path::Path,
    ) -> Result<Option<(Value, String)>, String> {
        let Some(tree_id) = self.tree_id else {
            return Ok(None);
        };
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, path)
            .map_err(|error| error.to_string())?;
        let tree = self
            .repo
            .find_tree(tree_id)
            .map_err(|error| error.to_string())?;
        let entry = match tree.get_path(std::path::Path::new(&relative)) {
            Ok(entry) => entry,
            Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        if entry.kind() != Some(git2::ObjectType::Blob)
            || !matches!(entry.filemode(), 0o100644 | 0o100755)
        {
            return Err("committed message is not a regular-file blob".to_string());
        }
        let odb = self.repo.odb().map_err(|error| error.to_string())?;
        let (size, kind) = odb
            .read_header(entry.id())
            .map_err(|error| error.to_string())?;
        if kind != git2::ObjectType::Blob || size > super::MAX_MESSAGE_ARTIFACT_BYTES {
            return Err("committed message exceeds the archive recovery byte bound".to_string());
        }
        let blob = self
            .repo
            .find_blob(entry.id())
            .map_err(|error| error.to_string())?;
        let actual_id = git2::Oid::hash_object_ext(
            git2::ObjectType::Blob,
            blob.content(),
            entry.id().object_format(),
        )
        .map_err(|error| error.to_string())?;
        if actual_id != entry.id() {
            return Err("committed message content does not match its object identity".to_string());
        }
        let text = std::str::from_utf8(blob.content())
            .map_err(|_| "committed message is not UTF-8".to_string())?;
        let (frontmatter, body) = text
            .strip_prefix("---json\n")
            .and_then(|text| text.split_once("\n---\n\n"))
            .ok_or_else(|| "committed message has invalid canonical frontmatter".to_string())?;
        let message = serde_json::from_str(frontmatter)
            .map_err(|_| "committed message has invalid JSON".to_string())?;
        Ok(Some((message, body.to_string())))
    }
}

/// Reconcile a bounded pass against the server's live mailbox pool.
///
/// Recent catch-up and rotating history each admit at most 16 IDs. Forward
/// catch-up reads one additional ID to distinguish a full page from its end;
/// this lookahead performs no payload or archive work in the pass. At most
/// four successful repairs and 16 MiB of serialized payload are admitted to
/// archive work per pass. SQL projections are separately bounded before JSON
/// serialization; an expanded payload that cannot fit any batch is deferred
/// without pinning the cursor. These are work bounds, not deadlines on SQL,
/// filesystem or libgit2 calls. Normal archive writes receive a 30-second grace
/// plus observed archive lag, capped at ten additional minutes. Pre-startup
/// rows and known failed-write retries remain eligible during that grace.
/// No row, receipt, delivery, notification or thread digest is mutated. Transient
/// archive refusals retain a bounded FIFO retry lane; conflicts stay on backfill
/// with a cooldown. Retries never bypass source or archive authority checks.
/// Database-promotion contention defers before consuming a candidate. A busy
/// global archive fence or project lock defers that message within the finite
/// scan; other messages can progress and the next pass revisits it. Shutdown
/// reaches the archive admission path as well as the outer loop. Blocking I/O and Git
/// operations still have no hard deadline.
///
/// Startup-aware cursors atomically checkpoint scan positions and retry debt.
/// A checkpoint is source-generation scoped scheduling metadata, never archive
/// authority. Unavailable checkpoint storage leaves repair enabled in memory.
/// The first descending history round requests catch-up after each consumed
/// page, including already-archived pages. It resumes the checkpoint's position
/// rather than restarting at the newest ID. Once that round reaches its end,
/// subsequent healthy history rounds return to the ordinary polling cadence.
/// A restored, lagging tail also receives a finite newest-first startup lane,
/// restricted to rows predating this process. It interleaves with the original
/// scans without advancing their durable watermarks or enlarging pass budgets.
/// Independently, a consumed forward page with a witnessed eligible successor
/// requests catch-up until that backlog is reached, including after a restored
/// history round finishes. Exactly-full tail pages without a successor stay idle.
///
/// # Errors
///
/// Refuses mismatched/readonly sources, source acquisition/query errors, or an
/// open corruption breaker. Ambiguous/conflicting/oversized artifacts defer.
pub fn reconcile_message_batch(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ReconcileCursor,
    stop: &AtomicBool,
) -> Result<ReconcileReport, String> {
    if cursor.persist_progress {
        checkpoint::run(cx, pool, config, cursor, stop)
    } else {
        reconcile_message_batch_inner(cx, pool, config, cursor, stop)
    }
}

fn reconcile_message_batch_inner(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ReconcileCursor,
    stop: &AtomicBool,
) -> Result<ReconcileReport, String> {
    let mut report = ReconcileReport::default();
    if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
        report.interrupted = true;
        return Ok(report);
    }
    if corruption_circuit_breaker().is_tripped() {
        return Err(
            "message reconciliation refused: source corruption breaker is open".to_string(),
        );
    }
    let cutoff = normal_archive_cutoff(
        mcp_agent_mail_db::now_micros(),
        cursor.settled_before_us,
        crate::archive_lag_snapshot().oldest_unmaterialized_us,
    );
    let selected = {
        // The selection itself must not enter a pool while promotion owns
        // admission. Refusal leaves both scan positions untouched.
        let _selection = mcp_agent_mail_db::write_barrier::try_begin_write_activity()
            .ok_or("message reconciliation deferred: recovery promotion or admission contention")?;
        validate_pool_binding(pool, config)?;
        let selected = select_ids(cx, pool, cursor, cutoff)?;
        // Selection already bound and validated the writable source. Inject
        // known failed IDs into the ordinary bounded retry lane, not a second
        // worker or a larger publication batch.
        durability::seed_retries(config, cursor);
        selected
    };
    let selected_identity = cursor.source_identity.clone();
    let tail_started_after = selected.tail_started_after;
    let tail_next_id = selected.tail_next_id;
    let now = Instant::now();
    let mut retries = cursor
        .retries
        .iter()
        .filter(|retry| retry.ready_at <= now)
        .take(MAX_RETRIES_PER_PASS)
        .map(|retry| (retry.id, None))
        .collect::<Vec<_>>()
        .into_iter();
    let mut selected = selected.ids.into_iter().map(|(id, lane)| (id, Some(lane)));
    let mut seen = HashSet::new();
    loop {
        // At saturation only a retained item can free capacity. Do not let
        // normal-lane priority pin a full queue before any due retry runs.
        let next = if cursor.next_lane_is_retry || cursor.retries.len() >= MAX_RETRY_IDS {
            retries.next().or_else(|| selected.next())
        } else {
            selected.next().or_else(|| retries.next())
        };
        let Some((id, lane)) = next else {
            break;
        };
        if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
            report.interrupted = true;
            break;
        }
        if corruption_circuit_breaker().is_tripped() {
            return Err("message reconciliation stopped: source corruption observed".to_string());
        }
        if report.repaired >= MAX_REPAIRS_PER_BATCH || report.scanned >= MAX_IDS_PER_PASS {
            report.budget_exhausted = true;
            break;
        }
        // A scan-lane duplicate cannot bypass a retry's cooldown or cause a
        // second publication attempt during the same bounded pass.
        let cooling_down = cursor
            .retries
            .iter()
            .any(|retry| retry.id == id && retry.ready_at > now)
            || cursor
                .conflict_backoff
                .iter()
                .any(|(known, until)| *known == id && *until > now);
        // A retained ID belongs to the retry lane. Consuming its scan-lane
        // duplicate must not steal the normal lane's turn or byte allowance.
        let retained_scan = lane.is_some() && cursor.retries.iter().any(|retry| retry.id == id);
        if seen.contains(&id) || cooling_down || retained_scan {
            if let Some(lane) = lane {
                cursor.advance_scan(id, lane);
            }
            continue;
        }
        // At saturation, leave the unconsumed scan position intact. Evicting
        // a transiently deferred ID would recreate the days-long wrap delay.
        if cursor.retries.len() >= MAX_RETRY_IDS
            && !cursor.retries.iter().any(|retry| retry.id == id)
        {
            break;
        }
        // Freeze recovery promotion through source observation and archive
        // publication, but never wait for closed admission on this worker.
        // A replacement between selection and this item invalidates the page;
        // the next pass rebinds its cursors before reading new-generation IDs.
        let write_activity = mcp_agent_mail_db::write_barrier::try_begin_write_activity()
            .ok_or("message reconciliation deferred: recovery promotion or admission contention")?;
        if pool.sqlite_identity_key() != selected_identity {
            return Err(
                "message reconciliation source changed after selection; rescan required".into(),
            );
        }
        seen.insert(id);
        let result = match prepare_message_classified(cx, pool, id) {
            Ok(prepared) => match payload_admission(report.payload_bytes, prepared.payload_bytes) {
                PayloadAdmission::Oversized => {
                    // SQL's raw-text bound does not bound JSON escaping. This
                    // item cannot fit an empty batch either: report it and
                    // advance below so all later mail can still converge.
                    Err(format!(
                        "serialized message payload exceeds per-batch limit ({} > {} bytes); source preserved",
                        prepared.payload_bytes, MAX_BATCH_PAYLOAD_BYTES,
                    ).into())
                }
                PayloadAdmission::NextBatch => {
                    // This item CAN fit a fresh batch. Do not consume its
                    // cursor or lane priority before it has been attempted.
                    report.budget_exhausted = true;
                    break;
                }
                PayloadAdmission::Fits => {
                    report.payload_bytes += prepared.payload_bytes;
                    if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
                        report.interrupted = true;
                        break;
                    }
                    let ticket = durability::ticket_for(config, &prepared);
                    let result = reconcile_prepared_inner(
                        config,
                        &prepared,
                        Some(RepairControl { cx, stop }),
                        // Clearing a failed-write refusal consumes an archive
                        // proof, so even unchanged failed mail must retain the
                        // original guarded verification. The optimistic path
                        // is only an observation for ordinary maintenance.
                        ticket.is_some(),
                    );
                    if result.is_ok() {
                        durability::confirm_repaired(
                            cx,
                            pool,
                            config,
                            &selected_identity,
                            stop,
                            ticket.as_ref(),
                        );
                    }
                    result
                }
            },
            Err(error) => Err(error),
        };
        // Archive guards and SQL connections have already been dropped. Slow
        // logging must not prolong this message's database-promotion exclusion.
        drop(write_activity);
        if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
            report.interrupted = true;
            break;
        }
        report.scanned += 1;
        cursor.next_lane_is_retry = lane.is_some();
        match result {
            Ok(result) if result.files_created > 0 || result.git_commit_needed => {
                report.repaired += 1;
                report.files_created += result.files_created;
                cursor.forget_retry(id);
            }
            Ok(_) => {
                report.unchanged += 1;
                cursor.forget_retry(id);
            }
            Err(error) => record_failure(id, error, cursor, &mut report, now),
        }
        if let Some(lane) = lane {
            cursor.advance_scan(id, lane);
        }
    }
    // Finding no missing files on the newest page does not establish that a
    // crash left no older orphans. Continue this finite startup history round
    // using the worker's existing one-second catch-up poll. No loop, SQL page,
    // repair count, payload bound, retry cooldown, or publication guard changes.
    // A cancelled or wholly blocked pass does not request extra polling here.
    if cursor.persist_progress
        && (!cursor.startup_backfill_complete || cursor.startup_tail.is_some())
        && report.scanned > 0
        && !report.interrupted
    {
        report.budget_exhausted = true;
    }
    // A restored history round can finish long before its forward cursor has
    // reached crash-lost mail. Keep the existing bounded catch-up schedule while
    // a consumed tail page still has an observed eligible successor. Cursor-only
    // progress through cooling duplicates also counts; a wholly blocked or
    // cancelled pass must not manufacture progress or consume the lookahead.
    if !report.interrupted
        && tail_started_after
            .zip(tail_next_id)
            .is_some_and(|(before, next)| {
                cursor
                    .tail_after
                    .is_some_and(|after| after > before && after < next)
            })
    {
        report.budget_exhausted = true;
    }
    report.retry_len = cursor.retries.len();
    Ok(report)
}

/// Verify all message copies and referenced attachments against one committed
/// tree, then check that those exact objects remain in HEAD and on disk. Git
/// staging or working-tree files alone cannot authorize deleting the DB source.
/// `None` means the complete bundle fits a fresh pass but not its remaining budget.
fn verify_retention_archive(
    archive: &ProjectArchive,
    prepared: &PreparedMessage,
    admitted_bytes: &mut usize,
) -> Result<Option<usize>, String> {
    let committed = CommittedMessages::open(archive)?;
    verify_retention_archive_at(archive, prepared, admitted_bytes, &committed, None)
}

fn verify_retention_archive_at(
    archive: &ProjectArchive,
    prepared: &PreparedMessage,
    admitted_bytes: &mut usize,
    committed: &CommittedMessages,
    control: Option<RepairControl<'_>>,
) -> Result<Option<usize>, String> {
    super::checkpoint(control).map_err(|error| error.to_string())?;
    let paths = crate::message_paths_for_bundle(
        archive,
        &prepared.message,
        &prepared.sender,
        &prepared.recipients,
    )
    .map_err(|error| error.to_string())?
    .0;
    let attachments = super::attachments::required_paths(archive, &prepared.message)
        .map_err(|error| error.to_string())?;
    let mut expected = Vec::new();
    let mut bundle_bytes = 0_usize;
    let mut message_bytes = 0_usize;
    for (path, message) in std::iter::once((&paths.canonical, true))
        .chain(std::iter::once((&paths.outbox, true)))
        .chain(paths.inbox.iter().map(|path| (path, true)))
        .chain(attachments.iter().map(|path| (path, false)))
    {
        super::checkpoint(control).map_err(|error| error.to_string())?;
        let (oid, size) = committed.file_identity(archive, path)?;
        if message && size > super::MAX_MESSAGE_ARTIFACT_BYTES {
            return Err("retention message exceeds the archive artifact byte bound".into());
        }
        bundle_bytes = bundle_bytes
            .checked_add(size)
            .filter(|bytes| *bytes <= super::MAX_BUNDLE_BYTES)
            .ok_or("retention bundle exceeds the archive byte bound")?;
        if message {
            message_bytes += size;
        }
        expected.push((path, oid));
    }
    if bundle_bytes > super::MAX_BUNDLE_BYTES.saturating_sub(*admitted_bytes) {
        return Ok(None);
    }
    // Failed/conflicting candidates consume their read admission too. Charging
    // only successful proofs would let corrupt large bundles bypass the budget.
    *admitted_bytes += bundle_bytes;
    let mut surviving = None;
    for path in [&paths.canonical, &paths.outbox] {
        super::checkpoint(control).map_err(|error| error.to_string())?;
        let (message, body) = committed
            .read(archive, path)?
            .ok_or("retention requires committed canonical and outbox copies")?;
        validate_surviving_message(prepared, &message, &body)?;
        merge_surviving_metadata(
            &mut surviving,
            message,
            "retention canonical and outbox metadata disagree",
        )?;
    }
    for path in &paths.inbox {
        super::checkpoint(control).map_err(|error| error.to_string())?;
        let (message, body) = committed
            .read(archive, path)?
            .ok_or("retention requires every committed recipient inbox copy")?;
        let message = restore_inbox_metadata(prepared, message, &body)?;
        merge_surviving_metadata(
            &mut surviving,
            message,
            "retention canonical and inbox metadata disagree",
        )?;
    }
    // Reuse attachment path, size and content-digest validation. The preceding
    // inventory requires committed blobs even when repair could use a proven
    // uncommitted original. This verifier never creates or commits files.
    super::checkpoint(control).map_err(|error| error.to_string())?;
    let files = super::attachments::prepare(
        &committed.repo,
        archive,
        &prepared.message,
        bundle_bytes - message_bytes,
    )
    .map_err(|error| error.to_string())?;
    for (path, bytes) in files {
        super::checkpoint(control).map_err(|error| error.to_string())?;
        let (oid, _) = committed.file_identity(archive, &path)?;
        if git2::Oid::hash_object_ext(git2::ObjectType::Blob, &bytes, oid.object_format())
            .map_err(|error| error.to_string())?
            != oid
        {
            return Err("retention attachment differs from its committed authority".into());
        }
    }
    let current = CommittedMessages::open(archive)?;
    for (path, oid) in expected {
        super::checkpoint(control).map_err(|error| error.to_string())?;
        if current.file_identity(archive, path)?.0 != oid {
            return Err("retention archive changed during verification".into());
        }
        let blob = committed
            .repo
            .find_blob(oid)
            .map_err(|error| error.to_string())?;
        if git2::Oid::hash_object_ext(git2::ObjectType::Blob, blob.content(), oid.object_format())
            .map_err(|error| error.to_string())?
            != oid
            || super::artifact_missing(path, blob.content()).map_err(|error| error.to_string())?
        {
            return Err("retention requires intact committed and on-disk archive copies".into());
        }
    }
    Ok(Some(bundle_bytes))
}

enum PruneAttempt {
    Pruned(MessagePruneReport),
    NextBatch,
    Interrupted,
}

fn prune_retention_candidate(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    older_than_us: i64,
    id: i64,
    report: &mut ArchivePruneReport,
    stop: &AtomicBool,
) -> Result<PruneAttempt, String> {
    let candidate = outcome(block_on(capture_message_prune_candidate(cx, pool, id)))?
        .ok_or("retention source is missing, oversized or incomplete")?;
    let prepared = source::prepare_source_rows(id, candidate.source_rows())?;
    match payload_admission(report.payload_bytes, prepared.payload_bytes) {
        PayloadAdmission::NextBatch => return Ok(PruneAttempt::NextBatch),
        PayloadAdmission::Oversized => {
            return Err("retention source exceeds the payload byte bound".into());
        }
        PayloadAdmission::Fits => report.payload_bytes += prepared.payload_bytes,
    }
    // Match archive writers' lock order: publication fence, then project lock.
    // Keep both through the DB commit so cooperating archive writes cannot
    // invalidate the evidence between verification and destructive retention.
    // Replayable maintenance must not retain promotion admission indefinitely
    // behind a busy publisher. Use the native recovery fence and lock budget.
    let control = RepairControl { cx, stop };
    let _mutation = control
        .begin_at(&config.storage_root)
        .map_err(|error| error.to_string())?;
    let archive = crate::open_archive(config, &prepared.project_slug)
        .map_err(|error| error.to_string())?
        .ok_or("retention requires an existing authoritative project archive")?;
    archive
        .with_repair_lock(
            || control.cancelled(),
            || {
                if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
                    return Ok(PruneAttempt::Interrupted);
                }
                if verify_retention_archive(&archive, &prepared, &mut report.archive_bytes)
                    .map_err(super::invalid)?
                    .is_none()
                {
                    return Ok(PruneAttempt::NextBatch);
                }
                if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
                    return Ok(PruneAttempt::Interrupted);
                }
                let pruned = outcome(block_on(prune_verified_settled_messages(
                    cx,
                    pool,
                    older_than_us,
                    1,
                    std::slice::from_ref(&candidate),
                )))
                .map_err(super::invalid)?;
                Ok(PruneAttempt::Pruned(pruned))
            },
        )
        .map_err(|error| error.to_string())
}

/// Prune only settled messages whose complete archive has been verified.
///
/// Each pass inspects at most 32 IDs and admits 16 MiB of prepared DB payload
/// plus 64 MiB of unique archive bundle content. Raw SQL projections have a
/// separate per-candidate bound; invalid projections still count toward the ID
/// limit. Missing, conflicting or oversized evidence leaves the
/// DB row intact for the repair worker. The deletion transaction rechecks the
/// exact source projection and settlement observed before archive I/O.
/// Connections are released during archive reads. One write-activity lease
/// excludes recovery promotion from source selection through deletion; each
/// candidate also retains the archive publication fence and project lock.
/// Promotion and publication contention defer without waiting indefinitely;
/// project-lock admission shares recovery's bounded, cancellation-aware budget.
/// Deferred evidence is always reverified before a later deletion attempt.
pub fn prune_archived_message_batch(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    older_than_us: i64,
    cursor: &mut ArchivePruneCursor,
    stop: &AtomicBool,
) -> Result<ArchivePruneReport, String> {
    let mut report = ArchivePruneReport::default();
    if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
        report.interrupted = true;
        return Ok(report);
    }
    if corruption_circuit_breaker().is_tripped() {
        return Err("archive-verified retention refused: source corruption breaker is open".into());
    }
    let _write_activity = mcp_agent_mail_db::write_barrier::try_begin_write_activity()
        .ok_or("archive-verified retention deferred: recovery promotion or admission contention")?;
    validate_pool_binding(pool, config)?;
    {
        let conn = outcome(block_on(pool.acquire(cx)))?;
        let rows = conn
            .query_sync("PRAGMA query_only", &[])
            .map_err(source_error)?;
        if rows.first().and_then(|row| row.get_as::<i64>(0).ok()) != Some(0) {
            return Err("query-only snapshots cannot authorize message retention".into());
        }
    }
    let identity = pool.sqlite_identity_key();
    if cursor.source_identity != identity {
        *cursor = ArchivePruneCursor {
            source_identity: identity,
            ..Default::default()
        };
    }
    let page = outcome(block_on(list_prunable_message_ids(
        cx,
        pool,
        older_than_us,
        cursor.after,
        cursor.ceiling,
        mcp_agent_mail_db::queries::MESSAGE_PRUNE_MAX_CANDIDATES,
    )))?;
    cursor.ceiling = Some(page.ceiling);
    let mut completed_page = true;
    for id in page.ids {
        if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() {
            report.interrupted = true;
            completed_page = false;
            break;
        }
        if corruption_circuit_breaker().is_tripped() {
            return Err("archive-verified retention stopped: source corruption observed".into());
        }
        let result =
            prune_retention_candidate(cx, pool, config, older_than_us, id, &mut report, stop);
        match result {
            Ok(PruneAttempt::NextBatch) => {
                report.budget_exhausted = true;
                completed_page = false;
                break;
            }
            Ok(PruneAttempt::Interrupted) => {
                report.interrupted = true;
                completed_page = false;
                break;
            }
            Ok(PruneAttempt::Pruned(pruned)) => {
                report.pruned.deleted_messages += pruned.deleted_messages;
                report.pruned.deleted_recipients += pruned.deleted_recipients;
                if pruned.deleted_messages == 0 {
                    report.deferred += 1;
                }
            }
            Err(_) if stop.load(Ordering::Acquire) || cx.checkpoint().is_err() => {
                // A cancelled lock wait did not inspect or delete this source.
                // Preserve its position rather than consuming it as a conflict.
                report.interrupted = true;
                completed_page = false;
                break;
            }
            Err(reason) => {
                report.deferred += 1;
                tracing::warn!(
                    target: "maintenance", event = "message_retention_archive_deferred",
                    message_id = id, reason = %reason,
                    "settled message retained until complete archive evidence is available"
                );
            }
        }
        report.scanned += 1;
        cursor.after = id;
    }
    report.pruned.more = page.more || !completed_page || report.deferred > 0;
    if completed_page && !page.more {
        cursor.after = 0;
        cursor.ceiling = None;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared() -> PreparedMessage {
        let message = json!({
            "id": 9, "from": "BlueLake", "to": ["GreenStone"], "cc": [], "bcc": ["RedFox"],
            "subject": "handoff", "created": "2026-09-17T01:02:03.123456Z",
            "thread_id": "thread", "topic": null, "project": "/project", "project_slug": "project",
            "importance": "normal", "ack_required": false, "attachments": [],
        });
        PreparedMessage {
            message,
            body: "body\n".into(),
            sender: "BlueLake".into(),
            project_slug: "project".into(),
            recipients: vec!["GreenStone".into(), "RedFox".into()],
            payload_bytes: 512,
            archive_metadata_known: false,
        }
    }

    #[test]
    fn enable_override_is_explicit_and_invalid_values_fail_disabled() {
        assert!(parse_enabled(None));
        for raw in ["true", " 1 ", "YES", "on"] {
            assert!(parse_enabled(Some(raw)));
        }
        for raw in ["false", "0", "off", "no", "", "typo"] {
            assert!(!parse_enabled(Some(raw)));
        }
        let config = Config {
            database_url: "sqlite:///:memory:".into(),
            ..Config::default()
        };
        assert!(
            !enabled(&config),
            "ephemeral mailboxes do not start archive maintenance"
        );
    }

    #[test]
    fn surviving_reply_metadata_is_preserved_but_conflicting_fields_refuse() {
        let original = prepared();
        let mut message = original.message.clone();
        message["reply_to"] = json!(7);
        message["future_metadata"] = json!({"opaque": true});
        validate_surviving_message(&original, &message, &original.body).unwrap();
        for (key, value) in [
            ("id", json!(10)),
            ("from", json!("RedFox")),
            ("to", json!(["RedFox"])),
            ("bcc", json!([])),
            ("project", json!("/other")),
            ("reply_to", json!(9)),
            ("created", json!("invalid")),
        ] {
            let mut changed = message.clone();
            changed[key] = value;
            assert!(
                validate_surviving_message(&original, &changed, &original.body).is_err(),
                "{key}"
            );
        }
        assert!(validate_surviving_message(&original, &message, "different body").is_err());
        assert_eq!(message["reply_to"], 7);
    }

    #[test]
    fn unarchived_thread_is_deferred_instead_of_inventing_its_parent() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            ..Config::default()
        };
        let original = prepared();
        let error = reconcile_prepared(&config, &original).unwrap_err();
        assert!(error.contains("reply metadata cannot be inferred"));
        let archive = crate::ensure_archive(&config, "project").unwrap();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        assert!(!paths.canonical.exists());
    }

    #[test]
    fn inbox_only_reply_recovery_preserves_parent_extensions_and_bcc_privacy() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            ..Config::default()
        };
        let original = prepared();
        let archive = crate::ensure_archive(&config, "project").unwrap();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let mut full = original.message.clone();
        full["reply_to"] = json!(7);
        full["future_metadata"] = json!({"opaque": ["keep", 42]});
        let redacted = crate::redact_message_bcc_for_inbox(&full);
        let bytes = crate::render_message_bundle_content(&redacted, &original.body).unwrap();
        crate::ensure_parent_dir(&paths.inbox[0]).unwrap();
        std::fs::write(&paths.inbox[0], bytes.as_bytes()).unwrap();

        let repaired = reconcile_prepared(&config, &original).unwrap();
        assert_eq!(repaired.files_created, 3);
        assert!(repaired.git_commit_needed);
        for path in [&paths.canonical, &paths.outbox] {
            let (message, body) = read_surviving_message(path).unwrap().unwrap();
            assert_eq!(message, full);
            assert_eq!(body, original.body);
        }
        for path in &paths.inbox {
            assert_eq!(std::fs::read(path).unwrap(), bytes.as_bytes());
            let (message, _) = read_surviving_message(path).unwrap().unwrap();
            assert_eq!(message["bcc"], json!([]));
            assert_eq!(message["reply_to"], 7);
        }
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        let before = repo.head().unwrap().target().unwrap();
        assert_eq!(
            reconcile_prepared(&config, &original).unwrap(),
            ReconcileResult::default()
        );
        assert_eq!(repo.head().unwrap().target().unwrap(), before);
    }

    #[test]
    fn conflicting_inbox_reply_parents_preserve_all_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            ..Config::default()
        };
        let original = prepared();
        let archive = crate::ensure_archive(&config, "project").unwrap();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let mut originals = Vec::new();
        for (index, path) in paths.inbox.iter().enumerate() {
            let mut message = crate::redact_message_bcc_for_inbox(&original.message);
            message["reply_to"] = json!(7 + index);
            let bytes = crate::render_message_bundle_content(&message, &original.body).unwrap();
            crate::ensure_parent_dir(path).unwrap();
            std::fs::write(path, bytes.as_bytes()).unwrap();
            originals.push(bytes);
        }
        let error = reconcile_prepared(&config, &original).unwrap_err();
        assert!(error.contains("inbox metadata disagree"), "{error}");
        assert!(!paths.canonical.exists());
        assert!(!paths.outbox.exists());
        for (path, bytes) in paths.inbox.iter().zip(originals) {
            assert_eq!(std::fs::read(path).unwrap(), bytes.as_bytes());
        }
    }

    #[test]
    fn inbox_recovery_refuses_private_routing_and_malformed_redaction() {
        let original = prepared();
        let redacted = crate::redact_message_bcc_for_inbox(&original.message);
        for bcc in [json!(["RedFox"]), json!(null), json!("RedFox")] {
            let mut message = redacted.clone();
            message["bcc"] = bcc;
            let error = restore_inbox_metadata(&original, message, &original.body).unwrap_err();
            assert!(error.contains("BCC redaction"), "{error}");
        }
        let mut missing = redacted;
        missing.as_object_mut().unwrap().remove("bcc");
        assert!(restore_inbox_metadata(&original, missing, &original.body).is_err());
    }

    #[test]
    fn inbox_recovery_still_requires_live_identity_body_and_visible_routing() {
        let original = prepared();
        let redacted = crate::redact_message_bcc_for_inbox(&original.message);
        for (key, value) in [
            ("id", json!(10)),
            ("project_slug", json!("other-project")),
            ("to", json!(["RedFox"])),
            ("cc", json!(["RedFox"])),
            ("reply_to", json!(9)),
        ] {
            let mut message = redacted.clone();
            message[key] = value;
            assert!(
                restore_inbox_metadata(&original, message, &original.body).is_err(),
                "{key}"
            );
        }
        assert!(restore_inbox_metadata(&original, redacted, "different body").is_err());
    }

    /// Store message artifacts only in Git, never in the working tree. Existing
    /// tree entries and parent commits remain intact throughout these fixtures.
    fn commit_survivors(
        archive: &ProjectArchive,
        files: &[(&std::path::Path, &Value)],
        body: &str,
    ) -> git2::Oid {
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
        let baseline = match &parent {
            Some(parent) => parent.tree().unwrap(),
            None => {
                let id = repo.treebuilder(None).unwrap().write().unwrap();
                repo.find_tree(id).unwrap()
            }
        };
        let mut updates = git2::build::TreeUpdateBuilder::new();
        for (path, message) in files {
            let bytes = crate::render_message_bundle_content(message, body).unwrap();
            let blob = repo.blob(bytes.as_bytes()).unwrap();
            let relative = crate::rel_path_cached(&archive.canonical_repo_root, path).unwrap();
            updates.upsert(relative.as_str(), blob, git2::FileMode::Blob);
        }
        let id = updates.create_updated(&repo, &baseline).unwrap();
        let tree = repo.find_tree(id).unwrap();
        let signature = git2::Signature::now("reconcile-test", "reconcile@test.invalid").unwrap();
        let parents: Vec<_> = parent.iter().collect();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "surviving mail",
            &tree,
            &parents,
        )
        .unwrap()
    }

    pub(super) fn git_fixture() -> (tempfile::TempDir, Config, PreparedMessage, ProjectArchive) {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            ..Config::default()
        };
        let original = prepared();
        let archive = crate::ensure_archive(&config, &original.project_slug).unwrap();
        (temp, config, original, archive)
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn prepared_repair_releases_metadata_fence_and_rechecks_surviving_reply() {
        use super::super::cancellable_tests::{ObserveScan, isolated};
        use std::fs;
        use std::sync::atomic::Ordering;

        if isolated() {
            return;
        }
        for replace_survivor in [false, true] {
            let (_temp, config, original, archive) = git_fixture();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &original.message,
                &original.sender,
                &original.recipients,
            )
            .unwrap()
            .0;
            let mut full = original.message.clone();
            full["reply_to"] = json!(7);
            full["future_metadata"] = json!({"opaque": ["keep", 42]});
            let redacted = crate::redact_message_bcc_for_inbox(&full);
            let survivor_bytes =
                crate::render_message_bundle_content(&redacted, &original.body).unwrap();
            crate::ensure_parent_dir(&paths.inbox[0]).unwrap();
            fs::write(&paths.inbox[0], survivor_bytes.as_bytes()).unwrap();
            let history = archive.root.join("messages/2025/01/other.md");
            commit_survivors(&archive, &[(&history, &json!({"id": 99}))], "history");
            let other = crate::ensure_archive(&config, "reservation-peer").unwrap();
            let op = crate::WriteOp::FileReservation {
                project_slug: other.slug.clone(),
                config: config.clone(),
                reservations: vec![json!({
                    "id": 1, "agent": "GreenCastle", "path_pattern": "src/**",
                    "exclusive": true, "reason": "unrelated reservation",
                    "created_ts": "2026-10-01T00:00:00Z", "expires_ts": "2030-10-01T00:00:00Z",
                })],
            };
            let mut replacement = redacted.clone();
            replacement["reply_to"] = json!(8);
            replacement["future_metadata"] = json!({"concurrent": "preserve"});
            let replacement_bytes =
                crate::render_message_bundle_content(&replacement, &original.body).unwrap();
            let replacement_for_hook = replacement_bytes.clone();
            let survivor = paths.inbox[0].clone();
            let visited = std::sync::Arc::new(AtomicBool::new(false));
            let hook_visited = visited.clone();
            let observer = ObserveScan::new(move |stage| {
                if stage != "git blob" || hook_visited.swap(true, Ordering::AcqRel) {
                    return;
                }
                let op = op.clone();
                let result = std::thread::spawn(move || crate::write_op_sync_direct(&op))
                    .join()
                    .unwrap();
                assert!(
                    matches!(result, crate::DirectArchiveWrite::Written),
                    "{result:?}"
                );
                crate::flush_async_commits();
                if replace_survivor {
                    fs::write(&survivor, replacement_for_hook.as_bytes()).unwrap();
                }
            });
            let cx = Cx::for_testing();
            let stop = AtomicBool::new(false);
            let result = reconcile_prepared_inner(
                &config,
                &original,
                Some(RepairControl {
                    cx: &cx,
                    stop: &stop,
                }),
                true,
            );
            drop(observer);
            assert!(visited.load(Ordering::Acquire));
            let reservation: Value = serde_json::from_slice(
                &fs::read(other.root.join("file_reservations/id-1.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(reservation["agent"], "GreenCastle");
            assert_eq!(reservation["path_pattern"], "src/**");
            if replace_survivor {
                let error = result.unwrap_err();
                assert!(error.detail.contains("conflict"), "{}", error.detail);
                assert!(!paths.canonical.exists());
                assert!(!paths.outbox.exists());
                assert!(!paths.inbox[1].exists());
                assert_eq!(
                    fs::read(&paths.inbox[0]).unwrap(),
                    replacement_bytes.as_bytes()
                );
            } else {
                let repaired = result.unwrap();
                assert_eq!(repaired.files_created, 3);
                assert!(repaired.git_commit_needed);
                for path in [&paths.canonical, &paths.outbox] {
                    let (message, body) = read_surviving_message(path).unwrap().unwrap();
                    assert_eq!(message, full);
                    assert_eq!(body, original.body);
                }
                for path in &paths.inbox {
                    assert_eq!(fs::read(path).unwrap(), survivor_bytes.as_bytes());
                }
                let repo = git2::Repository::open(&archive.repo_root).unwrap();
                let tree = repo.head().unwrap().peel_to_tree().unwrap();
                for path in [&paths.canonical, &paths.outbox]
                    .into_iter()
                    .chain(paths.inbox.iter())
                {
                    let relative =
                        crate::rel_path_cached(&archive.canonical_repo_root, path).unwrap();
                    let entry = tree.get_path(std::path::Path::new(&relative)).unwrap();
                    let blob = repo.find_blob(entry.id()).unwrap();
                    assert_eq!(blob.content(), fs::read(path).unwrap().as_slice());
                }
            }
        }
    }

    #[test]
    fn committed_inbox_only_restores_all_copies_and_is_idempotent() {
        let (_temp, config, original, archive) = git_fixture();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let mut full = original.message.clone();
        full["reply_to"] = json!(7);
        full["future_metadata"] = json!({"opaque": ["keep", 42]});
        let redacted = crate::redact_message_bcc_for_inbox(&full);
        let source = commit_survivors(
            &archive,
            &[
                (paths.inbox[0].as_path(), &redacted),
                (paths.inbox[1].as_path(), &redacted),
            ],
            &original.body,
        );
        for path in [&paths.canonical, &paths.outbox]
            .into_iter()
            .chain(paths.inbox.iter())
        {
            assert!(!path.exists());
        }
        let result = reconcile_prepared(&config, &original).unwrap();
        assert_eq!(result.files_created, 4);
        assert!(result.git_commit_needed);
        for path in [&paths.canonical, &paths.outbox] {
            let (message, body) = read_surviving_message(path).unwrap().unwrap();
            assert_eq!(message, full);
            assert_eq!(body, original.body);
        }
        for path in &paths.inbox {
            let (message, body) = read_surviving_message(path).unwrap().unwrap();
            assert_eq!(message, redacted);
            assert_eq!(body, original.body);
            assert_eq!(message["bcc"], json!([]));
        }
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        assert!(repo.find_commit(source).is_ok());
        let head = repo.head().unwrap().target().unwrap();
        assert_eq!(
            reconcile_prepared(&config, &original).unwrap(),
            ReconcileResult::default()
        );
        assert_eq!(repo.head().unwrap().target().unwrap(), head);
    }

    #[test]
    fn conflicting_committed_full_copies_are_not_silently_selected() {
        let (_temp, config, original, archive) = git_fixture();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let mut first = original.message.clone();
        first["reply_to"] = json!(7);
        let mut second = first.clone();
        second["reply_to"] = json!(8);
        let head = commit_survivors(
            &archive,
            &[
                (paths.canonical.as_path(), &first),
                (paths.outbox.as_path(), &second),
            ],
            &original.body,
        );
        let error = reconcile_prepared(&config, &original).unwrap_err();
        assert!(
            error.contains("committed canonical and outbox metadata disagree"),
            "{error}"
        );
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        assert_eq!(repo.head().unwrap().target().unwrap(), head);
        assert!(!paths.canonical.exists());
        assert!(!paths.outbox.exists());
    }

    #[test]
    fn conflicting_committed_inbox_parents_preserve_head_and_worktree() {
        let (_temp, config, original, archive) = git_fixture();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let mut first = crate::redact_message_bcc_for_inbox(&original.message);
        first["reply_to"] = json!(7);
        let mut second = first.clone();
        second["reply_to"] = json!(8);
        let head = commit_survivors(
            &archive,
            &[
                (paths.inbox[0].as_path(), &first),
                (paths.inbox[1].as_path(), &second),
            ],
            &original.body,
        );
        let error = reconcile_prepared(&config, &original).unwrap_err();
        assert!(
            error.contains("committed inbox metadata disagree"),
            "{error}"
        );
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        assert_eq!(repo.head().unwrap().target().unwrap(), head);
        for path in [&paths.canonical, &paths.outbox]
            .into_iter()
            .chain(paths.inbox.iter())
        {
            assert!(!path.exists());
        }
    }

    #[test]
    fn committed_inbox_cannot_supply_bcc_or_a_different_body() {
        for wrong_body in [false, true] {
            let (_temp, config, original, archive) = git_fixture();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &original.message,
                &original.sender,
                &original.recipients,
            )
            .unwrap()
            .0;
            let mut message = crate::redact_message_bcc_for_inbox(&original.message);
            message["reply_to"] = json!(7);
            if !wrong_body {
                message["bcc"] = json!(["RedFox"]);
            }
            let body = if wrong_body {
                "different"
            } else {
                &original.body
            };
            let head = commit_survivors(&archive, &[(paths.inbox[0].as_path(), &message)], body);
            let error = reconcile_prepared(&config, &original).unwrap_err();
            let expected = if wrong_body {
                "body conflicts"
            } else {
                "BCC redaction"
            };
            assert!(error.contains(expected), "{error}");
            let repo = git2::Repository::open(&archive.repo_root).unwrap();
            assert_eq!(repo.head().unwrap().target().unwrap(), head);
            assert!(!paths.canonical.exists());
        }
    }

    #[test]
    fn committed_message_snapshot_does_not_follow_head_advancement() {
        let (_temp, _config, original, archive) = git_fixture();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &original.message,
            &original.sender,
            &original.recipients,
        )
        .unwrap()
        .0;
        let mut first = crate::redact_message_bcc_for_inbox(&original.message);
        first["reply_to"] = json!(7);
        commit_survivors(
            &archive,
            &[(paths.inbox[0].as_path(), &first)],
            &original.body,
        );
        let snapshot = CommittedMessages::open(&archive).unwrap();
        let mut second = first.clone();
        second["reply_to"] = json!(8);
        commit_survivors(
            &archive,
            &[(paths.inbox[0].as_path(), &second)],
            &original.body,
        );
        let (observed, _) = snapshot.read(&archive, &paths.inbox[0]).unwrap().unwrap();
        assert_eq!(observed, first);
        let (observed, _) = CommittedMessages::open(&archive)
            .unwrap()
            .read(&archive, &paths.inbox[0])
            .unwrap()
            .unwrap();
        assert_eq!(observed, second);
    }

    #[test]
    fn payload_admission_distinguishes_fresh_batch_from_impossible_payload() {
        assert_eq!(
            payload_admission(0, MAX_BATCH_PAYLOAD_BYTES),
            PayloadAdmission::Fits
        );
        assert_eq!(
            payload_admission(1, MAX_BATCH_PAYLOAD_BYTES),
            PayloadAdmission::NextBatch
        );
        assert_eq!(
            payload_admission(0, MAX_BATCH_PAYLOAD_BYTES + 1),
            PayloadAdmission::Oversized
        );
        assert_eq!(
            payload_admission(MAX_BATCH_PAYLOAD_BYTES, 1),
            PayloadAdmission::NextBatch
        );
        assert_eq!(
            payload_admission(usize::MAX, usize::MAX),
            PayloadAdmission::Oversized
        );
        assert_eq!(
            payload_admission(usize::MAX, 1),
            PayloadAdmission::NextBatch
        );
        assert_eq!(
            payload_admission(MAX_BATCH_PAYLOAD_BYTES - 1, 1),
            PayloadAdmission::Fits
        );
    }

    #[test]
    fn json_expansion_can_exceed_batch_limit_without_exceeding_sql_input_limit() {
        let subject = "\u{0001}".repeat(3 * 1024 * 1024);
        assert!(subject.len() < usize::try_from(MAX_DB_PAYLOAD_BYTES).unwrap());
        let payload = json!({"subject": subject}).to_string();
        assert!(payload.len() > MAX_BATCH_PAYLOAD_BYTES);
        assert_eq!(
            payload_admission(0, payload.len()),
            PayloadAdmission::Oversized
        );
    }

    #[test]
    fn lane_order_survives_single_payload_batch_budgets() {
        let mut cursor = ReconcileCursor::default();
        let mut tail_next = 1000;
        let mut history_next = 999;
        let mut tail_count = 0;
        let mut history_count = 0;
        let mut seen = HashSet::new();
        for _ in 0..100 {
            let ids = interleave_ids(&[tail_next], &[history_next], cursor.next_lane_is_history);
            let (id, tail) = ids[0];
            assert!(seen.insert(id));
            assert_eq!(
                payload_admission(0, MAX_BATCH_PAYLOAD_BYTES),
                PayloadAdmission::Fits
            );
            assert_eq!(
                payload_admission(MAX_BATCH_PAYLOAD_BYTES, 1),
                PayloadAdmission::NextBatch
            );
            // Exactly one item fits. The rejected second item must retain
            // priority across the next selection, not be perpetually second.
            cursor.advance(id, tail);
            if tail {
                tail_count += 1;
                tail_next += 1;
            } else {
                history_count += 1;
                history_next -= 1;
            }
        }
        assert_eq!((tail_count, history_count), (50, 50));
        assert_eq!(cursor.tail_after, Some(1049));
        assert_eq!(cursor.backfill_ceiling, Some(949));
    }

    #[test]
    fn startup_priority_preserves_both_normal_lanes_when_one_payload_fills_a_pass() {
        let mut cursor = ReconcileCursor {
            tail_after: Some(999),
            backfill_ceiling: Some(999),
            startup_tail: Some(StartupTail {
                after: 2000,
                ceiling: 9000,
            }),
            startup_tail_initialized: true,
            next_lane_is_startup: true,
            ..Default::default()
        };
        let mut counts = [0; 3];
        let mut seen = HashSet::new();
        for _ in 0..100 {
            let normal = interleave_ids(
                &[cursor.tail_after.unwrap() + 1],
                &[cursor.backfill_ceiling.unwrap()],
                cursor.next_lane_is_history,
            );
            let selected = interleave_startup_ids(
                &normal,
                &[cursor.startup_tail.as_ref().unwrap().ceiling],
                cursor.next_lane_is_startup,
            );
            let (id, lane) = selected[0];
            assert!(seen.insert(id));
            assert_eq!(
                payload_admission(0, MAX_BATCH_PAYLOAD_BYTES),
                PayloadAdmission::Fits
            );
            assert_eq!(
                payload_admission(MAX_BATCH_PAYLOAD_BYTES, 1),
                PayloadAdmission::NextBatch
            );
            cursor.advance_scan(id, lane);
            counts[match lane {
                ScanLane::Tail => 0,
                ScanLane::History => 1,
                ScanLane::StartupTail => 2,
            }] += 1;
        }
        assert_eq!(counts, [25, 25, 50]);
        assert_eq!(cursor.tail_after, Some(1024));
        assert_eq!(cursor.backfill_ceiling, Some(974));
        assert_eq!(cursor.startup_tail.unwrap().ceiling, 8950);
    }

    #[test]
    fn interleaving_preserves_each_lane_and_duplicate_cursor_updates() {
        assert_eq!(
            interleave_ids(&[10, 11], &[9, 8, 7], true),
            vec![(9, false), (10, true), (8, false), (11, true), (7, false)]
        );
        assert_eq!(
            interleave_ids(&[], &[9, 8], false),
            vec![(9, false), (8, false)]
        );
        assert_eq!(
            interleave_ids(&[10, 11], &[], true),
            vec![(10, true), (11, true)]
        );
        let mut cursor = ReconcileCursor::default();
        let mut processed = HashSet::new();
        for (id, tail) in interleave_ids(&[9], &[9], false) {
            processed.insert(id);
            cursor.advance(id, tail);
        }
        assert_eq!(processed.len(), 1);
        assert_eq!(cursor.tail_after, Some(9));
        assert_eq!(cursor.backfill_ceiling, Some(8));
    }

    #[test]
    fn oversized_projection_does_not_pin_later_mail_and_preserves_source() {
        if admission_tests::isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let db_path = temp.path().join("mail.sqlite3");
            let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&db_path);
            let pool = mcp_agent_mail_db::create_pool(&mcp_agent_mail_db::DbPoolConfig {
                database_url: database_url.clone(),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let cx = Cx::for_testing();
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(101, 'project', '/project', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES(101, 101, 'BlueLake', 'test', 'test', '', 1, 1), (102, 101, 'GreenStone', 'test', 'test', '', 1, 1)").unwrap();
            for id in [901, 902] {
                conn.execute_raw(&format!(
                    "INSERT INTO messages(id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) VALUES({id}, 101, 101, 'small', 'body', 'normal', 0, 1000000, '{{\"to\":[\"GreenStone\"],\"cc\":[],\"bcc\":[]}}', '[]')"
                )).unwrap();
                conn.execute_raw(&format!(
                    "INSERT INTO message_recipients(message_id, agent_id, kind) VALUES({id}, 102, 'to')"
                )).unwrap();
            }
            // Literal contains no SQL quotes; control characters are stored raw
            // and expand sixfold only when the archive JSON is serialized.
            let subject = "\u{0001}".repeat(3 * 1024 * 1024);
            conn.execute_raw(&format!(
                "UPDATE messages SET subject = '{subject}' WHERE id = 901"
            ))
            .unwrap();
            drop(conn);
            std::fs::create_dir_all(pool.storage_root()).unwrap();
            let config = Config {
                database_url,
                storage_root: pool.storage_root().to_path_buf(),
                ..Config::default()
            };
            let oversized = prepare_message(&cx, &pool, 901).unwrap();
            assert!(oversized.payload_bytes > MAX_BATCH_PAYLOAD_BYTES);
            let mut cursor = ReconcileCursor {
                source_identity: pool.sqlite_identity_key(),
                tail_after: Some(900),
                ..Default::default()
            };
            let report =
                reconcile_message_batch(&cx, &pool, &config, &mut cursor, &AtomicBool::new(false))
                    .unwrap();
            assert_eq!(report.scanned, 2);
            assert_eq!(report.deferred, 1);
            assert_eq!(report.repaired, 1);
            assert_eq!(report.files_created, 3);
            assert!(!report.budget_exhausted);
            assert_eq!(cursor.tail_after, Some(902));
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            let rows = conn
                .query_sync("SELECT subject FROM messages WHERE id = 901", &[])
                .unwrap();
            assert_eq!(rows[0].get_named::<String>("subject").unwrap(), subject);
            let receipts = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 902",
                    &[],
                )
                .unwrap();
            assert_eq!(
                receipts[0].get_named::<Option<i64>>("read_ts").unwrap(),
                None
            );
            assert_eq!(
                receipts[0].get_named::<Option<i64>>("ack_ts").unwrap(),
                None
            );
        });
    }

    pub(super) fn retention_fixture(count: i64, test: impl FnOnce(&Cx, &DbPool, &Config)) {
        // Recovery now refuses unrelated process-global lock contention.
        // Keep the existing exact expectations isolated from parallel tests.
        if admission_tests::isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let storage_root = temp.path().join("archive");
            std::fs::create_dir_all(&storage_root).unwrap();
            let database_url =
                mcp_agent_mail_core::disk::sqlite_url_from_path(&temp.path().join("mail.sqlite3"));
            let pool = mcp_agent_mail_db::create_pool(&mcp_agent_mail_db::DbPoolConfig {
                database_url: database_url.clone(),
                storage_root: Some(storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let cx = Cx::for_testing();
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(101, 'project', '/project', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) \
                VALUES(101, 101, 'BlueLake', 'test', 'test', '', 1, 1), \
                (102, 101, 'GreenStone', 'test', 'test', '', 1, 1), \
                (103, 101, 'RedFox', 'test', 'test', '', 1, 1)").unwrap();
            for id in 901..901 + count {
                conn.execute_raw(&format!(
                    "INSERT INTO messages(id, project_id, sender_id, thread_id, topic, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments, archive_metadata_json) \
                    VALUES({id}, 101, 101, 'retained-thread', 'handoffs', 'handoff-{id}', 'Keep every body.', 'high', 1, 1000000, \
                    '{{\"to\":[\"GreenStone\"],\"cc\":[],\"bcc\":[\"RedFox\"]}}', '[]', '{{\"reply_to\":700}}')"
                )).unwrap();
                conn.execute_raw(&format!(
                    "INSERT INTO message_recipients(message_id, agent_id, kind, read_ts, ack_ts) \
                    VALUES({id}, 102, 'to', 2000000, 2500000), ({id}, 103, 'bcc', 2000000, 2500000)"
                ))
                .unwrap();
            }
            drop(conn);
            test(
                &cx,
                &pool,
                &Config {
                    database_url,
                    storage_root,
                    ..Config::default()
                },
            );
        });
    }

    /// br-kp1in.14: mail committed moments before a crash is repaired on the
    /// restarted reconciler's first pass. The 30 s grace still shields rows
    /// that this process's own write-behind queue may be writing.
    #[test]
    fn rows_predating_the_reconciling_process_skip_the_in_flight_grace() {
        retention_fixture(3, |cx, pool, config| {
            let committed_just_before_crash = mcp_agent_mail_db::now_micros();
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(&format!(
                "UPDATE messages SET created_ts = {committed_just_before_crash}"
            ))
            .unwrap();
            drop(conn);
            let stop = AtomicBool::new(false);

            let mut in_process = ReconcileCursor::default();
            let shielded =
                reconcile_message_batch(cx, pool, config, &mut in_process, &stop).unwrap();
            assert_eq!(
                (shielded.scanned, shielded.repaired),
                (0, 0),
                "rows this process may still be writing wait out the grace"
            );

            let mut restarted = ReconcileCursor::settled_before(committed_just_before_crash + 1);
            let first = reconcile_message_batch(cx, pool, config, &mut restarted, &stop).unwrap();
            assert_eq!(first.repaired, 3, "{first:?}");
            let again = reconcile_message_batch(cx, pool, config, &mut restarted, &stop).unwrap();
            assert_eq!(again.repaired, 0, "{again:?}");
        });
    }

    fn retained_message_ids(cx: &Cx, pool: &DbPool) -> Vec<i64> {
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.query_sync("SELECT id FROM messages ORDER BY id", &[])
            .unwrap()
            .iter()
            .map(|row| row.get_named("id").unwrap())
            .collect()
    }

    #[test]
    fn retention_backlog_never_outpaces_complete_archive_repair() {
        retention_fixture(7, |cx, pool, config| {
            let originals = (901..908)
                .map(|id| prepare_message(cx, pool, id).unwrap())
                .collect::<Vec<_>>();
            let stop = AtomicBool::new(false);
            let mut prune_cursor = ArchivePruneCursor::default();
            let missing =
                prune_archived_message_batch(cx, pool, config, 3_000_000, &mut prune_cursor, &stop)
                    .unwrap();
            assert_eq!(missing.pruned.deleted_messages, 0);
            assert_eq!(missing.deferred, 7);
            assert_eq!(retained_message_ids(cx, pool).len(), 7);

            let mut repair_cursor = ReconcileCursor::default();
            let first =
                reconcile_message_batch(cx, pool, config, &mut repair_cursor, &stop).unwrap();
            assert_eq!(first.repaired, MAX_REPAIRS_PER_BATCH);
            let pruned =
                prune_archived_message_batch(cx, pool, config, 3_000_000, &mut prune_cursor, &stop)
                    .unwrap();
            assert_eq!(pruned.pruned.deleted_messages, 4);
            assert_eq!(pruned.pruned.deleted_recipients, 8);
            assert_eq!(pruned.deferred, 3);
            assert_eq!(retained_message_ids(cx, pool).len(), 3);
            let rest =
                reconcile_message_batch(cx, pool, config, &mut repair_cursor, &stop).unwrap();
            assert_eq!(rest.repaired, 3);
            let pruned =
                prune_archived_message_batch(cx, pool, config, 3_000_000, &mut prune_cursor, &stop)
                    .unwrap();
            assert_eq!(pruned.pruned.deleted_messages, 3);
            assert_eq!(pruned.deferred, 0);
            assert!(retained_message_ids(cx, pool).is_empty());

            let archive = crate::open_archive(config, "project").unwrap().unwrap();
            for original in originals {
                assert!(
                    verify_retention_archive(&archive, &original, &mut 0)
                        .unwrap()
                        .is_some()
                );
                let paths = crate::message_paths_for_bundle(
                    &archive,
                    &original.message,
                    &original.sender,
                    &original.recipients,
                )
                .unwrap()
                .0;
                let (full, body) = read_surviving_message(&paths.canonical).unwrap().unwrap();
                assert_eq!(body, "Keep every body.");
                assert_eq!(full["reply_to"], 700);
                assert_eq!(full["topic"], "handoffs");
                assert_eq!(full["thread_id"], "retained-thread");
                assert_eq!(full["bcc"], json!(["RedFox"]));
                for inbox in paths.inbox {
                    let (message, _) = read_surviving_message(&inbox).unwrap().unwrap();
                    assert_eq!(message["bcc"], json!([]));
                    assert_eq!(message["reply_to"], 700);
                }
            }
        });
    }

    #[test]
    fn retention_requires_writable_source_committed_copies_and_intact_disk() {
        retention_fixture(1, |cx, pool, config| {
            let mut cursor = ArchivePruneCursor::default();
            let stopped = prune_archived_message_batch(
                cx,
                pool,
                config,
                3_000_000,
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
                prune_archived_message_batch(
                    cx,
                    &readonly,
                    config,
                    3_000_000,
                    &mut cursor,
                    &AtomicBool::new(false),
                )
                .unwrap_err()
                .contains("query-only snapshots")
            );
            drop(readonly);
            let original = prepare_message(cx, pool, 901).unwrap();
            let archive = crate::ensure_archive(config, "project").unwrap();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &original.message,
                &original.sender,
                &original.recipients,
            )
            .unwrap()
            .0;
            let full =
                crate::render_message_bundle_content(&original.message, &original.body).unwrap();
            let inbox = crate::render_message_bundle_content(
                &crate::redact_message_bcc_for_inbox(&original.message),
                &original.body,
            )
            .unwrap();
            for (path, bytes) in [&paths.canonical, &paths.outbox]
                .into_iter()
                .map(|path| (path, &full))
                .chain(paths.inbox.iter().map(|path| (path, &inbox)))
            {
                crate::ensure_parent_dir(path).unwrap();
                std::fs::write(path, bytes).unwrap();
            }
            let stop = AtomicBool::new(false);
            let uncommitted =
                prune_archived_message_batch(cx, pool, config, 3_000_000, &mut cursor, &stop)
                    .unwrap();
            assert_eq!(uncommitted.deferred, 1);
            assert_eq!(uncommitted.pruned.deleted_messages, 0);
            assert!(
                reconcile_prepared(config, &original)
                    .unwrap()
                    .git_commit_needed
            );

            let held = paths.inbox[0].with_extension("held");
            std::fs::rename(&paths.inbox[0], &held).unwrap();
            let missing =
                prune_archived_message_batch(cx, pool, config, 3_000_000, &mut cursor, &stop)
                    .unwrap();
            assert_eq!(missing.deferred, 1);
            assert_eq!(missing.pruned.deleted_messages, 0);
            std::fs::rename(&held, &paths.inbox[0]).unwrap();
            std::fs::write(&paths.outbox, "conflicting working evidence").unwrap();
            let conflict =
                prune_archived_message_batch(cx, pool, config, 3_000_000, &mut cursor, &stop)
                    .unwrap();
            assert_eq!(conflict.deferred, 1);
            assert_eq!(conflict.pruned.deleted_messages, 0);
            assert_eq!(retained_message_ids(cx, pool), [901]);
            std::fs::write(&paths.outbox, &full).unwrap();

            let mut used = super::super::MAX_BUNDLE_BYTES;
            assert!(
                verify_retention_archive(&archive, &original, &mut used)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(used, super::super::MAX_BUNDLE_BYTES);
            let complete =
                prune_archived_message_batch(cx, pool, config, 3_000_000, &mut cursor, &stop)
                    .unwrap();
            assert_eq!(complete.pruned.deleted_messages, 1);
            assert_eq!(complete.pruned.deleted_recipients, 2);
            assert_eq!(complete.deferred, 0);
        });
    }

    #[test]
    fn retention_preserves_source_until_referenced_attachment_is_durable() {
        use sha1::{Digest as _, Sha1};

        retention_fixture(1, |cx, pool, config| {
            let bytes = b"irreplaceable attachment";
            let relative = "projects/project/attachments/files/evidence.bin";
            let attachment = json!([{
                "type": "file", "path": relative, "bytes": bytes.len(),
                "sha1": hex::encode(Sha1::digest(bytes)),
            }]);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(&format!(
                "UPDATE messages SET attachments = '{attachment}' WHERE id = 901"
            ))
            .unwrap();
            drop(conn);
            let original = prepare_message(cx, pool, 901).unwrap();
            let archive = crate::ensure_archive(config, "project").unwrap();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &original.message,
                &original.sender,
                &original.recipients,
            )
            .unwrap()
            .0;
            let inbox = crate::redact_message_bcc_for_inbox(&original.message);
            commit_survivors(
                &archive,
                &[
                    (&paths.canonical, &original.message),
                    (&paths.outbox, &original.message),
                    (&paths.inbox[0], &inbox),
                    (&paths.inbox[1], &inbox),
                ],
                &original.body,
            );
            let path = archive.repo_root.join(relative);
            crate::ensure_parent_dir(&path).unwrap();
            std::fs::write(&path, bytes).unwrap();
            let mut cursor = ArchivePruneCursor::default();
            let stop = AtomicBool::new(false);
            let uncommitted =
                prune_archived_message_batch(cx, pool, config, 3_000_000, &mut cursor, &stop)
                    .unwrap();
            assert_eq!(uncommitted.pruned.deleted_messages, 0);
            assert_eq!(uncommitted.deferred, 1);
            assert!(
                reconcile_prepared(config, &original)
                    .unwrap()
                    .git_commit_needed
            );

            let held = path.with_extension("held");
            std::fs::rename(&path, &held).unwrap();
            let missing =
                prune_archived_message_batch(cx, pool, config, 3_000_000, &mut cursor, &stop)
                    .unwrap();
            assert_eq!(missing.pruned.deleted_messages, 0);
            assert_eq!(missing.deferred, 1);
            assert_eq!(retained_message_ids(cx, pool), [901]);
            std::fs::rename(&held, &path).unwrap();
            let complete =
                prune_archived_message_batch(cx, pool, config, 3_000_000, &mut cursor, &stop)
                    .unwrap();
            assert_eq!(complete.pruned.deleted_messages, 1);
            assert_eq!(complete.deferred, 0);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        });
    }

    #[test]
    fn real_file_backed_projection_repairs_without_modifying_mailbox_rows() {
        if admission_tests::isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let db_path = temp.path().join("mail.sqlite3");
            let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&db_path);
            let pool_config = mcp_agent_mail_db::DbPoolConfig {
                database_url: database_url.clone(),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            };
            let pool = mcp_agent_mail_db::create_pool(&pool_config).unwrap();
            let cx = Cx::for_testing();
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(101, 'project', '/project', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) \
                VALUES(101, 101, 'BlueLake', 'test', 'test', '', 1, 1), (102, 101, 'GreenStone', 'test', 'test', '', 1, 1)").unwrap();
            conn.execute_raw("INSERT INTO messages(id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
                VALUES(901, 101, 101, 'handoff', 'body', 'normal', 0, 1000000, '{\"to\":[\"GreenStone\"],\"cc\":[],\"bcc\":[]}', '[]')").unwrap();
            conn.execute_raw(
                "INSERT INTO message_recipients(message_id, agent_id, kind) VALUES(901, 102, 'to')",
            )
            .unwrap();
            let before = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901",
                    &[],
                )
                .unwrap();
            assert_eq!(before[0].get_named::<Option<i64>>("read_ts").unwrap(), None);
            assert_eq!(before[0].get_named::<Option<i64>>("ack_ts").unwrap(), None);
            drop(conn);
            // Source admission canonicalizes the archive root before checking
            // query-only mode. Match server initialization so this fixture
            // reaches the intended refusal instead of failing on a missing root.
            std::fs::create_dir_all(pool.storage_root()).unwrap();
            let config = Config {
                storage_root: pool.storage_root().to_path_buf(),
                database_url,
                ..Config::default()
            };
            let stop = AtomicBool::new(false);
            let readonly = DbPool::new_query_only(&pool_config).unwrap();
            let rejected = reconcile_message_batch(
                &cx,
                &readonly,
                &config,
                &mut ReconcileCursor::default(),
                &stop,
            )
            .unwrap_err();
            assert!(rejected.contains("query-only snapshots"), "{rejected}");
            drop(readonly);
            let mut cursor = ReconcileCursor::default();
            let report = reconcile_message_batch(&cx, &pool, &config, &mut cursor, &stop).unwrap();
            assert_eq!(report.scanned, 1);
            assert_eq!(report.repaired, 1);
            assert_eq!(report.files_created, 3);
            assert_eq!(report.deferred, 0);
            let again = reconcile_message_batch(
                &cx,
                &pool,
                &config,
                &mut ReconcileCursor::default(),
                &stop,
            )
            .unwrap();
            assert_eq!(again.repaired, 0);
            assert_eq!(again.unchanged, 1);
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            let after = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901",
                    &[],
                )
                .unwrap();
            assert_eq!(after[0].get_named::<Option<i64>>("read_ts").unwrap(), None);
            assert_eq!(after[0].get_named::<Option<i64>>("ack_ts").unwrap(), None);
            let rows = conn
                .query_sync("SELECT body_md FROM messages WHERE id = 901", &[])
                .unwrap();
            assert_eq!(rows[0].get_named::<String>("body_md").unwrap(), "body");
            drop(conn);
            stop.store(true, Ordering::Release);
            let stopped = reconcile_message_batch(&cx, &pool, &config, &mut cursor, &stop).unwrap();
            assert!(stopped.interrupted);
            assert_eq!(stopped.scanned, 0);
            let other = temp.path().join("not-the-live-db.sqlite3");
            std::fs::write(&other, b"preserve other database bytes").unwrap();
            let mut mismatched = config.clone();
            mismatched.database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&other);
            assert!(
                validate_pool_binding(&pool, &mismatched)
                    .unwrap_err()
                    .contains("not the configured live database")
            );
            assert_eq!(
                std::fs::read(&other).unwrap(),
                b"preserve other database bytes"
            );
        });
    }

    fn archive_fixture_range(cx: &Cx, pool: &DbPool, config: &Config, ids: std::ops::Range<i64>) {
        for id in ids {
            let prepared = prepare_message(cx, pool, id).unwrap();
            reconcile_prepared(config, &prepared).unwrap();
        }
    }

    #[test]
    fn startup_backfill_reaches_old_orphan_behind_healthy_pages_then_returns_to_idle() {
        retention_fixture(40, |cx, pool, config| {
            // The newest pages are completely healthy, but the oldest accepted
            // message never reached Git. No retry record knows about it yet.
            archive_fixture_range(cx, pool, config, 902..941);
            let stop = AtomicBool::new(false);
            let mut ordinary = ReconcileCursor::default();
            let ordinary_page =
                reconcile_message_batch(cx, pool, config, &mut ordinary, &stop).unwrap();
            assert_eq!(ordinary_page.unchanged, 16);
            assert!(!ordinary_page.budget_exhausted);

            let mut cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            for pass in 0..2 {
                let report = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
                assert_eq!(report.unchanged, 16, "pass {pass}: {report:?}");
                assert_eq!(report.repaired, 0);
                assert_eq!(report.deferred, 0);
                assert!(
                    report.budget_exhausted,
                    "healthy pages must not hide an older orphan"
                );
                assert_eq!(report.checkpoint_saved, Some(true));
            }
            let repaired = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(repaired.scanned, 8);
            assert_eq!(repaired.repaired, 1);
            assert_eq!(repaired.files_created, 4);
            assert_eq!(repaired.deferred, 0);
            assert!(repaired.scanned <= MAX_IDS_PER_PASS);
            assert!(repaired.repaired <= MAX_REPAIRS_PER_BATCH);
            assert!(repaired.payload_bytes <= MAX_BATCH_PAYLOAD_BYTES);
            let original = prepare_message(cx, pool, 901).unwrap();
            assert_eq!(
                reconcile_prepared(config, &original).unwrap(),
                ReconcileResult::default()
            );
            assert!(
                verify_retention_archive(
                    &crate::open_archive(config, "project").unwrap().unwrap(),
                    &original,
                    &mut 0
                )
                .unwrap()
                .is_some()
            );

            let end = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(end.scanned, 0);
            assert!(!end.budget_exhausted);
            assert!(cursor.startup_backfill_complete);
            // Ordinary rotating history continues, but does not keep the
            // worker on the fast startup cadence forever.
            let wrapped = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(wrapped.unchanged, 16);
            assert!(!wrapped.budget_exhausted);
            assert!(cursor.startup_backfill_complete);
            assert_eq!(
                retained_message_ids(cx, pool),
                (901..941).collect::<Vec<_>>()
            );
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let receipts = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901",
                    &[],
                )
                .unwrap();
            assert_eq!(receipts.len(), 2);
            for receipt in receipts {
                assert_eq!(receipt.get_named::<i64>("read_ts").unwrap(), 2_000_000);
                assert_eq!(receipt.get_named::<i64>("ack_ts").unwrap(), 2_500_000);
            }
        });
    }

    #[test]
    fn startup_backfill_resumes_persisted_position_instead_of_restarting_recent_pages() {
        retention_fixture(40, |cx, pool, config| {
            archive_fixture_range(cx, pool, config, 902..941);
            let stop = AtomicBool::new(false);
            let mut before_restart =
                ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            let first =
                reconcile_message_batch(cx, pool, config, &mut before_restart, &stop).unwrap();
            assert_eq!(first.unchanged, 16);
            assert_eq!(before_restart.backfill_ceiling, Some(924));
            assert_eq!(first.checkpoint_saved, Some(true));

            let mut restarted = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            let resumed = reconcile_message_batch(cx, pool, config, &mut restarted, &stop).unwrap();
            assert!(resumed.checkpoint_restored);
            assert_eq!(resumed.unchanged, 16);
            assert_eq!(restarted.backfill_ceiling, Some(908));
            assert!(resumed.budget_exhausted);
            assert!(!restarted.startup_backfill_complete);
            let next = reconcile_message_batch(cx, pool, config, &mut restarted, &stop).unwrap();
            assert_eq!(next.repaired, 1);
            assert_eq!(next.scanned, 8);
            assert_eq!(next.deferred, 0);
            let end = reconcile_message_batch(cx, pool, config, &mut restarted, &stop).unwrap();
            assert!(!end.budget_exhausted);
            assert!(restarted.startup_backfill_complete);
        });
    }

    #[test]
    fn startup_priority_has_a_fixed_bound_and_cannot_advance_over_young_tail_rows() {
        retention_fixture(40, |cx, pool, _config| {
            let anchor = 2_000_000;
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE messages SET created_ts = 2000001 WHERE id = 938")
                .unwrap();
            drop(conn);
            let mut cursor = ReconcileCursor::settled_before(anchor);
            cursor.source_identity = pool.sqlite_identity_key();
            cursor.tail_after = Some(905);
            cursor.backfill_ceiling = Some(904);
            let first = select_ids(cx, pool, &mut cursor, anchor).unwrap();
            assert_eq!(first.ids[0], (940, ScanLane::StartupTail));
            let mut recent = HashSet::new();
            for (id, lane) in first.ids {
                if lane == ScanLane::StartupTail {
                    assert!(recent.insert(id));
                    cursor.advance_scan(id, lane);
                }
            }
            // A late commit may carry a timestamp predating this process. It
            // must remain normal tail work, not extend the finite priority lane.
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts) \
                 VALUES (941, 101, 101, 'late commit', 'keep later mail', 1000000)",
            )
            .unwrap();
            drop(conn);
            for _ in 0..4 {
                for (id, lane) in select_ids(cx, pool, &mut cursor, anchor).unwrap().ids {
                    if lane == ScanLane::StartupTail {
                        assert!(recent.insert(id));
                        cursor.advance_scan(id, lane);
                    }
                }
            }
            assert!(cursor.startup_tail.is_none());
            assert_eq!(recent.len(), 34);
            assert!(!recent.contains(&938));
            assert!(!recent.contains(&941));
            assert_eq!(cursor.tail_after, Some(905));
            assert_eq!(cursor.backfill_ceiling, Some(904));

            let mut forward = HashSet::new();
            for _ in 0..4 {
                for (id, lane) in select_ids(cx, pool, &mut cursor, anchor + 1).unwrap().ids {
                    assert_ne!(lane, ScanLane::StartupTail);
                    if lane == ScanLane::Tail {
                        assert!(forward.insert(id));
                    }
                    cursor.advance_scan(id, lane);
                }
            }
            assert_eq!(forward, (906..942).collect());
            assert_eq!(cursor.tail_after, Some(941));
        });
    }

    #[test]
    fn forward_tail_lookahead_distinguishes_more_work_from_newest_and_exact_end_pages() {
        retention_fixture(32, |cx, pool, config| {
            archive_fixture_range(cx, pool, config, 901..933);
            let stop = AtomicBool::new(false);
            let mut newest = ReconcileCursor::default();
            let recent = reconcile_message_batch(cx, pool, config, &mut newest, &stop).unwrap();
            assert_eq!(recent.unchanged, 16);
            assert!(!recent.budget_exhausted);
            assert_eq!(newest.tail_after, Some(932));

            // An already-restored forward watermark has different semantics
            // from the initial newest-page query. History is at its end, so
            // only witnessed forward work may request the short follow-up.
            let mut cursor = ReconcileCursor {
                source_identity: pool.sqlite_identity_key(),
                tail_after: Some(900),
                backfill_ceiling: Some(900),
                ..ReconcileCursor::default()
            };
            let first = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(first.unchanged, 16);
            assert!(first.budget_exhausted);
            assert!(cursor.startup_backfill_complete);
            assert_eq!(cursor.tail_after, Some(916), "lookahead is not consumed");

            let exact_end = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(exact_end.unchanged, 16);
            assert_eq!(exact_end.repaired, 0);
            assert!(!exact_end.budget_exhausted);
            assert_eq!(cursor.tail_after, Some(932));
            assert!(exact_end.scanned <= MAX_IDS_PER_PASS);
            assert!(exact_end.payload_bytes <= MAX_BATCH_PAYLOAD_BYTES);
            assert_eq!(
                retained_message_ids(cx, pool),
                (901..933).collect::<Vec<_>>()
            );
        });
    }

    #[test]
    fn cooling_tail_page_advances_without_retrying_and_reaches_the_next_orphan() {
        retention_fixture(17, |cx, pool, config| {
            let stop = AtomicBool::new(false);
            let mut cursor = ReconcileCursor {
                source_identity: pool.sqlite_identity_key(),
                tail_after: Some(900),
                backfill_ceiling: Some(900),
                conflict_backoff: (901..917)
                    .map(|id| (id, Instant::now() + MAX_RETRY_BACKOFF))
                    .collect(),
                ..ReconcileCursor::default()
            };
            let cooling = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(cooling.scanned, 0);
            assert_eq!(cooling.repaired, 0);
            assert_eq!(cooling.deferred, 0);
            assert_eq!(cooling.retry_len, 0);
            assert!(cooling.budget_exhausted);
            assert_eq!(cursor.tail_after, Some(916));

            // The unconsumed sentinel is an actual DB-only message, not proof
            // of a repair. Its following pass still performs all live-source,
            // archive and Git checks before publication.
            let next = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(next.scanned, 1);
            assert_eq!(next.repaired, 1);
            assert_eq!(next.files_created, 4);
            assert_eq!(next.deferred, 0);
            assert!(!next.budget_exhausted);
            let original = prepare_message(cx, pool, 917).unwrap();
            assert!(
                verify_retention_archive(
                    &crate::open_archive(config, "project").unwrap().unwrap(),
                    &original,
                    &mut 0,
                )
                .unwrap()
                .is_some()
            );
            let idle = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(idle.scanned, 0);
            assert!(!idle.budget_exhausted);
            assert_eq!(
                retained_message_ids(cx, pool),
                (901..918).collect::<Vec<_>>()
            );
        });
    }

    #[test]
    fn forward_tail_lookahead_does_not_accelerate_cancelled_blocked_or_grace_only_passes() {
        retention_fixture(17, |cx, pool, config| {
            let mut cursor = ReconcileCursor {
                source_identity: pool.sqlite_identity_key(),
                tail_after: Some(900),
                backfill_ceiling: Some(900),
                retries: (0..MAX_RETRY_IDS)
                    .map(|index| DeferredRetry {
                        id: 10_000 + i64::try_from(index).unwrap(),
                        attempts: 1,
                        ready_at: Instant::now() + MAX_RETRY_BACKOFF,
                    })
                    .collect(),
                ..ReconcileCursor::default()
            };
            let cancelled =
                reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(true))
                    .unwrap();
            assert!(cancelled.interrupted);
            assert!(!cancelled.budget_exhausted);
            assert_eq!(cursor.tail_after, Some(900));

            let stop = AtomicBool::new(false);
            for _ in 0..2 {
                let blocked =
                    reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
                assert_eq!(blocked.scanned, 0);
                assert_eq!(blocked.repaired, 0);
                assert_eq!(blocked.retry_len, MAX_RETRY_IDS);
                assert!(!blocked.budget_exhausted);
                assert_eq!(cursor.tail_after, Some(900));
            }

            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw(&format!("UPDATE messages SET created_ts = {}", i64::MAX,))
                .unwrap();
            drop(conn);
            let mut grace_only = ReconcileCursor {
                source_identity: pool.sqlite_identity_key(),
                tail_after: Some(900),
                backfill_ceiling: Some(900),
                ..ReconcileCursor::default()
            };
            for _ in 0..2 {
                let shielded =
                    reconcile_message_batch(cx, pool, config, &mut grace_only, &stop).unwrap();
                assert_eq!(shielded.scanned, 0);
                assert_eq!(shielded.repaired, 0);
                assert!(!shielded.budget_exhausted);
                assert_eq!(grace_only.tail_after, Some(900));
            }
            assert_eq!(
                retained_message_ids(cx, pool),
                (901..918).collect::<Vec<_>>()
            );
        });
    }

    #[test]
    fn empty_and_stopped_startup_backfill_do_not_request_busy_polling() {
        retention_fixture(0, |cx, pool, config| {
            let mut cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            let stopped =
                reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(true))
                    .unwrap();
            assert!(stopped.interrupted);
            assert!(!stopped.budget_exhausted);
            assert!(!cursor.startup_backfill_complete);
            for _ in 0..3 {
                let empty =
                    reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                        .unwrap();
                assert_eq!(empty.scanned, 0);
                assert!(!empty.budget_exhausted);
                assert!(cursor.startup_backfill_complete);
            }
        });
    }
}
