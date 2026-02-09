//! Query tracking and instrumentation for MCP Agent Mail.
//!
//! Provides lightweight counters for total queries, per-table breakdowns,
//! and a capped slow-query log. Mirrors the Python `QueryTracker`.
//!
//! ## Lock-Free Design
//!
//! The hot path (`record`) uses only atomic operations:
//! - `AtomicU64` for total query count and cumulative duration
//! - `[AtomicU64; TableId::COUNT]` array for per-table counters
//! - Fast keyword-based table extraction (no regex on hot path)
//!
//! The `OrderedMutex` is only acquired for slow-query logging (rare cold path).

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use mcp_agent_mail_core::{LockLevel, OrderedMutex};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Maximum number of slow queries retained in the log.
const SLOW_QUERY_LIMIT: usize = 50;

/// Compiled table extraction patterns (built once, reused).
/// Used only for slow-query logging and the legacy `extract_table()` API.
static TABLE_PATTERNS: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        Regex::new(r#"(?i)\binsert\s+(?:or\s+\w+\s+)?into\s+([\w.`"\[\]]+)"#).unwrap(),
        Regex::new(r#"(?i)\bupdate\s+([\w.`"\[\]]+)"#).unwrap(),
        Regex::new(r#"(?i)\bfrom\s+([\w.`"\[\]]+)"#).unwrap(),
    ]
});

// =============================================================================
// TableId — known table enumeration for lock-free counting
// =============================================================================

/// Known database tables for O(1) atomic counter indexing.
///
/// Each variant maps to a slot in the `per_table: [AtomicU64; COUNT]` array.
/// `Unknown` captures queries against unrecognized tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TableId {
    Projects = 0,
    Products = 1,
    ProductProjectLinks = 2,
    Agents = 3,
    Messages = 4,
    MessageRecipients = 5,
    FileReservations = 6,
    AgentLinks = 7,
    ProjectSiblingSuggestions = 8,
    FtsMessages = 9,
    Unknown = 10,
}

impl TableId {
    /// Total number of variants (for array sizing).
    pub const COUNT: usize = 11;

    /// Human-readable table name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Projects => "projects",
            Self::Products => "products",
            Self::ProductProjectLinks => "product_project_links",
            Self::Agents => "agents",
            Self::Messages => "messages",
            Self::MessageRecipients => "message_recipients",
            Self::FileReservations => "file_reservations",
            Self::AgentLinks => "agent_links",
            Self::ProjectSiblingSuggestions => "project_sibling_suggestions",
            Self::FtsMessages => "fts_messages",
            Self::Unknown => "unknown",
        }
    }

    /// Convert array index back to `TableId`.
    #[must_use]
    pub const fn from_index(i: usize) -> Self {
        match i {
            0 => Self::Projects,
            1 => Self::Products,
            2 => Self::ProductProjectLinks,
            3 => Self::Agents,
            4 => Self::Messages,
            5 => Self::MessageRecipients,
            6 => Self::FileReservations,
            7 => Self::AgentLinks,
            8 => Self::ProjectSiblingSuggestions,
            9 => Self::FtsMessages,
            _ => Self::Unknown,
        }
    }
}

/// Match a lowercase table name to a known `TableId`.
fn match_known_table_lower(name: &[u8]) -> TableId {
    // Ordered by expected query frequency (messages/agents most common).
    match name {
        b"messages" => TableId::Messages,
        b"agents" => TableId::Agents,
        b"message_recipients" => TableId::MessageRecipients,
        b"projects" => TableId::Projects,
        b"file_reservations" => TableId::FileReservations,
        b"agent_links" => TableId::AgentLinks,
        b"fts_messages" => TableId::FtsMessages,
        b"products" => TableId::Products,
        b"product_project_links" => TableId::ProductProjectLinks,
        b"project_sibling_suggestions" => TableId::ProjectSiblingSuggestions,
        _ => TableId::Unknown,
    }
}

// =============================================================================
// Fast table extraction (no regex, no allocation)
// =============================================================================

