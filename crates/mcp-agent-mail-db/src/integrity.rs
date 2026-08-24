//! `SQLite` integrity checking for corruption detection and recovery.
//!
//! Provides three levels of checking:
//!
//! 1. **Quick check** (`PRAGMA quick_check`): Fast subset of integrity checks.
//!    Run on pool initialization when `INTEGRITY_CHECK_ON_STARTUP=true`.
//!
//! 2. **Incremental check** (`PRAGMA integrity_check(1)`): First-error-only check.
//!    Suitable for periodic connection-recycle validation.
//!
//! 3. **Full check** (`PRAGMA integrity_check`): Complete scan of the database.
//!    Run on a background schedule (default: every 24 hours).
//!
//! When corruption is detected, the system:
//! - Logs a CRITICAL error with the raw check output.
//! - Returns an `IntegrityCorruption` error so callers can set health to Red.
//! - Optionally attempts recovery via checkpoint + `VACUUM` + validated file copy.

use crate::DbConn;
use crate::error::{DbError, DbResult};
use serde::{Deserialize, Serialize};
use sqlmodel_core::{Row, Value};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering};

/// Result of an integrity check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityCheckResult {
    /// Whether the check passed (no corruption detected).
    pub ok: bool,
    /// Raw output lines from the PRAGMA.
    pub details: Vec<String>,
    /// Duration of the check in microseconds.
    pub duration_us: u64,
    /// Which kind of check was run.
    pub kind: CheckKind,
}

/// The kind of integrity check that was run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckKind {
    /// `PRAGMA quick_check` — fast subset.
    Quick,
    /// `PRAGMA integrity_check(1)` — first error only.
    Incremental,
    /// `PRAGMA integrity_check` — full scan.
    Full,
}

impl CheckKind {
    const fn as_storage_u8(self) -> u8 {
        match self {
            Self::Quick => 1,
            Self::Incremental => 2,
            Self::Full => 3,
        }
    }

    const fn from_storage_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Quick),
            2 => Some(Self::Incremental),
            3 => Some(Self::Full),
            _ => None,
        }
    }
}

impl std::fmt::Display for CheckKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quick => write!(f, "quick_check"),
            Self::Incremental => write!(f, "integrity_check(1)"),
            Self::Full => write!(f, "integrity_check"),
        }
    }
}

/// Outcome of an integrity check retained for the current process lifetime.
///
/// A passing `quick_check` is weaker evidence than a complete
/// `integrity_check`; callers that need a durable health verdict must inspect
/// [`IntegrityMetrics::last_full_check_outcome`] separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityCheckOutcome {
    /// No check of this class has completed in this process.
    #[default]
    Unknown,
    /// The check completed without reporting corruption.
    Passed,
    /// The check reported corruption or returned no usable result rows.
    Failed,
}

impl IntegrityCheckOutcome {
    const fn as_storage_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Passed => 1,
            Self::Failed => 2,
        }
    }

    const fn from_storage_u8(value: u8) -> Self {
        match value {
            1 => Self::Passed,
            2 => Self::Failed,
            _ => Self::Unknown,
        }
    }
}

/// Global state tracking the last integrity check result.
static LAST_CHECK: OnceLock<IntegrityCheckState> = OnceLock::new();

#[derive(Debug)]
struct IntegrityCheckState {
    /// Timestamp (microseconds since epoch) of the last successful check.
    last_ok_ts: AtomicI64,
    /// Timestamp (microseconds since epoch) of the last check (success or fail).
    last_check_ts: AtomicI64,
    /// Timestamp (microseconds since epoch) of the last completed full check.
    last_full_check_ts: AtomicI64,
    /// Kind of the latest check. Zero denotes that no check has completed.
    last_check_kind: AtomicU8,
    /// Outcome of the latest check, independent of historical counters.
    last_check_outcome: AtomicU8,
    /// Outcome of the latest full `PRAGMA integrity_check`.
    ///
    /// This deliberately is not cleared by a later `quick_check` success: a
    /// weaker probe cannot attest that corruption found by the complete scan
    /// has been repaired.
    last_full_check_outcome: AtomicU8,
    /// Total number of checks run.
    checks_total: AtomicU64,
    /// Total number of failures detected.
    failures_total: AtomicU64,
    /// Number of failures observed *since the last clean check* — reset to 0
    /// on every passing check. Unlike `failures_total` (a monotonic lifetime
    /// tally), this reflects *current* integrity state: a daemon that saw a
    /// transient failure earlier but is healthy now reports 0 here even while
    /// `failures_total` stays elevated (#164).
    failures_since_last_ok: AtomicU64,
}

impl IntegrityCheckState {
    const fn new() -> Self {
        Self {
            last_ok_ts: AtomicI64::new(0),
            last_check_ts: AtomicI64::new(0),
            last_full_check_ts: AtomicI64::new(0),
            last_check_kind: AtomicU8::new(0),
            last_check_outcome: AtomicU8::new(IntegrityCheckOutcome::Unknown.as_storage_u8()),
            last_full_check_outcome: AtomicU8::new(IntegrityCheckOutcome::Unknown.as_storage_u8()),
            checks_total: AtomicU64::new(0),
            failures_total: AtomicU64::new(0),
            failures_since_last_ok: AtomicU64::new(0),
        }
    }
}

fn state() -> &'static IntegrityCheckState {
    LAST_CHECK.get_or_init(IntegrityCheckState::new)
}