/// Extract the `TableId` from a SQL statement using fast keyword scanning.
///
/// Scans for `INTO`, `UPDATE`, and `FROM` keywords (case-insensitive) in
/// priority order, then matches the extracted table name against known tables.
///
/// This is the hot-path replacement for `extract_table()` — no regex, no
/// heap allocation.
fn extract_table_id(sql: &str) -> TableId {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    if len < 6 {
        return TableId::Unknown;
    }

    // Track the earliest keyword match position to preserve priority
    // (INSERT INTO > UPDATE > FROM, naturally by position).
    let mut best_pos = usize::MAX;
    let mut best_name_start = 0usize;

    // Scan for " INTO " or "\nINTO " or "\tINTO " (case-insensitive)
    let mut i = 1; // INTO always preceded by whitespace
    while i + 5 <= len {
        if is_ws(bytes[i - 1]) && ci_eq4(bytes, i, *b"into") && i + 4 < len && is_ws(bytes[i + 4]) {
            let ns = skip_ws(bytes, i + 5);
            if ns < len && i < best_pos {
                best_pos = i;
                best_name_start = ns;
            }
            break; // INTO is highest priority, no need to continue
        }
        i += 1;
    }

    // Scan for "UPDATE " at start or after whitespace
    i = 0;
    while i + 7 <= len {
        if (i == 0 || is_ws(bytes[i - 1])) && ci_eq_n(bytes, i, b"update") && is_ws(bytes[i + 6]) {
            let ns = skip_ws(bytes, i + 7);
            if ns < len && i < best_pos {
                best_pos = i;
                best_name_start = ns;
            }
            break;
        }
        i += 1;
    }

    // Scan for " FROM " (case-insensitive)
    i = 1;
    while i + 5 <= len {
        if is_ws(bytes[i - 1]) && ci_eq4(bytes, i, *b"from") && i + 4 < len && is_ws(bytes[i + 4]) {
            let ns = skip_ws(bytes, i + 5);
            if ns < len && i < best_pos {
                best_pos = i;
                best_name_start = ns;
            }
            break; // Take the first FROM occurrence
        }
        i += 1;
    }

    if best_pos == usize::MAX {
        return TableId::Unknown;
    }

    // Skip quote characters at start
    let start = skip_quotes_at(bytes, best_name_start);
    if start >= len {
        return TableId::Unknown;
    }

    // Extract the table name word (lowercase it in-place on the stack)
    let mut buf = [0u8; 64]; // known table names are all < 64 bytes
    let mut bi = 0;

    // Handle schema-qualified: skip to the part after the last dot
    let qname_end = find_qname_end(bytes, start);
    let last_segment_start = find_last_segment(bytes, start, qname_end);
    let mut si = skip_quotes_at(bytes, last_segment_start);

    while si < qname_end && bi < buf.len() {
        let b = bytes[si];
        if is_ident_char(b) {
            buf[bi] = b.to_ascii_lowercase();
            bi += 1;
        } else if is_quote_char(b) {
            // skip quote chars
        } else {
            break;
        }
        si += 1;
    }

    match_known_table_lower(&buf[..bi])
}

/// Check if byte is ASCII whitespace (space, tab, newline, carriage return).
#[inline]
const fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Check if byte is a quote character.
#[inline]
const fn is_quote_char(b: u8) -> bool {
    matches!(b, b'`' | b'"' | b'[' | b']')
}

/// Check if byte is valid in a table identifier.
#[inline]
const fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Case-insensitive 4-byte match. `keyword` must be lowercase (e.g., `b"from"`).
#[inline]
fn ci_eq4(bytes: &[u8], pos: usize, keyword: [u8; 4]) -> bool {
    bytes[pos].to_ascii_lowercase() == keyword[0]
        && bytes[pos + 1].to_ascii_lowercase() == keyword[1]
        && bytes[pos + 2].to_ascii_lowercase() == keyword[2]
        && bytes[pos + 3].to_ascii_lowercase() == keyword[3]
}

/// Case-insensitive N-byte match. `keyword` must be lowercase.
#[inline]
fn ci_eq_n(bytes: &[u8], pos: usize, keyword: &[u8]) -> bool {
    for (i, &k) in keyword.iter().enumerate() {
        if bytes[pos + i].to_ascii_lowercase() != k {
            return false;
        }
    }
    true
}

/// Skip whitespace starting at `pos`, return the position of the first non-ws byte.
#[inline]
fn skip_ws(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() && is_ws(bytes[pos]) {
        pos += 1;
    }
    pos
}

/// Skip quote characters at `pos`.
#[inline]
fn skip_quotes_at(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() && is_quote_char(bytes[pos]) {
        pos += 1;
    }
    pos
}

/// Find the end of a qualified name (identifiers, dots, and quotes).
fn find_qname_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len()
        && (is_ident_char(bytes[i]) || bytes[i] == b'.' || is_quote_char(bytes[i]))
    {
        i += 1;
    }
    i
}

/// Find the start of the last segment after the last dot in a qualified name.
fn find_last_segment(bytes: &[u8], start: usize, end: usize) -> usize {
    let mut last_dot = None;
    let mut i = start;
    while i < end {
        if bytes[i] == b'.' {
            last_dot = Some(i);
        }
        i += 1;
    }
    last_dot.map_or(start, |pos| pos + 1)
}

// =============================================================================
// SlowQueryEntry
// =============================================================================

/// A slow-query entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlowQueryEntry {
    pub table: Option<String>,
    pub duration_ms: f64,
}

// =============================================================================
// QueryTracker
// =============================================================================

/// Auxiliary state protected by mutex (cold path only).
///
/// Locked for: (a) unknown table names, (b) slow query logging.
/// Both are rare in production since 99.9%+ of queries target known tables
/// and few queries exceed the slow-query threshold.
#[derive(Debug, Default)]
struct TrackerAux {
    slow_queries: Vec<SlowQueryEntry>,
    unknown_tables: std::collections::HashMap<String, u64>,
}

/// Lightweight query tracker matching the Python `QueryTracker`.
///
/// Thread-safe via atomics for counters. The mutex is only used for the
/// slow-query log and unknown-table counting (cold path).
#[derive(Debug)]
pub struct QueryTracker {
    enabled: AtomicBool,
    total: AtomicU64,
    total_time_us: AtomicU64,
    slow_enabled: AtomicBool,
    slow_threshold_us: AtomicU64,
    /// Lock-free per-table counters indexed by `TableId`.
    per_table: [AtomicU64; TableId::COUNT],
    /// Mutex-protected auxiliary state (slow queries + unknown table counts).
    aux: OrderedMutex<TrackerAux>,
}

/// Helper to create a zeroed `AtomicU64` array.
fn new_atomic_array<const N: usize>() -> [AtomicU64; N] {
    std::array::from_fn(|_| AtomicU64::new(0))
}

impl Default for QueryTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryTracker {
    /// Create a disabled tracker (no overhead until `enable()` is called).
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            total: AtomicU64::new(0),
            total_time_us: AtomicU64::new(0),
            slow_enabled: AtomicBool::new(true),
            slow_threshold_us: AtomicU64::new(250_000), // 250ms default
            per_table: new_atomic_array(),
            aux: OrderedMutex::new(LockLevel::DbQueryTrackerInner, TrackerAux::default()),
        }
    }

    /// Enable tracking with an optional slow-query threshold (in milliseconds).
    pub fn enable(&self, slow_threshold_ms: Option<u64>) {
        match slow_threshold_ms {
            Some(ms) => {
                self.slow_threshold_us
                    .store(ms.saturating_mul(1000), Ordering::Relaxed);
                self.slow_enabled.store(true, Ordering::Release);
            }
            None => {
                self.slow_enabled.store(false, Ordering::Release);
            }
        }
        self.enabled.store(true, Ordering::Release);
    }

    /// Disable tracking.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
        self.slow_enabled.store(false, Ordering::Release);
    }

    /// Whether tracking is currently enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Record a completed query. Call this after each SQL execution.
    ///
    /// **Hot path** (known tables): uses only atomic operations — no locks,
    /// no regex, no allocation.
    ///
    /// **Cold path** (unknown tables or slow queries): falls back to regex
    /// extraction and mutex for the auxiliary state. This is rare in
    /// production since 99.9%+ of queries target known tables.
    pub fn record(&self, sql: &str, duration_us: u64) {
        if !self.is_enabled() {
            return;
        }

        // Atomic counters — no locks
        self.total.fetch_add(1, Ordering::Relaxed);
        self.total_time_us.fetch_add(duration_us, Ordering::Relaxed);

        // Fast table ID extraction (no regex, no allocation)
        let table_id = extract_table_id(sql);

        // Check if we need the slow path (unknown table or slow query)
        let is_slow = self.slow_enabled.load(Ordering::Acquire)
            && duration_us >= self.slow_threshold_us.load(Ordering::Relaxed);
        let needs_mutex = table_id == TableId::Unknown || is_slow;

        if !needs_mutex {
            // HOT PATH: known table, not slow — pure atomic increment
            self.per_table[table_id as usize].fetch_add(1, Ordering::Relaxed);
            return;
        }

        // COLD PATH: unknown table or slow query — lock mutex
        if table_id == TableId::Unknown {
            // Unknown table: regex extraction + mutex for counting
            let name = extract_table(sql);
            let mut aux = self.aux.lock();
            if let Some(ref table_str) = name {
                *aux.unknown_tables.entry(table_str.clone()).or_insert(0) += 1;
            }
            if is_slow && aux.slow_queries.len() < SLOW_QUERY_LIMIT {
                aux.slow_queries.push(SlowQueryEntry {
                    table: name,
                    duration_ms: round_ms(duration_us),
                });
            }
        } else {
            // Known table + slow query
            self.per_table[table_id as usize].fetch_add(1, Ordering::Relaxed);
            if is_slow {
                let mut aux = self.aux.lock();
                if aux.slow_queries.len() < SLOW_QUERY_LIMIT {
                    aux.slow_queries.push(SlowQueryEntry {
                        table: Some(table_id.as_str().to_string()),
                        duration_ms: round_ms(duration_us),
                    });
                }
            }
        }
    }

    /// Get a snapshot of current metrics.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn snapshot(&self) -> QueryTrackerSnapshot {
        // Build per_table HashMap from atomic array (known tables)
        let mut per_table = std::collections::HashMap::new();
        for i in 0..TableId::COUNT {
            let count = self.per_table[i].load(Ordering::Relaxed);
            if count > 0 {
                let id = TableId::from_index(i);
                // Don't include "unknown" bucket in the per_table map
                if id != TableId::Unknown {
                    per_table.insert(id.as_str().to_string(), count);
                }
            }
        }

        let slow_query_ms = if self.slow_enabled.load(Ordering::Acquire) {
            Some(self.slow_threshold_us.load(Ordering::Relaxed) as f64 / 1000.0)
        } else {
            None
        };

        // Merge unknown table counts and grab slow queries
        let aux = self.aux.lock();
        for (table, &count) in &aux.unknown_tables {
            *per_table.entry(table.clone()).or_insert(0) += count;
        }
        let slow_queries = aux.slow_queries.clone();
        drop(aux);

        QueryTrackerSnapshot {
            total: self.total.load(Ordering::Relaxed),
            total_time_ms: round_ms(self.total_time_us.load(Ordering::Relaxed)),
            per_table,
            slow_query_ms,
            slow_queries,
        }
    }

    /// Reset all counters and logs.
    pub fn reset(&self) {
        self.total.store(0, Ordering::Relaxed);
        self.total_time_us.store(0, Ordering::Relaxed);
        for counter in &self.per_table {
            counter.store(0, Ordering::Relaxed);
        }
        let mut aux = self.aux.lock();
        aux.slow_queries.clear();
        aux.unknown_tables.clear();
    }
}