/// Snapshot of integrity check metrics for health reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityMetrics {
    pub last_ok_ts: i64,
    pub last_check_ts: i64,
    /// Timestamp of the most recent complete `PRAGMA integrity_check`.
    #[serde(default)]
    pub last_full_check_ts: i64,
    /// The probe that produced `last_check_outcome`, if a probe has run.
    #[serde(default)]
    pub last_check_kind: Option<CheckKind>,
    /// Result of the most recently completed check of any kind.
    #[serde(default)]
    pub last_check_outcome: IntegrityCheckOutcome,
    /// Result of the most recently completed full `PRAGMA integrity_check`.
    ///
    /// This remains `failed` until another full check succeeds; a passing
    /// quick or incremental check cannot overwrite it.
    #[serde(default)]
    pub last_full_check_outcome: IntegrityCheckOutcome,
    pub checks_total: u64,
    /// Monotonic lifetime tally of failed checks since this process started.
    /// This NEVER decreases, so it does not reflect the *current* integrity
    /// verdict — read `failures_since_last_ok` for that (#164).
    pub failures_total: u64,
    /// Failures observed since the last clean check (reset to 0 on each pass).
    /// `0` here with `last_ok_ts == last_check_ts` means the DB is currently
    /// healthy regardless of how large `failures_total` is.
    #[serde(default)]
    pub failures_since_last_ok: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxIntegrityStatus {
    Healthy,
    Suspect,
    Broken,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxIntegrityVerdict {
    pub status: MailboxIntegrityStatus,
    pub metrics: IntegrityMetrics,
    pub check: Option<IntegrityCheckResult>,
    pub detail: String,
}

/// Get current integrity check metrics.
#[must_use]
pub fn integrity_metrics() -> IntegrityMetrics {
    let s = state();
    let runtime_failures = mcp_agent_mail_core::global_metrics()
        .db
        .integrity_failures_total
        .load();
    IntegrityMetrics {
        last_ok_ts: s.last_ok_ts.load(Ordering::Relaxed),
        last_check_ts: s.last_check_ts.load(Ordering::Relaxed),
        last_full_check_ts: s.last_full_check_ts.load(Ordering::Relaxed),
        last_check_kind: CheckKind::from_storage_u8(s.last_check_kind.load(Ordering::Relaxed)),
        last_check_outcome: IntegrityCheckOutcome::from_storage_u8(
            s.last_check_outcome.load(Ordering::Relaxed),
        ),
        last_full_check_outcome: IntegrityCheckOutcome::from_storage_u8(
            s.last_full_check_outcome.load(Ordering::Relaxed),
        ),
        checks_total: s.checks_total.load(Ordering::Relaxed),
        failures_total: s
            .failures_total
            .load(Ordering::Relaxed)
            .saturating_add(runtime_failures),
        failures_since_last_ok: s.failures_since_last_ok.load(Ordering::Relaxed),
    }
}

#[must_use]
pub fn inspect_mailbox_integrity(db_path: &Path, kind: CheckKind) -> MailboxIntegrityVerdict {
    let metrics_before = integrity_metrics();
    if db_path.as_os_str() == ":memory:" {
        return MailboxIntegrityVerdict {
            status: MailboxIntegrityStatus::Skipped,
            metrics: metrics_before,
            check: None,
            detail: "In-memory database (integrity check skipped)".to_string(),
        };
    }

    // `DbConn::open_file` opens SQLite with `SQLITE_OPEN_CREATE`, which would
    // silently materialize an empty DB stub for a missing mailbox.  Integrity
    // inspection is read-only diagnostics — refuse gracefully so external
    // callers of this `pub` surface can't inadvertently leave behind a
    // zero-byte `.sqlite3` file.
    if !db_path.exists() {
        return MailboxIntegrityVerdict {
            status: MailboxIntegrityStatus::Skipped,
            metrics: metrics_before,
            check: None,
            detail: format!("Database file not found: {}", db_path.display()),
        };
    }

    let conn = match crate::pool::open_guarded_read_only_sqlite_file(
        db_path,
        "mailbox integrity diagnostic",
    ) {
        Ok(conn) => conn,
        Err(error) => {
            return MailboxIntegrityVerdict {
                status: MailboxIntegrityStatus::Broken,
                metrics: metrics_before,
                check: None,
                detail: format!("Cannot open database for integrity check: {error}"),
            };
        }
    };
    let conn = crate::guard_db_conn(conn, "mailbox integrity diagnostic");

    match run_check(&conn, kind) {
        Ok(check) => MailboxIntegrityVerdict {
            status: MailboxIntegrityStatus::Healthy,
            metrics: integrity_metrics(),
            detail: format!("{} passed", check.kind),
            check: Some(check),
        },
        Err(DbError::IntegrityCorruption { details, .. })
            if integrity_details_are_suspect(&details) =>
        {
            MailboxIntegrityVerdict {
                status: MailboxIntegrityStatus::Suspect,
                metrics: integrity_metrics(),
                detail: format!(
                    "{} reported benign/suspect findings: {}",
                    kind,
                    details.join("; ")
                ),
                check: Some(IntegrityCheckResult {
                    ok: false,
                    details,
                    duration_us: 0,
                    kind,
                }),
            }
        }
        Err(error) => MailboxIntegrityVerdict {
            status: MailboxIntegrityStatus::Broken,
            metrics: integrity_metrics(),
            check: None,
            detail: error.to_string(),
        },
    }
}

/// GH#247: how many unused-page rows (`Page N: never used` / `... unused`)
/// an integrity check may report before the "benign freelist/accounting
/// slack" classification stops applying.
///
/// A handful of unaccounted pages is the known benign residual class (engine
/// page-accounting slack after crash recovery). But `PRAGMA integrity_check`
/// reports each orphaned page as its own row, so whole-file page loss looks
/// like the SAME class at a vastly larger magnitude — fleet incidents showed
/// 59–72% of all pages orphaned while the bare (100-error-capped) check
/// reported exactly 100 `never used` rows that this classifier then waved
/// through as benign. Bounding the class by row count keeps the false-positive
/// tolerance for genuine slack while refusing to call mass page loss healthy.
pub const BENIGN_UNUSED_PAGE_ROW_LIMIT: usize = 16;

/// GH#247: explicit error-row ceiling for the full `PRAGMA integrity_check`.
///
/// The bare pragma stops after 100 errors by default, which both under-reports
/// severity and lets magnitude-sensitive classifiers
/// ([`integrity_details_are_suspect`]) misread whole-file page loss as a
/// small benign residual. Every full check issued through this module passes
/// this limit explicitly.
pub const INTEGRITY_CHECK_MAX_ERROR_ROWS: usize = 1_000_000;

#[must_use]
pub fn integrity_details_are_suspect(details: &[String]) -> bool {
    if details.is_empty() {
        return false;
    }
    let mut unused_page_rows = 0_usize;
    for detail in details {
        let trimmed = detail.trim();
        let lower = trimmed.to_ascii_lowercase();
        // Section headers like "*** in database main ***" carry no verdict.
        if lower == "ok"
            || lower.contains("wal without shm")
            || (trimmed.starts_with("***") && trimmed.ends_with("***"))
        {
            continue;
        }
        if lower.contains("never used") || lower.contains("unused") {
            unused_page_rows += 1;
            continue;
        }
        return false;
    }
    // GH#247: the unused-page class is only benign in small numbers. Beyond
    // the slack limit it is page loss and must be treated as corruption.
    unused_page_rows <= BENIGN_UNUSED_PAGE_ROW_LIMIT
}

/// br-mdfpz: names of damaged indexes when EVERY integrity-check detail row
/// is index-level-only damage that a plain `REINDEX` can rebuild.
///
/// This is the class behind the 2026-08-12 csd incident, where two corrupt
/// `file_reservations` indexes routed startup into a nine-day archive
/// reconstruction crash loop that a seconds-long `REINDEX` would have healed
/// (the table b-trees were intact throughout).
///
/// Returns `Some(index_names)` only when at least one row is index-class
/// damage (`wrong # of entries in index <name>` or `row <N> missing from
/// index <name>`) and every other row is either the `*** in database ... ***`
/// section header or a benign finding (freelist/sidecar slack, per
/// [`integrity_details_are_suspect`]'s classes). Any other row — page-level
/// damage, fragmentation accounting, b-tree errors — disqualifies the fast
/// path and `None` is returned so callers escalate to repair/reconstruct.
///
/// The returned names are **diagnostic witnesses, not a complete repair
/// target list**. SQLite's checker can identify one damaged secondary index
/// while a sibling index on the same table also needs rebuilding. A repair
/// caller must resolve each name to its owner table and issue `REINDEX` for
/// every distinct owner table, which rebuilds all of that table's indexes.
#[must_use]
pub fn index_only_corruption_index_names(details: &[String]) -> Option<Vec<String>> {
    fn index_name_from_detail(detail: &str) -> Option<&str> {
        let trimmed = detail.trim();
        if let Some(name) = trimmed.strip_prefix("wrong # of entries in index ") {
            return Some(name.trim());
        }
        if trimmed.starts_with("row ")
            && let Some(pos) = trimmed.find(" missing from index ")
        {
            let name = trimmed[pos + " missing from index ".len()..].trim();
            // The prefix between "row " and the marker must be a bare rowid;
            // anything else is a message we did not anticipate.
            let rowid = &trimmed["row ".len()..pos];
            if !rowid.is_empty() && rowid.chars().all(|c| c.is_ascii_digit()) {
                return Some(name);
            }
        }
        None
    }

    fn detail_is_unused_page_row(detail: &str) -> bool {
        let lower = detail.trim().to_ascii_lowercase();
        lower.contains("never used") || lower.contains("unused")
    }

    fn detail_is_ignorable(detail: &str) -> bool {
        let trimmed = detail.trim();
        let lower = trimmed.to_ascii_lowercase();
        // Section headers like "*** in database main ***" carry no verdict.
        (trimmed.starts_with("***") && trimmed.ends_with("***"))
            || lower == "ok"
            || lower.contains("never used")
            || lower.contains("unused")
            || lower.contains("wal without shm")
    }

    let mut names: Vec<String> = Vec::new();
    let mut unused_page_rows = 0_usize;
    for detail in details {
        if let Some(name) = index_name_from_detail(detail) {
            if name.is_empty() {
                return None;
            }
            if !names.iter().any(|existing| existing == name) {
                names.push(name.to_string());
            }
        } else if detail_is_ignorable(detail) {
            // GH#247: unused-page rows are only ignorable as small freelist
            // slack. Mass page loss alongside index damage must escalate to
            // repair/reconstruct, never the REINDEX fast path.
            if detail_is_unused_page_row(detail) {
                unused_page_rows += 1;
                if unused_page_rows > BENIGN_UNUSED_PAGE_ROW_LIMIT {
                    return None;
                }
            }
        } else {
            return None;
        }
    }
    if names.is_empty() { None } else { Some(names) }
}

/// Run `PRAGMA quick_check` on an open connection.
///
/// This is fast (typically <100ms) and catches most common corruption.
/// Suitable for startup validation.
pub fn quick_check(conn: &DbConn) -> DbResult<IntegrityCheckResult> {
    run_check(conn, CheckKind::Quick)
}

/// Run `PRAGMA integrity_check(1)` — stops after the first error.
///
/// Faster than a full check but provides less detail. Suitable for
/// periodic connection-recycle checks.
pub fn incremental_check(conn: &DbConn) -> DbResult<IntegrityCheckResult> {
    run_check(conn, CheckKind::Incremental)
}

/// Run a full `PRAGMA integrity_check`.
///
/// This scans the entire database and can take seconds on large databases.
/// Run on a dedicated connection, not from the pool hot path.
pub fn full_check(conn: &DbConn) -> DbResult<IntegrityCheckResult> {
    run_check(conn, CheckKind::Full)
}

#[must_use]
pub const fn preferred_check_sql(kind: CheckKind) -> &'static str {
    check_sql_candidates(kind)[0]
}