/// Immutable snapshot of tracker state, suitable for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryTrackerSnapshot {
    pub total: u64,
    pub total_time_ms: f64,
    pub per_table: std::collections::HashMap<String, u64>,
    pub slow_query_ms: Option<f64>,
    pub slow_queries: Vec<SlowQueryEntry>,
}

impl QueryTrackerSnapshot {
    /// Convert the snapshot into a JSON-friendly dictionary matching legacy output.
    #[must_use]
    pub fn to_dict(&self) -> serde_json::Value {
        let mut pairs: Vec<(&String, &u64)> = Vec::with_capacity(self.per_table.len());
        pairs.extend(self.per_table.iter());
        pairs.sort_by(|(a_name, a_count), (b_name, b_count)| {
            b_count.cmp(a_count).then_with(|| a_name.cmp(b_name))
        });

        let mut per_table = serde_json::Map::with_capacity(pairs.len());
        for (name, count) in pairs {
            per_table.insert(name.clone(), serde_json::Value::Number((*count).into()));
        }

        let slow_queries = self
            .slow_queries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "table": entry.table,
                    "duration_ms": entry.duration_ms,
                })
            })
            .collect::<Vec<_>>();

        serde_json::json!({
            "total": self.total,
            "total_time_ms": self.total_time_ms,
            "per_table": per_table,
            "slow_query_ms": self.slow_query_ms,
            "slow_queries": slow_queries,
        })
    }
}

/// Start a timer for query instrumentation.
/// Returns an `Instant` that should be passed to [`elapsed_us`].
#[must_use]
pub fn query_timer() -> Instant {
    Instant::now()
}

/// Compute elapsed microseconds since the timer was started.
#[must_use]
pub fn elapsed_us(start: Instant) -> u64 {
    let micros = start.elapsed().as_micros().min(u128::from(u64::MAX));
    u64::try_from(micros).unwrap_or(u64::MAX)
}

thread_local! {
    static ACTIVE_TRACKER: RefCell<Option<Arc<QueryTracker>>> = const { RefCell::new(None) };
}

/// Guard that restores the previous active tracker on drop.
pub struct ActiveTrackerGuard {
    previous: Option<Arc<QueryTracker>>,
}

impl Drop for ActiveTrackerGuard {
    fn drop(&mut self) {
        ACTIVE_TRACKER.with(|slot| {
            *slot.borrow_mut() = self.previous.take();
        });
    }
}

/// Set the active query tracker for the current thread.
pub fn set_active_tracker(tracker: Arc<QueryTracker>) -> ActiveTrackerGuard {
    let previous = ACTIVE_TRACKER.with(|slot| slot.borrow_mut().replace(tracker));
    ActiveTrackerGuard { previous }
}

/// Return the active tracker for the current thread, if any.
#[must_use]
pub fn active_tracker() -> Option<Arc<QueryTracker>> {
    ACTIVE_TRACKER.with(|slot| slot.borrow().clone())
}

/// Access the global tracker for enabling/disabling and snapshots.
#[must_use]
pub fn global_tracker() -> &'static QueryTracker {
    &crate::QUERY_TRACKER
}

/// Record a query against the active tracker (or the global fallback).
///
/// Called by `TrackedConnection` / `TrackedTransaction` after each SQL execution.
/// No-op when tracking is disabled.
pub fn record_query(sql: &str, duration_us: u64) {
    if let Some(tracker) = active_tracker() {
        tracker.record(sql, duration_us);
    } else {
        crate::QUERY_TRACKER.record(sql, duration_us);
    }
}

// =============================================================================
// Legacy regex-based table extraction (used for slow-query log + fixtures)
// =============================================================================

/// Extract the primary table name from a SQL statement using regex.
///
/// Handles schema-qualified names (`public.agents` → `agents`) and
/// various quoting styles (backticks, double-quotes, brackets).
///
/// This is the **slow path** — only called for slow-query log entries when the
/// fast `extract_table_id()` returns `Unknown`, and for fixture tests.
fn extract_table(sql: &str) -> Option<String> {
    /// Compiled pattern to split on schema dots, capturing optional schema segments.
    static SCHEMA_DOT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"[`"\[\]]*\.[`"\[\]]*"#).unwrap());

    for pattern in TABLE_PATTERNS.iter() {
        if let Some(captures) = pattern.captures(sql) {
            if let Some(m) = captures.get(1) {
                let raw = m.as_str();
                // Take last segment after schema dots, then strip quote chars
                let last_segment = SCHEMA_DOT.split(raw).last().unwrap_or(raw);
                let table =
                    last_segment.trim_matches(|c| c == '`' || c == '"' || c == '[' || c == ']');
                if table.is_empty() {
                    return None;
                }
                return Some(table.to_string());
            }
        }
    }
    None
}