#[must_use]
pub const fn fallback_check_sql(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::Quick => "PRAGMA quick_check",
        CheckKind::Incremental => "PRAGMA integrity_check(1)",
        CheckKind::Full => "PRAGMA integrity_check(1000000)",
    }
}

/// Candidate SQL forms for one integrity probe, in preference order.
///
/// GH#247: the full check passes an explicit error-row limit
/// ([`INTEGRITY_CHECK_MAX_ERROR_ROWS`]) because the bare pragma caps at 100
/// errors, which masked whole-file page loss (100 reported rows vs 59–72% of
/// all pages actually orphaned) behind the "small benign residual" class. The
/// bare forms remain as final fallbacks for engines that reject the argument.
#[must_use]
pub const fn check_sql_candidates(kind: CheckKind) -> &'static [&'static str] {
    match kind {
        CheckKind::Quick => &[
            "SELECT quick_check FROM pragma_quick_check()",
            "PRAGMA quick_check",
        ],
        CheckKind::Incremental => &[
            "SELECT integrity_check FROM pragma_integrity_check() LIMIT 1",
            "PRAGMA integrity_check(1)",
        ],
        CheckKind::Full => &[
            "SELECT integrity_check FROM pragma_integrity_check(1000000)",
            "PRAGMA integrity_check(1000000)",
            "SELECT integrity_check FROM pragma_integrity_check()",
            "PRAGMA integrity_check",
        ],
    }
}

/// Run an integrity probe across the candidate SQL forms for `kind`.
///
/// Best-effort fallback order: table-valued pragma first, then the classic
/// PRAGMA statement; for the full check the explicitly-uncapped forms come
/// before the bare 100-error-capped legacy forms.
///
/// Some SQLite-compatible engines in this workspace still do not implement the
/// table-valued `pragma_*` functions or pragma arguments. Callers should use
/// this helper instead of hardcoding the preferred SQL so runtime probes,
/// doctor flows, and recovery all behave consistently.
pub fn probe_check_rows<F>(mut query: F, kind: CheckKind) -> Result<Vec<Row>, String>
where
    F: FnMut(&str) -> Result<Vec<Row>, String>,
{
    let candidates = check_sql_candidates(kind);
    let mut errors: Vec<String> = Vec::new();
    let mut empty_ok = false;
    for sql in candidates {
        match query(sql) {
            Ok(rows) if !rows.is_empty() => return Ok(rows),
            Ok(_) => {
                // An empty result is not authoritative (a healthy check always
                // yields at least the "ok" row); remember it and try the next
                // form, but treat it as success if nothing better exists.
                empty_ok = true;
            }
            Err(error) => errors.push(format!("`{sql}`: {error}")),
        }
    }
    if empty_ok {
        return Ok(Vec::new());
    }
    Err(format!(
        "every {kind} probe form failed — {}",
        errors.join("; ")
    ))
}

/// Compact, bounded rendering of integrity-check detail rows.
///
/// An uncapped full check (GH#247) can return hundreds of thousands of rows;
/// joining them verbatim into logs or verdict strings would flood journals
/// and truncate away the magnitude, which is the most important signal.
/// Shows the first few rows plus an explicit total and unused-page-row count.
#[must_use]
pub fn summarize_check_details(details: &[String]) -> String {
    /// Repetitive unused-page rows shown before eliding.
    const SHOWN_UNUSED: usize = 5;
    /// Distinctive (non-unused-page) rows shown before eliding. Generous so
    /// class-signature substrings (e.g. the known `NOCASE` index-order false
    /// positive that [`crate::pool`] greps from the message) survive the
    /// summary for realistic outputs.
    const SHOWN_DISTINCTIVE: usize = 50;

    if details.len() <= SHOWN_UNUSED {
        return details.join("; ");
    }
    let mut shown: Vec<&str> = Vec::new();
    let mut unused_page_rows = 0_usize;
    let mut distinctive_rows = 0_usize;
    for detail in details {
        let lower = detail.to_ascii_lowercase();
        if lower.contains("never used") || lower.contains("unused") {
            unused_page_rows += 1;
            if unused_page_rows <= SHOWN_UNUSED {
                shown.push(detail);
            }
        } else {
            distinctive_rows += 1;
            if distinctive_rows <= SHOWN_DISTINCTIVE {
                shown.push(detail);
            }
        }
    }
    let elided = details.len() - shown.len();
    if elided == 0 {
        return shown.join("; ");
    }
    format!(
        "{}; … {} more row(s) elided ({} unused-page row(s) of {} total)",
        shown.join("; "),
        elided,
        unused_page_rows,
        details.len()
    )
}

#[must_use]
pub fn extract_check_details(rows: &[Row], kind: CheckKind) -> Vec<String> {
    let primary_column = match kind {
        CheckKind::Quick => "quick_check",
        CheckKind::Incremental | CheckKind::Full => "integrity_check",
    };

    let mut details: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            if let Some(Value::Text(text)) = row.get_by_name(primary_column) {
                Some(text.clone())
            } else if let Some(Value::Text(text)) = row.get_by_name("integrity_check") {
                Some(text.clone())
            } else if let Some(Value::Text(text)) = row.get_by_name("quick_check") {
                Some(text.clone())
            } else if let Some(Value::Text(text)) = row.values().next() {
                Some(text.clone())
            } else {
                None
            }
        })
        // GH#247: some drivers return the ENTIRE check output as one
        // newline-joined text value. Before this split, a single row holding
        // "*** in database main ***\nPage 2: never used\n…\nPage 137: never
        // used" counted as ONE detail — so magnitude-sensitive classifiers
        // ([`integrity_details_are_suspect`]) saw one benign-looking row no
        // matter how many pages were lost, and a multi-line row mixing benign
        // and corruption text could match a benign substring class. Normalize
        // to one detail per reported line so every consumer sees the same
        // per-finding granularity.
        .flat_map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();

    if details.is_empty() {
        if rows.is_empty() {
            details.push(format!("{kind} returned no rows"));
        } else {
            details.push(format!(
                "{kind} returned {} row(s) but none had extractable text values",
                rows.len()
            ));
        }
    }

    details
}

#[must_use]
pub fn details_indicate_ok(details: &[String]) -> bool {
    details.len() == 1 && details[0].trim().eq_ignore_ascii_case("ok")
}

fn run_check(conn: &DbConn, kind: CheckKind) -> DbResult<IntegrityCheckResult> {
    let start = std::time::Instant::now();
    let rows: Vec<Row> = probe_check_rows(
        |sql| conn.query_sync(sql, &[]).map_err(|error| error.to_string()),
        kind,
    )
    .map_err(|error| DbError::Sqlite(format!("{kind} failed: {error}")))?;

    let duration_us =
        u64::try_from(start.elapsed().as_micros().min(u128::from(u64::MAX))).unwrap_or(u64::MAX);

    evaluate_check_rows(&rows, kind, duration_us)
}