/// Round microseconds to milliseconds with 2 decimal places.
#[allow(clippy::cast_precision_loss)]
fn round_ms(us: u64) -> f64 {
    let ms = us as f64 / 1000.0;
    (ms * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn round_f64_to_u64(value: f64) -> u64 {
        if value.is_sign_negative() {
            0
        } else {
            value.round() as u64
        }
    }

    // ── extract_table (regex, legacy) tests ─────────────────────────────

    #[test]
    fn extract_table_insert() {
        assert_eq!(
            extract_table("INSERT INTO messages (id) VALUES (1)"),
            Some("messages".to_string())
        );
    }

    #[test]
    fn extract_table_update() {
        assert_eq!(
            extract_table("UPDATE agents SET name = 'x' WHERE id = 1"),
            Some("agents".to_string())
        );
    }

    #[test]
    fn extract_table_select() {
        assert_eq!(
            extract_table("SELECT * FROM projects WHERE id = 1"),
            Some("projects".to_string())
        );
    }

    #[test]
    fn extract_table_quoted() {
        assert_eq!(
            extract_table(r#"SELECT * FROM "file_reservations" WHERE 1"#),
            Some("file_reservations".to_string())
        );
    }

    #[test]
    fn extract_table_unknown() {
        assert_eq!(extract_table("PRAGMA wal_checkpoint"), None);
    }

    // ── extract_table_id (fast path) tests ──────────────────────────────

    #[test]
    fn fast_extract_known_tables() {
        assert_eq!(
            extract_table_id("SELECT * FROM messages WHERE id = 1"),
            TableId::Messages
        );
        assert_eq!(
            extract_table_id("INSERT INTO agents (name) VALUES ('x')"),
            TableId::Agents
        );
        assert_eq!(
            extract_table_id("UPDATE projects SET name = 'x'"),
            TableId::Projects
        );
        assert_eq!(
            extract_table_id("SELECT * FROM file_reservations WHERE 1"),
            TableId::FileReservations
        );
        assert_eq!(
            extract_table_id("SELECT * FROM agent_links WHERE status = 'approved'"),
            TableId::AgentLinks
        );
        assert_eq!(
            extract_table_id("INSERT INTO message_recipients (message_id) VALUES (1)"),
            TableId::MessageRecipients
        );
        assert_eq!(
            extract_table_id("SELECT * FROM fts_messages WHERE fts_messages MATCH 'test'"),
            TableId::FtsMessages
        );
        assert_eq!(
            extract_table_id("INSERT INTO products (name) VALUES ('test')"),
            TableId::Products
        );
        assert_eq!(
            extract_table_id("SELECT * FROM product_project_links WHERE 1"),
            TableId::ProductProjectLinks
        );
        assert_eq!(
            extract_table_id("SELECT * FROM project_sibling_suggestions"),
            TableId::ProjectSiblingSuggestions
        );
    }

    #[test]
    fn fast_extract_unknown() {
        assert_eq!(extract_table_id("PRAGMA wal_checkpoint"), TableId::Unknown);
        assert_eq!(extract_table_id("SELECT 1"), TableId::Unknown);
        assert_eq!(extract_table_id(""), TableId::Unknown);
        assert_eq!(
            extract_table_id("DELETE FROM old_messages WHERE 1"),
            TableId::Unknown
        );
    }

    #[test]
    fn fast_extract_case_insensitive() {
        assert_eq!(
            extract_table_id("select * FROM Messages"),
            TableId::Messages
        );
        assert_eq!(
            extract_table_id("Insert Into agents (name) values ('x')"),
            TableId::Agents
        );
        assert_eq!(
            extract_table_id("update projects set name = 'y'"),
            TableId::Projects
        );
    }

    #[test]
    fn fast_extract_with_quotes() {
        assert_eq!(
            extract_table_id(r#"SELECT * FROM "messages" WHERE id=1"#),
            TableId::Messages
        );
        assert_eq!(
            extract_table_id("SELECT * FROM `agents` WHERE 1"),
            TableId::Agents
        );
    }

    #[test]
    fn fast_extract_schema_qualified() {
        assert_eq!(
            extract_table_id(r#"SELECT * FROM "public"."messages""#),
            TableId::Messages
        );
        assert_eq!(
            extract_table_id("SELECT * FROM catalog.schema.agents"),
            TableId::Agents
        );
    }

    #[test]
    fn fast_extract_insert_priority() {
        // INSERT INTO should take priority over FROM in subqueries
        assert_eq!(
            extract_table_id("INSERT INTO messages SELECT * FROM agents"),
            TableId::Messages
        );
    }

    #[test]
    fn fast_extract_or_ignore() {
        assert_eq!(
            extract_table_id("INSERT OR IGNORE INTO agents (name) VALUES ('x')"),
            TableId::Agents
        );
    }

    #[test]
    fn fast_extract_multiline() {
        assert_eq!(
            extract_table_id("SELECT id, name\nFROM agents\nWHERE active=1"),
            TableId::Agents
        );
    }

    #[test]
    fn fast_extract_extra_whitespace() {
        assert_eq!(
            extract_table_id("INSERT   INTO   messages  (body) VALUES (?)"),
            TableId::Messages
        );
        assert_eq!(
            extract_table_id("UPDATE    projects   SET archived=1"),
            TableId::Projects
        );
    }

    // ── TableId round-trip ──────────────────────────────────────────────

    #[test]
    fn table_id_from_index_roundtrip() {
        for i in 0..TableId::COUNT {
            let id = TableId::from_index(i);
            assert_eq!(id as usize, i);
        }
        // Out of range -> Unknown
        assert_eq!(TableId::from_index(99), TableId::Unknown);
    }

    // ── Tracker tests ───────────────────────────────────────────────────

    #[test]
    fn tracker_disabled_by_default() {
        let tracker = QueryTracker::new();
        assert!(!tracker.is_enabled());
        tracker.record("SELECT 1 FROM projects", 100);
        let snap = tracker.snapshot();
        assert_eq!(snap.total, 0);
    }

    #[test]
    fn tracker_records_when_enabled() {
        let tracker = QueryTracker::new();
        tracker.enable(Some(100)); // 100ms threshold
        tracker.record("SELECT * FROM messages WHERE id = 1", 50_000); // 50ms
        tracker.record("INSERT INTO agents (name) VALUES ('x')", 200_000); // 200ms (slow)
        let snap = tracker.snapshot();
        assert_eq!(snap.total, 2);
        assert_eq!(snap.per_table.get("messages"), Some(&1));
        assert_eq!(snap.per_table.get("agents"), Some(&1));
        // 200ms >= 100ms threshold → slow
        assert_eq!(snap.slow_queries.len(), 1);
        assert_eq!(snap.slow_queries[0].table.as_deref(), Some("agents"));
    }

    #[test]
    fn tracker_reset() {
        let tracker = QueryTracker::new();
        tracker.enable(None);
        tracker.record("SELECT 1 FROM projects", 100);
        tracker.reset();
        let snap = tracker.snapshot();
        assert_eq!(snap.total, 0);
        assert!(snap.per_table.is_empty());
    }

    #[test]
    fn slow_query_cap() {
        let tracker = QueryTracker::new();
        tracker.enable(Some(0)); // 0ms threshold = everything is slow
        for i in 0..60 {
            tracker.record(&format!("SELECT {i} FROM messages"), 1000);
        }
        let snap = tracker.snapshot();
        assert_eq!(snap.total, 60);
        assert_eq!(snap.slow_queries.len(), SLOW_QUERY_LIMIT);
    }

    // ── round_ms edge cases ─────────────────────────────────────────────

    fn assert_close(got: f64, expected: f64) {
        let diff = (got - expected).abs();
        assert!(diff < 1e-9, "expected {expected} (diff={diff}), got {got}");
    }

    #[test]
    fn round_ms_zero() {
        assert_close(round_ms(0), 0.0);
    }

    #[test]
    fn round_ms_exact_milliseconds() {
        assert_close(round_ms(1000), 1.0); // 1ms
        assert_close(round_ms(250_000), 250.0); // 250ms
    }

    #[test]
    fn round_ms_fractional_rounds_to_2_decimal() {
        assert_close(round_ms(1234), 1.23); // 1.234ms → 1.23
        assert_close(round_ms(1235), 1.24); // 1.235ms → 1.24 (round half up at .5)
        assert_close(round_ms(1500), 1.5); // 1.5ms
        assert_close(round_ms(999), 1.0); // 0.999ms → 1.0
    }

    #[test]
    fn round_ms_large_value() {
        assert_close(round_ms(60_000_000), 60000.0); // 60 seconds
    }

    // ── extract_table additional coverage ──────────────────────────────

    #[test]
    fn extract_table_insert_or_ignore() {
        assert_eq!(
            extract_table("INSERT OR IGNORE INTO agents (name) VALUES ('x')"),
            Some("agents".to_string())
        );
    }

    #[test]
    fn extract_table_insert_or_abort() {
        assert_eq!(
            extract_table("INSERT OR ABORT INTO messages (body) VALUES ('hi')"),
            Some("messages".to_string())
        );
    }

    #[test]
    fn extract_table_with_cte() {
        // WITH clause: FROM in CTE, but first FROM is matched
        assert_eq!(
            extract_table("WITH recent AS (SELECT * FROM messages) SELECT * FROM recent"),
            Some("messages".to_string())
        );
    }

    #[test]
    fn extract_table_alter_returns_none() {
        assert_eq!(
            extract_table("ALTER TABLE agents ADD COLUMN email TEXT"),
            None
        );
    }

    #[test]
    fn extract_table_drop_returns_none() {
        assert_eq!(extract_table("DROP TABLE IF EXISTS old_data"), None);
    }

    // ── Tracker enable/disable lifecycle ────────────────────────────────

    #[test]
    fn tracker_enable_then_disable() {
        let tracker = QueryTracker::new();
        tracker.enable(Some(100));
        assert!(tracker.is_enabled());

        tracker.record("SELECT * FROM agents", 1000);
        assert_eq!(tracker.snapshot().total, 1);

        tracker.disable();
        assert!(!tracker.is_enabled());

        // Recording after disable should be a no-op.
        tracker.record("SELECT * FROM messages", 1000);
        assert_eq!(tracker.snapshot().total, 1);
    }

    #[test]
    fn tracker_enable_without_slow_threshold() {
        let tracker = QueryTracker::new();
        tracker.enable(None); // No slow query tracking
        tracker.record("SELECT * FROM messages", 999_999_999); // Very slow
        let snap = tracker.snapshot();
        assert_eq!(snap.total, 1);
        assert!(
            snap.slow_queries.is_empty(),
            "no slow queries without threshold"
        );
        assert!(snap.slow_query_ms.is_none());
    }

    #[test]
    fn tracker_snapshot_is_immutable() {
        let tracker = QueryTracker::new();
        tracker.enable(Some(250));
        tracker.record("SELECT * FROM agents", 1000);
        let snap1 = tracker.snapshot();

        tracker.record("SELECT * FROM messages", 2000);
        let snap2 = tracker.snapshot();

        assert_eq!(snap1.total, 1, "first snapshot should not change");
        assert_eq!(snap2.total, 2, "second snapshot should reflect new query");
    }

    // ── to_dict sorting verification ────────────────────────────────────

    #[test]
    fn to_dict_per_table_sorted_by_count_desc_then_name_asc() {
        let tracker = QueryTracker::new();
        tracker.enable(Some(250));
        // agents: 2, messages: 3, projects: 1, file_reservations: 2
        tracker.record("SELECT * FROM agents", 1000);
        tracker.record("SELECT * FROM agents", 1000);
        tracker.record("SELECT * FROM messages", 1000);
        tracker.record("SELECT * FROM messages", 1000);
        tracker.record("SELECT * FROM messages", 1000);
        tracker.record("SELECT * FROM projects", 1000);
        tracker.record("SELECT * FROM file_reservations", 1000);
        tracker.record("SELECT * FROM file_reservations", 1000);

        let snap = tracker.snapshot();
        let dict = snap.to_dict();
        let per_table = dict["per_table"].as_object().unwrap();

        // Verify counts
        assert_eq!(per_table["messages"].as_u64(), Some(3));
        assert_eq!(per_table["agents"].as_u64(), Some(2));
        assert_eq!(per_table["file_reservations"].as_u64(), Some(2));
        assert_eq!(per_table["projects"].as_u64(), Some(1));

        let keys: Vec<&str> = per_table.keys().map(std::string::String::as_str).collect();

        // serde_json::Map backend depends on feature unification:
        // - BTreeMap backend iterates in key order.
        // - IndexMap backend iterates in insertion order.
        //
        // We keep this test robust under both while still checking that insertion
        // order reflects count-desc, name-asc sorting when preserve_order is enabled.
        let mut order_probe = serde_json::Map::new();
        order_probe.insert("b".to_string(), serde_json::Value::Null);
        order_probe.insert("a".to_string(), serde_json::Value::Null);
        let preserves_insertion_order = order_probe
            .keys()
            .map(std::string::String::as_str)
            .eq(["b", "a"]);

        if preserves_insertion_order {
            assert_eq!(
                keys,
                vec!["messages", "agents", "file_reservations", "projects"]
            );
        } else {
            assert_eq!(
                keys,
                vec!["agents", "file_reservations", "messages", "projects"]
            );
        }
    }

    // ── Thread-local tracker isolation ───────────────────────────────────

    #[test]
    fn active_tracker_is_none_initially() {
        // Note: this test depends on no other test having set the tracker
        // on this thread. Since tests may run in parallel on different threads,
        // this verifies the thread-local default.
        let tracker = active_tracker();
        // May or may not be None depending on test execution order on this thread,
        // but the mechanism should not panic.
        drop(tracker);
    }

    #[test]
    fn set_active_tracker_guard_restores_previous() {
        let t1 = Arc::new(QueryTracker::new());
        t1.enable(Some(100));
        let _g1 = set_active_tracker(t1);

        {
            let t2 = Arc::new(QueryTracker::new());
            t2.enable(Some(200));
            let _g2 = set_active_tracker(t2);

            // Inside inner scope, active should be t2.
            let current = active_tracker().unwrap();
            current.record("SELECT * FROM messages", 1000);
            assert_eq!(current.snapshot().total, 1);
        }

        // After inner guard dropped, active should be t1 again.
        let restored = active_tracker().unwrap();
        assert_eq!(restored.snapshot().total, 0, "t1 should have no queries");
    }

    // ── Snapshot JSON serialization ─────────────────────────────────────

    #[test]
    fn snapshot_serializes_to_json() {
        let tracker = QueryTracker::new();
        tracker.enable(Some(100));
        tracker.record("SELECT * FROM agents", 50_000);
        tracker.record("SELECT * FROM messages", 150_000);
        let snap = tracker.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["total"], 2);
        assert!(v["total_time_ms"].is_f64());
    }

    #[test]
    fn to_dict_matches_legacy_keys() {
        let tracker = QueryTracker::new();
        tracker.enable(Some(250));
        let snap = tracker.snapshot();
        let dict = snap.to_dict();
        assert!(dict.get("total").is_some());
        assert!(dict.get("total_time_ms").is_some());
        assert!(dict.get("per_table").is_some());
        assert!(dict.get("slow_query_ms").is_some());
        assert!(dict.get("slow_queries").is_some());
        // Must not have extra keys.
        assert_eq!(dict.as_object().unwrap().len(), 5);
    }

    // ── Fixture-driven table extraction tests ──────────────────────────
    #[test]
    fn fixture_table_extraction() {
        let raw = include_str!(
            "../../mcp-agent-mail-db/tests/fixtures/instrumentation/table_extraction.json"
        );
        let doc: serde_json::Value = serde_json::from_str(raw).unwrap();
        let vectors = doc["vectors"].as_array().unwrap();
        for (i, v) in vectors.iter().enumerate() {
            let sql = v["sql"].as_str().unwrap();
            let expected = v["expected"].as_str().map(String::from);
            let actual = extract_table(sql);
            assert_eq!(
                actual,
                expected,
                "table_extraction vector {i}: {desc}",
                desc = v["desc"].as_str().unwrap_or("?")
            );
        }
    }

    // ── Fixture-driven tracker aggregation tests ───────────────────────
    #[test]
    fn fixture_tracker_aggregation() {
        let raw = include_str!(
            "../../mcp-agent-mail-db/tests/fixtures/instrumentation/tracker_aggregation.json"
        );
        let doc: serde_json::Value = serde_json::from_str(raw).unwrap();
        let vectors = doc["vectors"].as_array().unwrap();
        for (i, v) in vectors.iter().enumerate() {
            let desc = v["desc"].as_str().unwrap_or("?");
            let slow_threshold_ms = if v["slow_query_ms"].is_null() {
                None
            } else {
                Some(round_f64_to_u64(v["slow_query_ms"].as_f64().unwrap()))
            };

            let tracker = QueryTracker::new();
            tracker.enable(slow_threshold_ms);

            let queries = v["queries"].as_array().unwrap();
            for q in queries {
                let sql = q["sql"].as_str().unwrap();
                let duration_ms = q["duration_ms"].as_f64().unwrap();
                // Convert ms to us for the tracker
                let duration_micros = round_f64_to_u64(duration_ms * 1000.0);
                tracker.record(sql, duration_micros);
            }

            let snap = tracker.snapshot();
            let expected = &v["expected"];

            // total
            assert_eq!(
                snap.total,
                expected["total"].as_u64().unwrap(),
                "aggregation vector {i} ({desc}): total mismatch"
            );

            // total_time_ms (compare with tolerance for floating point)
            let expected_time = expected["total_time_ms"].as_f64().unwrap();
            assert!(
                (snap.total_time_ms - expected_time).abs() < 0.02,
                "aggregation vector {i} ({desc}): total_time_ms mismatch: got {}, expected {}",
                snap.total_time_ms,
                expected_time
            );

            // per_table
            let expected_table = expected["per_table"].as_object().unwrap();
            assert_eq!(
                snap.per_table.len(),
                expected_table.len(),
                "aggregation vector {i} ({desc}): per_table length mismatch"
            );
            for (table, count) in expected_table {
                assert_eq!(
                    snap.per_table.get(table),
                    Some(&(count.as_u64().unwrap())),
                    "aggregation vector {i} ({desc}): table {table} count mismatch"
                );
            }

            // slow_query_ms
            if expected["slow_query_ms"].is_null() {
                assert_eq!(
                    snap.slow_query_ms, None,
                    "aggregation vector {i} ({desc}): slow_query_ms should be None"
                );
            } else {
                let expected_sq = expected["slow_query_ms"].as_f64().unwrap();
                assert!(
                    snap.slow_query_ms.is_some(),
                    "aggregation vector {i} ({desc}): slow_query_ms should be Some"
                );
                assert!(
                    (snap.slow_query_ms.unwrap() - expected_sq).abs() < 0.01,
                    "aggregation vector {i} ({desc}): slow_query_ms mismatch"
                );
            }

            // slow_queries
            let expected_slow = expected["slow_queries"].as_array().unwrap();
            assert_eq!(
                snap.slow_queries.len(),
                expected_slow.len(),
                "aggregation vector {i} ({desc}): slow_queries count mismatch"
            );
            for (j, (actual_sq, expected_sq)) in snap
                .slow_queries
                .iter()
                .zip(expected_slow.iter())
                .enumerate()
            {
                let exp_table = expected_sq["table"].as_str().map(String::from);
                assert_eq!(
                    actual_sq.table, exp_table,
                    "aggregation vector {i}.slow[{j}] ({desc}): table mismatch"
                );
                let exp_dur = expected_sq["duration_ms"].as_f64().unwrap();
                assert!(
                    (actual_sq.duration_ms - exp_dur).abs() < 0.02,
                    "aggregation vector {i}.slow[{j}] ({desc}): duration_ms mismatch: got {}, expected {}",
                    actual_sq.duration_ms,
                    exp_dur
                );
            }
        }
    }
}