/// Evaluate integrity/quick-check pragma rows and update global integrity metrics.
///
/// Shared helper to keep integrity semantics consistent across all callers.
pub fn evaluate_check_rows(
    rows: &[Row],
    kind: CheckKind,
    duration_us: u64,
) -> DbResult<IntegrityCheckResult> {
    let details = extract_check_details(rows, kind);
    let ok = details_indicate_ok(&details);

    // Update global state.
    let s = state();
    let now = crate::now_micros();
    s.last_check_ts.store(now, Ordering::Relaxed);
    s.last_check_kind
        .store(kind.as_storage_u8(), Ordering::Relaxed);
    let outcome = if ok {
        IntegrityCheckOutcome::Passed
    } else {
        IntegrityCheckOutcome::Failed
    };
    s.last_check_outcome
        .store(outcome.as_storage_u8(), Ordering::Relaxed);
    if kind == CheckKind::Full {
        s.last_full_check_ts.store(now, Ordering::Relaxed);
        s.last_full_check_outcome
            .store(outcome.as_storage_u8(), Ordering::Relaxed);
    }
    s.checks_total.fetch_add(1, Ordering::Relaxed);
    if ok {
        s.last_ok_ts.store(now, Ordering::Relaxed);
        // #164: a clean check means the DB is healthy *now*. Reset the
        // since-last-ok counter so `failures_since_last_ok` reflects current
        // state (0), while `failures_total` stays as the lifetime trend tally.
        s.failures_since_last_ok.store(0, Ordering::Relaxed);
        // K3 (br-bvq1x.11.3): a clean integrity check means the database is
        // healthy again, so clear the corruption circuit breaker (self-heal
        // after `am doctor repair`/`reconstruct`) and let writes resume.
        crate::corruption_circuit_breaker().reset();
    } else {
        s.failures_total.fetch_add(1, Ordering::Relaxed);
        s.failures_since_last_ok.fetch_add(1, Ordering::Relaxed);
        // A5 (br-bvq1x.1.5): record the corruption-class detection keyed by
        // probe source so operators get trend visibility, not a one-shot scare.
        let source = match kind {
            CheckKind::Quick => mcp_agent_mail_core::CorruptionDetectionSource::QuickCheck,
            CheckKind::Incremental | CheckKind::Full => {
                mcp_agent_mail_core::CorruptionDetectionSource::IntegrityCheck
            }
        };
        mcp_agent_mail_core::global_metrics()
            .corruption
            .record_detection(source);
    }

    let result = IntegrityCheckResult {
        ok,
        details,
        duration_us,
        kind,
    };

    if !ok {
        return Err(DbError::IntegrityCorruption {
            message: format!(
                "{kind} detected corruption ({duration_us}us): {}",
                summarize_check_details(&result.details)
            ),
            details: result.details,
        });
    }

    Ok(result)
}

/// Attempt recovery by checkpointing then copying the database file.
///
/// Returns the path of the clean copy on success.
pub fn attempt_vacuum_recovery(conn: &DbConn, original_path: &str) -> DbResult<String> {
    let recovery_path = format!("{original_path}.recovery");

    // Remove any leftover recovery file.
    cleanup_recovery_artifacts(&recovery_path);

    // Use PASSIVE checkpoint to flush what we can without modifying the
    // corrupt database aggressively. TRUNCATE could propagate WAL-resident
    // corruption into the main file. VACUUM on a corrupt DB risks partial
    // overwrite of the original before failing.
    let _ = conn.query_sync("PRAGMA wal_checkpoint(PASSIVE)", &[]);

    // Copy the database file as-is (preserving corruption evidence).
    std::fs::copy(original_path, &recovery_path)
        .map_err(|e| DbError::Sqlite(format!("copy recovery failed: {e}")))?;
    // Also copy WAL/SHM so the recovery copy has the full state.
    // These are best-effort (the files may not exist in rollback-journal mode),
    // but failures are logged because a missing WAL means recent writes are lost.
    if let Err(e) = std::fs::copy(
        format!("{original_path}-wal"),
        format!("{recovery_path}-wal"),
    ) {
        tracing::warn!(path = %original_path, error = %e, "WAL copy failed during recovery — recent writes may be missing");
    }
    if let Err(e) = std::fs::copy(
        format!("{original_path}-shm"),
        format!("{recovery_path}-shm"),
    ) {
        tracing::debug!(path = %original_path, error = %e, "SHM copy skipped during recovery");
    }

    // Verify the recovery copy is valid.
    let recovery_conn = DbConn::open_file(&recovery_path).map_err(|e| {
        cleanup_recovery_artifacts(&recovery_path);
        DbError::Sqlite(format!("failed to open recovery copy: {e}"))
    })?;

    match quick_check(&recovery_conn) {
        Ok(_) => Ok(recovery_path),
        Err(e) => {
            cleanup_recovery_artifacts(&recovery_path);
            Err(DbError::Internal(format!(
                "recovery copy also corrupt: {e}"
            )))
        }
    }
}

fn cleanup_recovery_artifacts(recovery_path: &str) {
    let _ = std::fs::remove_file(recovery_path);
    let _ = std::fs::remove_file(format!("{recovery_path}-wal"));
    let _ = std::fs::remove_file(format!("{recovery_path}-shm"));
}

/// Check whether enough time has elapsed since the last full check
/// to warrant running another one.
///
/// Returns `true` if `interval_hours` have elapsed since the last full check,
/// or if no full check has ever been run.
#[must_use]
pub fn is_full_check_due(interval_hours: u64) -> bool {
    if interval_hours == 0 {
        return false;
    }
    let s = state();
    let last = s.last_full_check_ts.load(Ordering::Relaxed);
    if last == 0 {
        return true;
    }
    let now = crate::now_micros();
    let elapsed_hours = u64::try_from((now - last).max(0)).unwrap_or(0) / (3_600 * 1_000_000);
    elapsed_hours >= interval_hours
}

/// One index-vs-table row-count disagreement found by
/// [`index_table_cross_count`] (GH#214 desync class).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossCountMismatch {
    pub table: String,
    pub index: String,
    /// Rows visible through the table btree (`SELECT count(*) ... NOT INDEXED`).
    pub table_rows: i64,
    /// Rows visible through the named index (`... INDEXED BY "<index>" WHERE 1`).
    pub index_rows: i64,
}

impl CrossCountMismatch {
    /// Render the mismatch as a corruption-classifiable message.
    ///
    /// The leading `database disk image is malformed` phrase is deliberate: it
    /// routes the finding through the same corruption classification
    /// (`error::is_corruption_error`) and recovery flow as a failing
    /// `PRAGMA integrity_check`, so a desynced-but-PRAGMA-green database
    /// (the GH#213 Linux presentation) is treated as the corruption it is.
    #[must_use]
    pub fn as_corruption_message(&self) -> String {
        format!(
            "database disk image is malformed: index \"{}\" returns {} rows but table \"{}\" holds {} rows (GH#214 index/table cross-count desync)",
            self.index, self.index_rows, self.table, self.table_rows
        )
    }
}

/// Arithmetic tripwire for the index/table desync class that `PRAGMA
/// quick_check` can miss at the single-row stage (GH#213/GH#214).
///
/// For each named table that exists, counts the rows through the table btree
/// (`NOT INDEXED`) and through every eligible named index (`INDEXED BY
/// "<index>" WHERE 1` — the `WHERE 1` defeats the count(*) covering-index
/// optimization, which would otherwise resolve the hint and then discard it,
/// silently comparing an index to itself; measured by the GH#213 reporter).
/// Any disagreement is returned as a [`CrossCountMismatch`].
///
/// Skipped, by design:
/// - missing tables (fresh/partial schemas are not findings);
/// - `sqlite_autoindex_*` (cannot be named in `INDEXED BY`; UNIQUE-constraint
///   coverage belongs to per-row point lookups);
/// - partial indexes (`CREATE INDEX ... WHERE ...` legitimately holds fewer
///   entries than the table);
/// - indexes whose forced probe errors (engine probe limitations are not
///   corruption evidence; the error is reported via `tracing` only).
///
/// Honest scope note: this catches the loud desync class (index and table
/// btrees disagree). It cannot see the GH#213 Windows silent-loss class,
/// where table and indexes are mutually consistent but acknowledged rows are
/// absent — only a client-side acknowledgement ledger sees that.
pub fn index_table_cross_count(
    conn: &DbConn,
    tables: &[&str],
) -> DbResult<Vec<CrossCountMismatch>> {
    let mut mismatches = Vec::new();
    for table in tables {
        let exists_rows = conn
            .query_sync(
                "SELECT count(*) AS c FROM sqlite_master WHERE type = 'table' AND name = ?",
                &[Value::Text((*table).to_string())],
            )
            .map_err(|error| DbError::Sqlite(format!("cross-count table probe failed: {error}")))?;
        let exists: i64 = exists_rows
            .first()
            .and_then(|row| row.get_named("c").ok())
            .unwrap_or(0);
        if exists == 0 {
            continue;
        }

        let table_sql = format!("SELECT count(*) AS c FROM \"{table}\" NOT INDEXED");
        let table_rows: i64 = conn
            .query_sync(&table_sql, &[])
            .map_err(|error| {
                DbError::Sqlite(format!(
                    "cross-count NOT INDEXED scan of {table} failed: {error}"
                ))
            })?
            .first()
            .and_then(|row| row.get_named("c").ok())
            .unwrap_or(0);

        let index_rows = conn
            .query_sync(
                "SELECT name, sql FROM sqlite_master WHERE type = 'index' AND tbl_name = ? ORDER BY name",
                &[Value::Text((*table).to_string())],
            )
            .map_err(|error| DbError::Sqlite(format!("cross-count index list failed: {error}")))?;
        for row in &index_rows {
            let Ok(index) = row.get_named::<String>("name") else {
                continue;
            };
            if index.starts_with("sqlite_autoindex_") {
                continue;
            }
            // Partial indexes legitimately hold fewer entries than the table.
            let create_sql = row.get_named::<String>("sql").unwrap_or_default();
            if create_sql.to_ascii_uppercase().contains("WHERE") {
                continue;
            }
            let forced_sql =
                format!("SELECT count(*) AS c FROM \"{table}\" INDEXED BY \"{index}\" WHERE 1");
            match conn.query_sync(&forced_sql, &[]) {
                Ok(rows) => {
                    let forced: i64 = rows
                        .first()
                        .and_then(|row| row.get_named("c").ok())
                        .unwrap_or(0);
                    if forced != table_rows {
                        mismatches.push(CrossCountMismatch {
                            table: (*table).to_string(),
                            index,
                            table_rows,
                            index_rows: forced,
                        });
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        table,
                        index,
                        %error,
                        "cross-count forced-index probe failed; skipping index (probe limitation, not corruption evidence)"
                    );
                }
            }
        }
    }
    Ok(mismatches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    static TEST_STATE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn open_test_db() -> DbConn {
        let conn = DbConn::open_memory().expect("open memory db");
        conn.execute_raw("CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT)")
            .expect("create table");
        conn
    }

    fn set_state_for_tests(
        last_ok_ts: i64,
        last_check_ts: i64,
        last_full_check_ts: i64,
        checks_total: u64,
        failures_total: u64,
    ) {
        let s = state();
        s.last_ok_ts.store(last_ok_ts, Ordering::Relaxed);
        s.last_check_ts.store(last_check_ts, Ordering::Relaxed);
        s.last_full_check_ts
            .store(last_full_check_ts, Ordering::Relaxed);
        s.last_check_kind.store(0, Ordering::Relaxed);
        s.last_check_outcome.store(
            IntegrityCheckOutcome::Unknown.as_storage_u8(),
            Ordering::Relaxed,
        );
        s.last_full_check_outcome.store(
            IntegrityCheckOutcome::Unknown.as_storage_u8(),
            Ordering::Relaxed,
        );
        s.checks_total.store(checks_total, Ordering::Relaxed);
        s.failures_total.store(failures_total, Ordering::Relaxed);
        s.failures_since_last_ok
            .store(failures_total, Ordering::Relaxed);
    }

    #[test]
    fn clean_check_resets_failures_since_last_ok_but_keeps_lifetime_total() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        // Simulate a process that saw transient failures earlier.
        set_state_for_tests(0, 0, 0, 10, 7);
        assert_eq!(integrity_metrics().failures_since_last_ok, 7);

        // A subsequent clean check on a healthy DB.
        let conn = open_test_db();
        let result = quick_check(&conn).expect("quick check should run");
        assert!(result.ok, "healthy DB should pass quick_check");

        let m = integrity_metrics();
        assert_eq!(
            m.failures_since_last_ok, 0,
            "a clean check must reset the since-last-ok counter (#164)"
        );
        assert!(
            m.failures_total >= 7,
            "lifetime tally must NOT decrease on a clean check (got {})",
            m.failures_total
        );
        assert_eq!(
            m.last_ok_ts, m.last_check_ts,
            "last_ok_ts should advance to last_check_ts on a clean check"
        );
    }

    #[test]
    fn failed_check_increments_both_total_and_since_last_ok() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        set_state_for_tests(0, 0, 0, 0, 0);
        // Drive a failing evaluation directly (empty rows == corruption verdict).
        let _ = evaluate_check_rows(&[], CheckKind::Quick, 0);
        let m = integrity_metrics();
        assert!(
            m.failures_since_last_ok >= 1,
            "a failed check must increment failures_since_last_ok"
        );
        assert!(m.failures_total >= 1);
    }

    #[test]
    fn quick_check_cannot_clear_a_failed_full_check_outcome() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        set_state_for_tests(0, 0, 0, 0, 0);

        let _ = evaluate_check_rows(&[], CheckKind::Full, 0)
            .expect_err("empty full-check rows must record a failed full outcome");
        let failed = integrity_metrics();
        assert_eq!(
            failed.last_full_check_outcome,
            IntegrityCheckOutcome::Failed
        );

        let conn = open_test_db();
        quick_check(&conn).expect("healthy quick_check should pass");
        let after_quick = integrity_metrics();
        assert_eq!(after_quick.last_check_kind, Some(CheckKind::Quick));
        assert_eq!(
            after_quick.last_check_outcome,
            IntegrityCheckOutcome::Passed
        );
        assert_eq!(
            after_quick.last_full_check_outcome,
            IntegrityCheckOutcome::Failed,
            "a weaker quick_check must not clear failed full-check evidence"
        );

        full_check(&conn).expect("healthy full check should pass");
        assert_eq!(
            integrity_metrics().last_full_check_outcome,
            IntegrityCheckOutcome::Passed,
            "only a succeeding full check clears failed full-check evidence"
        );
    }

    #[test]
    fn quick_check_passes_on_healthy_db() {
        let conn = open_test_db();
        let result = quick_check(&conn).expect("quick_check should pass");
        assert!(result.ok);
        assert_eq!(result.details, vec!["ok"]);
        assert_eq!(result.kind, CheckKind::Quick);
        assert!(result.duration_us < 1_000_000); // < 1s
    }

    #[test]
    fn incremental_check_passes_on_healthy_db() {
        let conn = open_test_db();
        let result = incremental_check(&conn).expect("incremental check should pass");
        assert!(result.ok);
        assert_eq!(result.details, vec!["ok"]);
        assert_eq!(result.kind, CheckKind::Incremental);
    }

    #[test]
    fn full_check_passes_on_healthy_db() {
        let conn = open_test_db();
        let result = full_check(&conn).expect("full check should pass");
        assert!(result.ok);
        assert_eq!(result.details, vec!["ok"]);
        assert_eq!(result.kind, CheckKind::Full);
    }

    #[test]
    fn check_kind_display() {
        assert_eq!(CheckKind::Quick.to_string(), "quick_check");
        assert_eq!(CheckKind::Incremental.to_string(), "integrity_check(1)");
        assert_eq!(CheckKind::Full.to_string(), "integrity_check");
    }

    #[test]
    fn integrity_metrics_tracks_checks() {
        let conn = open_test_db();
        let before = integrity_metrics();
        let before_total = before.checks_total;

        let _ = quick_check(&conn);
        let _ = full_check(&conn);

        let after = integrity_metrics();
        assert!(
            after.checks_total >= before_total + 2,
            "checks_total should increase by at least 2"
        );
        assert!(after.last_ok_ts > 0, "last_ok_ts should be set");
        assert!(after.last_check_ts > 0, "last_check_ts should be set");
    }

    #[test]
    fn is_full_check_due_when_never_run() {
        // This test checks the logic; the global state may have been
        // modified by other tests, but interval=0 should always be false.
        assert!(!is_full_check_due(0), "interval=0 means disabled");
    }

    #[test]
    fn integrity_metrics_serializable() {
        let m = integrity_metrics();
        let json = serde_json::to_value(&m).expect("serialize IntegrityMetrics");
        assert!(json.get("last_ok_ts").is_some());
        assert!(json.get("last_check_ts").is_some());
        assert!(json.get("last_full_check_ts").is_some());
        assert!(json.get("last_check_kind").is_some());
        assert!(json.get("last_check_outcome").is_some());
        assert!(json.get("last_full_check_outcome").is_some());
        assert!(json.get("checks_total").is_some());
        assert!(json.get("failures_total").is_some());
    }

    #[test]
    fn check_kind_equality() {
        assert_eq!(CheckKind::Quick, CheckKind::Quick);
        assert_ne!(CheckKind::Quick, CheckKind::Incremental);
        assert_ne!(CheckKind::Incremental, CheckKind::Full);
    }

    #[test]
    fn integrity_check_result_clone() {
        let conn = open_test_db();
        let result = quick_check(&conn).expect("quick_check");
        let cloned = result.clone();
        assert_eq!(cloned.ok, result.ok);
        assert_eq!(cloned.details, result.details);
        assert_eq!(cloned.kind, result.kind);
    }

    #[test]
    fn extract_check_details_empty_rows_are_not_ok() {
        let details = extract_check_details(&[], CheckKind::Quick);
        assert_eq!(details, vec!["quick_check returned no rows"]);
        assert!(
            !details_indicate_ok(&details),
            "empty integrity probes must not be normalized to success"
        );
    }

    #[test]
    fn evaluate_check_rows_empty_rows_reports_corruption() {
        let err = evaluate_check_rows(&[], CheckKind::Incremental, 0)
            .expect_err("empty integrity rows should fail");
        assert!(
            err.to_string().contains("returned no rows"),
            "unexpected empty-row verdict: {err}"
        );
    }

    #[test]
    fn probe_check_rows_falls_back_after_preferred_error() {
        let mut calls = Vec::new();
        let rows = probe_check_rows(
            |sql| {
                calls.push(sql.to_string());
                if sql == preferred_check_sql(CheckKind::Quick) {
                    Err("table-valued pragma unsupported".to_string())
                } else {
                    Ok(Vec::new())
                }
            },
            CheckKind::Quick,
        )
        .expect("fallback probe should succeed");

        assert!(
            rows.is_empty(),
            "test fallback intentionally returns empty rows"
        );
        assert_eq!(
            calls,
            vec![
                preferred_check_sql(CheckKind::Quick).to_string(),
                fallback_check_sql(CheckKind::Quick).to_string()
            ]
        );
    }

    #[test]
    fn probe_check_rows_falls_back_after_empty_preferred_rows() {
        let mut calls = Vec::new();
        let rows = probe_check_rows(
            |sql| {
                calls.push(sql.to_string());
                Ok(Vec::new())
            },
            CheckKind::Incremental,
        )
        .expect("fallback probe should succeed");

        assert!(
            rows.is_empty(),
            "test fallback intentionally returns empty rows"
        );
        assert_eq!(
            calls,
            vec![
                preferred_check_sql(CheckKind::Incremental).to_string(),
                fallback_check_sql(CheckKind::Incremental).to_string()
            ]
        );
    }

    #[test]
    fn full_check_candidates_are_explicitly_uncapped_first() {
        // GH#247: the bare `PRAGMA integrity_check` caps at 100 error rows,
        // which masked 59–72% whole-file page loss as a small benign residual.
        // Every preferred full-check form must carry an explicit high limit.
        let candidates = check_sql_candidates(CheckKind::Full);
        assert!(candidates[0].contains("pragma_integrity_check(1000000)"));
        assert!(candidates[1].contains("integrity_check(1000000)"));
        assert_eq!(preferred_check_sql(CheckKind::Full), candidates[0]);
        assert_eq!(
            fallback_check_sql(CheckKind::Full),
            "PRAGMA integrity_check(1000000)"
        );
    }

    #[test]
    fn probe_check_rows_full_falls_back_to_bare_forms_when_arg_unsupported() {
        // An engine that rejects the pragma argument must still be probed via
        // the legacy bare forms rather than erroring out entirely.
        let mut calls = Vec::new();
        let rows = probe_check_rows(
            |sql| {
                calls.push(sql.to_string());
                if sql.contains("1000000") {
                    Err("integrity_check does not accept arguments".to_string())
                } else {
                    Ok(vec![Row::new(
                        vec!["integrity_check".to_string()],
                        vec![Value::Text("ok".to_string())],
                    )])
                }
            },
            CheckKind::Full,
        )
        .expect("bare-form fallback should succeed");
        assert_eq!(rows.len(), 1);
        assert_eq!(calls.len(), 3, "two uncapped forms then first bare form");
    }

    #[test]
    fn extract_check_details_splits_newline_joined_driver_output() {
        // GH#247: the canonical driver can return the whole integrity_check
        // output as ONE multi-line text value. It must be normalized to one
        // detail per line, or magnitude-sensitive classifiers see a single
        // "row" no matter how many pages were lost.
        let joined = "*** in database main ***\nPage 2: never used\nPage 3: never used";
        let rows = vec![Row::new(
            vec!["integrity_check".to_string()],
            vec![Value::Text(joined.to_string())],
        )];
        let details = extract_check_details(&rows, CheckKind::Full);
        assert_eq!(details.len(), 3);
        assert_eq!(details[0], "*** in database main ***");
        assert_eq!(details[2], "Page 3: never used");
    }

    #[test]
    fn suspect_class_ignores_section_headers() {
        let details = vec![
            "*** in database main ***".to_string(),
            "Page 5: never used".to_string(),
        ];
        assert!(
            integrity_details_are_suspect(&details),
            "the database-section header carries no verdict of its own"
        );
    }

    #[test]
    fn suspect_class_accepts_small_unused_page_slack() {
        let details: Vec<String> = (1..=BENIGN_UNUSED_PAGE_ROW_LIMIT)
            .map(|page| format!("Page {page}: never used"))
            .collect();
        assert!(
            integrity_details_are_suspect(&details),
            "a small unused-page residual stays the benign/suspect class"
        );
    }

    #[test]
    fn suspect_class_rejects_mass_unused_page_loss() {
        // GH#247 regression: the capped 100-row output of a 59–72%-orphaned DB
        // was classified benign, so `am doctor triage` reported ok/0 findings
        // on a file stock SQLite calls malformed. Magnitude must disqualify
        // the benign class.
        let details: Vec<String> = (1..=100)
            .map(|page| format!("Page {page}: never used"))
            .collect();
        assert!(
            !integrity_details_are_suspect(&details),
            "mass unused-page loss must be treated as corruption, not slack"
        );
    }

    #[test]
    fn index_only_fast_path_disqualified_by_mass_unused_pages() {
        let mut details: Vec<String> = (1..=(BENIGN_UNUSED_PAGE_ROW_LIMIT + 1))
            .map(|page| format!("Page {page}: never used"))
            .collect();
        details.push("wrong # of entries in index idx_agents_name".to_string());
        assert!(
            index_only_corruption_index_names(&details).is_none(),
            "mass page loss must escalate to repair/reconstruct, never REINDEX"
        );
    }

    #[test]
    fn summarize_check_details_bounds_output_and_reports_magnitude() {
        let details: Vec<String> = (1..=250)
            .map(|page| format!("Page {page}: never used"))
            .collect();
        let summary = summarize_check_details(&details);
        assert!(summary.len() < 400, "summary must stay bounded: {summary}");
        assert!(summary.contains("245 more row(s) elided"));
        assert!(summary.contains("250 unused-page row(s) of 250 total"));
        let short = vec!["ok".to_string()];
        assert_eq!(summarize_check_details(&short), "ok");
    }

    #[test]
    fn summarize_check_details_keeps_distinctive_rows_over_unused_noise() {
        // A class-signature row (e.g. the NOCASE index-order false positive)
        // must survive the summary even when buried under unused-page noise,
        // because `reconcile_with_canonical` classifies from the message.
        let mut details: Vec<String> = (1..=200)
            .map(|page| format!("Page {page}: never used"))
            .collect();
        details.push(
            "entries are out of order for their declared key directions \
             in index idx_agents_project_name_nocase"
                .to_string(),
        );
        let summary = summarize_check_details(&details);
        assert!(
            summary.contains("nocase"),
            "distinctive row elided: {summary}"
        );
    }

    #[test]
    fn is_full_check_due_zero_interval_always_false() {
        // Regardless of state, interval=0 means disabled
        assert!(!is_full_check_due(0));
    }

    #[test]
    fn integrity_check_result_debug() {
        let result = IntegrityCheckResult {
            ok: true,
            details: vec!["ok".to_string()],
            duration_us: 42,
            kind: CheckKind::Quick,
        };
        let debug = format!("{result:?}");
        assert!(debug.contains("ok: true"));
        assert!(debug.contains("Quick"));
    }

    #[test]
    fn quick_and_incremental_both_pass_on_same_db() {
        let conn = open_test_db();
        // Insert some data
        conn.execute_raw("INSERT INTO test (id, name) VALUES (1, 'alpha')")
            .expect("insert");
        conn.execute_raw("INSERT INTO test (id, name) VALUES (2, 'beta')")
            .expect("insert");

        let qr = quick_check(&conn).expect("quick_check");
        assert!(qr.ok);
        let ir = incremental_check(&conn).expect("incremental_check");
        assert!(ir.ok);
        let fr = full_check(&conn).expect("full_check");
        assert!(fr.ok);
    }

    // ── br-3h13: Additional integrity.rs test coverage ─────────────

    #[test]
    fn quick_check_with_populated_db() {
        let conn = open_test_db();
        for i in 0..100 {
            conn.execute_raw(&format!(
                "INSERT INTO test (id, name) VALUES ({i}, 'item{i}')"
            ))
            .expect("insert");
        }
        let result = quick_check(&conn).expect("quick_check on populated DB");
        assert!(result.ok);
        assert_eq!(result.details, vec!["ok"]);
    }

    #[test]
    fn full_check_with_multiple_tables() {
        let conn = open_test_db();
        conn.execute_raw("CREATE TABLE other (val REAL)")
            .expect("create other");
        conn.execute_raw("INSERT INTO other VALUES (3.14)")
            .expect("insert");
        let result = full_check(&conn).expect("full check with multiple tables");
        assert!(result.ok);
    }

    #[test]
    fn integrity_metrics_failures_start_at_zero_or_above() {
        let m = integrity_metrics();
        // failures_total is cumulative from all tests, but should be non-negative
        assert!(m.failures_total < 1000, "unexpected failure count");
    }

    #[test]
    fn integrity_check_result_debug_with_failure_details() {
        let result = IntegrityCheckResult {
            ok: false,
            details: vec![
                "*** in database main ***".to_string(),
                "row 5 missing from index idx_test_name".to_string(),
            ],
            duration_us: 12345,
            kind: CheckKind::Full,
        };
        let debug = format!("{result:?}");
        assert!(debug.contains("ok: false"));
        assert!(debug.contains("Full"));
        assert!(debug.contains("12345"));
    }

    // ── br-mdfpz: index-only corruption classifier ─────────────────

    #[test]
    fn index_only_classifier_accepts_pure_index_damage() {
        // The exact shape from the 2026-08-12 csd incident: two damaged
        // file_reservations indexes, nothing else.
        let details = vec![
            "wrong # of entries in index idx_file_reservations_project_agent_released".to_string(),
            "wrong # of entries in index idx_file_reservations_project_released_expires"
                .to_string(),
        ];
        let names = index_only_corruption_index_names(&details).expect("index-only class");
        assert_eq!(
            names,
            vec![
                "idx_file_reservations_project_agent_released",
                "idx_file_reservations_project_released_expires"
            ]
        );
    }

    #[test]
    fn index_only_classifier_accepts_missing_rows_and_headers_and_dedups() {
        let details = vec![
            "*** in database main ***".to_string(),
            "row 21 missing from index idx_r_pa".to_string(),
            "row 35 missing from index idx_r_pa".to_string(),
            "wrong # of entries in index idx_r_pa".to_string(),
        ];
        let names = index_only_corruption_index_names(&details).expect("index-only class");
        assert_eq!(names, vec!["idx_r_pa"]);
    }

    #[test]
    fn index_only_classifier_rejects_mixed_damage() {
        // A fragmentation/page row alongside index rows must disqualify —
        // REINDEX cannot be assumed to fix page-level accounting damage.
        let details = vec![
            "Fragmentation of 33 bytes reported as 0 on page 3".to_string(),
            "wrong # of entries in index idx_r_pa".to_string(),
        ];
        assert!(index_only_corruption_index_names(&details).is_none());
    }

    #[test]
    fn index_only_classifier_rejects_non_index_and_healthy_inputs() {
        for details in [
            vec!["ok".to_string()],
            vec![],
            vec!["database disk image is malformed".to_string()],
            vec!["Page 5 is never used".to_string()], // benign-only: nothing to reindex
            vec!["row X missing from index idx_r".to_string()], // non-numeric rowid form
        ] {
            assert!(
                index_only_corruption_index_names(&details).is_none(),
                "should reject: {details:?}"
            );
        }
    }

    #[test]
    fn index_only_classifier_tolerates_benign_rows_alongside_index_damage() {
        let details = vec![
            "wrong # of entries in index idx_r_pa".to_string(),
            "Page 7 is never used".to_string(),
        ];
        let names = index_only_corruption_index_names(&details).expect("benign rows tolerated");
        assert_eq!(names, vec!["idx_r_pa"]);
    }

    #[test]
    fn check_kind_all_display_values_are_distinct() {
        let displays: Vec<String> = [CheckKind::Quick, CheckKind::Incremental, CheckKind::Full]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(displays.len(), 3);
        // All must be unique
        let mut sorted = displays;
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            3,
            "all CheckKind display values must be distinct"
        );
    }

    #[test]
    fn integrity_metrics_serde_has_all_fields() {
        let m = integrity_metrics();
        let json = serde_json::to_value(&m).expect("serialize");
        let obj = json.as_object().expect("should be object");
        assert_eq!(
            obj.len(),
            9,
            "IntegrityMetrics should have exactly 9 fields"
        );
        for key in &[
            "last_ok_ts",
            "last_check_ts",
            "last_full_check_ts",
            "last_check_kind",
            "last_check_outcome",
            "last_full_check_outcome",
            "checks_total",
            "failures_total",
            "failures_since_last_ok",
        ] {
            assert!(obj.contains_key(*key), "missing field: {key}");
        }
    }

    #[test]
    fn is_full_check_due_with_large_interval_is_false_after_recent_check() {
        // Run a full check to update last_full_check_ts to now.
        let conn = open_test_db();
        let _ = full_check(&conn);
        // interval of 1 billion hours should NOT be due
        assert!(!is_full_check_due(1_000_000_000));
    }

    #[test]
    fn is_full_check_due_ignores_recent_non_full_checks() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        let now = crate::now_micros();
        set_state_for_tests(now, now, now - 25 * 3_600 * 1_000_000, 10, 0);
        assert!(
            is_full_check_due(24),
            "recent quick/incremental checks must not hide an overdue full scan"
        );
    }

    #[test]
    fn integrity_metrics_include_runtime_corruption_failures() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        let metrics = mcp_agent_mail_core::global_metrics();
        let runtime_before = metrics.db.integrity_failures_total.load();
        let s = state();
        let state_before = (
            s.last_ok_ts.load(Ordering::Relaxed),
            s.last_check_ts.load(Ordering::Relaxed),
            s.last_full_check_ts.load(Ordering::Relaxed),
            s.checks_total.load(Ordering::Relaxed),
            s.failures_total.load(Ordering::Relaxed),
        );

        set_state_for_tests(0, 0, 0, 0, 0);
        metrics
            .db
            .integrity_failures_total
            .store(runtime_before.saturating_add(1));

        let snapshot = integrity_metrics();
        assert_eq!(
            snapshot.failures_total,
            runtime_before.saturating_add(1),
            "runtime corruption failures should surface in integrity metrics"
        );

        metrics.db.integrity_failures_total.store(runtime_before);
        set_state_for_tests(
            state_before.0,
            state_before.1,
            state_before.2,
            state_before.3,
            state_before.4,
        );
    }

    #[test]
    fn integrity_metrics_add_runtime_and_pragma_failures() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        let metrics = mcp_agent_mail_core::global_metrics();
        let runtime_before = metrics.db.integrity_failures_total.load();
        let s = state();
        let state_before = (
            s.last_ok_ts.load(Ordering::Relaxed),
            s.last_check_ts.load(Ordering::Relaxed),
            s.last_full_check_ts.load(Ordering::Relaxed),
            s.checks_total.load(Ordering::Relaxed),
            s.failures_total.load(Ordering::Relaxed),
        );

        set_state_for_tests(0, 0, 0, 0, 3);
        metrics
            .db
            .integrity_failures_total
            .store(runtime_before.saturating_add(7));

        let snapshot = integrity_metrics();
        assert_eq!(
            snapshot.failures_total,
            runtime_before.saturating_add(10),
            "integrity metrics should include both PRAGMA-detected and runtime failures"
        );

        metrics.db.integrity_failures_total.store(runtime_before);
        set_state_for_tests(
            state_before.0,
            state_before.1,
            state_before.2,
            state_before.3,
            state_before.4,
        );
    }

    #[test]
    fn integrity_check_result_clone_preserves_all_fields() {
        let original = IntegrityCheckResult {
            ok: false,
            details: vec!["error1".into(), "error2".into()],
            duration_us: 99999,
            kind: CheckKind::Incremental,
        };
        let cloned = original.clone();
        assert!(!cloned.ok);
        assert_eq!(cloned.details.len(), 2);
        // Use original after clone to prove independent copy.
        assert!(!original.ok);
        assert_eq!(cloned.details[0], "error1");
        assert_eq!(cloned.duration_us, 99999);
        assert_eq!(cloned.kind, CheckKind::Incremental);
    }

    #[test]
    fn vacuum_recovery_on_healthy_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let db_str = db_path.to_str().expect("path str");

        let conn = DbConn::open_file(db_str).expect("open db");
        conn.execute_raw("CREATE TABLE foo (id INTEGER PRIMARY KEY)")
            .expect("create table");
        conn.execute_raw("INSERT INTO foo VALUES (1)")
            .expect("insert");

        let recovery_path = attempt_vacuum_recovery(&conn, db_str).expect("vacuum recovery");
        assert!(
            std::path::Path::new(&recovery_path).exists(),
            "recovery file should exist"
        );

        // Verify recovery copy has data.
        let recovery_conn = DbConn::open_file(&recovery_path).expect("open recovery");
        let rows: Vec<Row> = recovery_conn
            .query_sync("SELECT COUNT(*) AS cnt FROM foo", &[])
            .expect("query");
        let cnt = rows
            .first()
            .and_then(|r| match r.get_by_name("cnt") {
                Some(Value::BigInt(n)) => Some(*n),
                Some(Value::Int(n)) => Some(i64::from(*n)),
                _ => None,
            })
            .unwrap_or(0);
        assert_eq!(cnt, 1, "recovery copy should have the data");
    }

    #[test]
    fn cleanup_recovery_artifacts_removes_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recovery = dir.path().join("test.db.recovery");
        std::fs::write(&recovery, b"db").expect("write recovery db");
        std::fs::write(format!("{}-wal", recovery.display()), b"wal").expect("write recovery wal");
        std::fs::write(format!("{}-shm", recovery.display()), b"shm").expect("write recovery shm");

        cleanup_recovery_artifacts(recovery.to_str().expect("recovery path"));

        assert!(!recovery.exists(), "recovery db should be removed");
        assert!(
            !dir.path().join("test.db.recovery-wal").exists(),
            "recovery wal should be removed"
        );
        assert!(
            !dir.path().join("test.db.recovery-shm").exists(),
            "recovery shm should be removed"
        );
    }

    #[test]
    fn cross_count_is_clean_on_a_healthy_table() {
        let conn = DbConn::open_memory().expect("open memory db");
        conn.execute_raw("CREATE TABLE cc (id INTEGER PRIMARY KEY, name TEXT, other TEXT)")
            .expect("create table");
        conn.execute_raw("CREATE INDEX idx_cc_name ON cc(name)")
            .expect("create index");
        conn.execute_raw("CREATE INDEX idx_cc_other ON cc(other)")
            .expect("create second index");
        for i in 0..3 {
            conn.execute_raw(&format!(
                "INSERT INTO cc (name, other) VALUES ('n{i}', 'o{i}')"
            ))
            .expect("insert row");
        }

        let mismatches = index_table_cross_count(&conn, &["cc"]).expect("cross count");
        assert!(
            mismatches.is_empty(),
            "healthy table must not report desync: {mismatches:?}"
        );
    }

    #[test]
    fn cross_count_skips_missing_tables() {
        let conn = DbConn::open_memory().expect("open memory db");
        let mismatches = index_table_cross_count(&conn, &["does_not_exist"]).expect("cross count");
        assert!(mismatches.is_empty(), "missing table is not a finding");
    }

    #[test]
    fn cross_count_skips_partial_indexes() {
        let conn = DbConn::open_memory().expect("open memory db");
        conn.execute_raw("CREATE TABLE ccp (id INTEGER PRIMARY KEY, flag INTEGER)")
            .expect("create table");
        // A partial index legitimately holds fewer entries than the table;
        // it must never be reported as a desync. If this engine build does
        // not support partial indexes, the skip path is moot — bail out.
        if conn
            .execute_raw("CREATE INDEX idx_ccp_flag ON ccp(flag) WHERE flag = 1")
            .is_err()
        {
            return;
        }
        conn.execute_raw("INSERT INTO ccp (flag) VALUES (1)")
            .expect("insert matching row");
        conn.execute_raw("INSERT INTO ccp (flag) VALUES (0)")
            .expect("insert non-matching row");

        let mismatches = index_table_cross_count(&conn, &["ccp"]).expect("cross count");
        assert!(
            mismatches.is_empty(),
            "partial index must be skipped, not reported: {mismatches:?}"
        );
    }

    #[test]
    fn cross_count_mismatch_message_classifies_as_corruption() {
        let mismatch = CrossCountMismatch {
            table: "agents".to_string(),
            index: "idx_agents_last_active_id_desc".to_string(),
            table_rows: 5,
            index_rows: 7,
        };
        let message = mismatch.as_corruption_message();
        assert!(
            crate::error::is_corruption_error(&message),
            "cross-count message must route through the corruption class: {message}"
        );
        assert!(message.contains("idx_agents_last_active_id_desc"));
        assert!(message.contains("GH#214"));
    }
}
