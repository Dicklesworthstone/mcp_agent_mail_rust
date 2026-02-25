//! Python-to-Rust database migration: timestamp format detection and conversion.
//!
//! The Python mcp-agent-mail stores timestamps as TEXT (ISO-8601 strings like
//! `"2026-02-24 15:30:00.123456"`), while the Rust version uses `i64` microseconds
//! since Unix epoch. This module detects which format a database uses and converts
//! TEXT timestamps to `i64` when migrating from Python.
//!
//! # Usage
//!
//! ```ignore
//! use mcp_agent_mail_db::migrate::{detect_timestamp_format, TimestampFormat};
//!
//! let format = detect_timestamp_format(&conn)?;
//! match format {
//!     TimestampFormat::RustMicros => println!("Already migrated"),
//!     TimestampFormat::PythonText => println!("Needs migration"),
//!     TimestampFormat::Empty => println!("No data to migrate"),
//!     TimestampFormat::Mixed { .. } => println!("Partially migrated"),
//!     TimestampFormat::Unknown(s) => eprintln!("Unknown format: {s}"),
//! }
//! ```

use crate::DbConn;
use chrono::NaiveDateTime;
use thiserror::Error;

// ── Error types ────────────────────────────────────────────────────────────

/// Errors that can occur during migration detection or conversion.
#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("database query failed: {0}")]
    Query(String),

    #[error("timestamp parse error in {table}.{column} row {row_id}: {value:?}")]
    TimestampParse {
        table: String,
        column: String,
        row_id: i64,
        value: String,
    },

    #[error("migration aborted: {0}")]
    Aborted(String),
}

impl From<sqlmodel_core::Error> for MigrationError {
    fn from(e: sqlmodel_core::Error) -> Self {
        Self::Query(e.to_string())
    }
}

// ── Timestamp format detection ─────────────────────────────────────────────

/// The detected timestamp format of a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimestampFormat {
    /// All timestamps are `i64` microseconds (Rust native format).
    RustMicros,

    /// All timestamps are TEXT strings (Python format, needs migration).
    PythonText,

    /// Database has no data — no migration needed.
    Empty,

    /// Some tables have TEXT, some have INTEGER — partially migrated.
    /// Contains the names of tables still in TEXT format.
    Mixed { text_tables: Vec<String> },

    /// Unrecognized format (stores the `typeof()` result).
    Unknown(String),
}

impl TimestampFormat {
    /// Whether migration is needed.
    #[must_use]
    pub const fn needs_migration(&self) -> bool {
        matches!(self, Self::PythonText | Self::Mixed { .. })
    }
}

impl std::fmt::Display for TimestampFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RustMicros => write!(f, "i64 microseconds (Rust native)"),
            Self::PythonText => write!(f, "TEXT timestamps (Python format, needs migration)"),
            Self::Empty => write!(f, "empty database (no migration needed)"),
            Self::Mixed { text_tables } => {
                write!(f, "mixed format (TEXT in: {})", text_tables.join(", "))
            }
            Self::Unknown(s) => write!(f, "unknown format: {s}"),
        }
    }
}

/// Tables and their primary timestamp column to probe for format detection.
///
/// We check the primary timestamp column of each table. If the column stores
/// a TEXT value, the table is in Python format.
const TIMESTAMP_PROBE_COLUMNS: &[(&str, &str)] = &[
    ("projects", "created_at"),
    ("agents", "inception_ts"),
    ("messages", "created_ts"),
    ("message_recipients", "read_ts"),
    ("file_reservations", "created_ts"),
    ("agent_links", "created_ts"),
    ("product_project_links", "created_at"),
    ("products", "created_at"),
];

/// All tables and ALL of their timestamp columns (for migration).
/// Each entry is `(table, column, is_nullable)`.
pub const TIMESTAMP_COLUMNS: &[(&str, &str, bool)] = &[
    ("projects", "created_at", false),
    ("products", "created_at", false),
    ("product_project_links", "created_at", false),
    ("agents", "inception_ts", false),
    ("agents", "last_active_ts", false),
    ("messages", "created_ts", false),
    ("message_recipients", "read_ts", true),
    ("message_recipients", "ack_ts", true),
    ("file_reservations", "created_ts", false),
    ("file_reservations", "expires_ts", false),
    ("file_reservations", "released_ts", true),
    ("agent_links", "created_ts", false),
    ("agent_links", "updated_ts", false),
    ("agent_links", "expires_ts", true),
    ("project_sibling_suggestions", "created_ts", false),
    ("project_sibling_suggestions", "evaluated_ts", false),
    ("project_sibling_suggestions", "confirmed_ts", true),
    ("project_sibling_suggestions", "dismissed_ts", true),
];

/// Detect the timestamp format used in a database.
///
/// Probes each table's primary timestamp column using `typeof()` to determine
/// whether the database stores TEXT (Python) or INTEGER (Rust) timestamps.
///
/// # Errors
///
/// Returns `MigrationError::Query` if any SQL query fails.
pub fn detect_timestamp_format(conn: &DbConn) -> Result<TimestampFormat, MigrationError> {
    let mut saw_integer = false;
    let mut saw_text = false;
    let mut text_tables = Vec::new();

    for &(table, column) in TIMESTAMP_PROBE_COLUMNS {
        // Check if table exists first (the table might not exist in older schemas).
        let table_check = conn.query_sync(
            &format!("SELECT name FROM sqlite_master WHERE type='table' AND name='{table}'"),
            &[],
        );
        // FrankenSQLite may not support sqlite_master — assume table exists and
        // let the typeof query below fail gracefully instead.
        let table_exists = table_check.as_ref().map_or(true, |rows| !rows.is_empty());
        if !table_exists {
            continue;
        }

        // Query the typeof() of the first non-NULL value in the column.
        let sql =
            format!("SELECT typeof({column}) AS t FROM {table} WHERE {column} IS NOT NULL LIMIT 1");
        let Ok(rows) = conn.query_sync(&sql, &[]) else {
            continue; // Table might not exist or column renamed
        };

        if rows.is_empty() {
            continue; // Table is empty, skip
        }

        let type_str: String = rows[0].get_named("t").unwrap_or_default();
        match type_str.as_str() {
            "integer" => saw_integer = true,
            "text" => {
                saw_text = true;
                text_tables.push(table.to_string());
            }
            "real" => {
                // REAL timestamps are unusual but could appear — treat like integer
                saw_integer = true;
            }
            "null" => {} // All values NULL, skip
            other => {
                return Ok(TimestampFormat::Unknown(other.to_string()));
            }
        }
    }

    if !saw_integer && !saw_text {
        return Ok(TimestampFormat::Empty);
    }
    if saw_text && !saw_integer {
        return Ok(TimestampFormat::PythonText);
    }
    if saw_integer && !saw_text {
        return Ok(TimestampFormat::RustMicros);
    }
    // Both TEXT and INTEGER found — partially migrated
    Ok(TimestampFormat::Mixed { text_tables })
}

/// Detect format for a specific table and column.
///
/// Returns `Some("integer")`, `Some("text")`, `Some("null")`, or `None` if
/// the table is empty or does not exist.
pub fn detect_column_format(
    conn: &DbConn,
    table: &str,
    column: &str,
) -> Result<Option<String>, MigrationError> {
    let sql =
        format!("SELECT typeof({column}) AS t FROM {table} WHERE {column} IS NOT NULL LIMIT 1");
    match conn.query_sync(&sql, &[]) {
        Ok(rows) if rows.is_empty() => Ok(None),
        Ok(rows) => {
            let t: String = rows[0].get_named("t").unwrap_or_default();
            Ok(Some(t))
        }
        Err(_) => Ok(None),
    }
}

// ── Timestamp conversion functions ─────────────────────────────────────────

/// Convert a Python TEXT timestamp to Rust i64 microseconds.
///
/// Handles these Python timestamp formats:
/// - `"2026-02-24 15:30:00.123456"` (space separator, microseconds)
/// - `"2026-02-24T15:30:00.123456"` (ISO-8601 with T)
/// - `"2026-02-24 15:30:00"` (no fractional seconds)
/// - `"2026-02-24T15:30:00"` (no fractional, T separator)
/// - `"2026-02-24"` (date only → midnight UTC)
/// - `"2026-02-24 15:30:00.123456+00:00"` (with timezone → strip tz, treat as UTC)
///
/// Returns `None` for empty strings (treated as NULL).
///
/// # Errors
///
/// Returns `MigrationError::TimestampParse` if the string cannot be parsed.
pub fn text_to_micros(
    text: &str,
    table: &str,
    column: &str,
    row_id: i64,
) -> Result<Option<i64>, MigrationError> {
    // Parse formats in priority order.
    const FORMATS: &[&str] = &[
        "%Y-%m-%d %H:%M:%S%.f", // "2026-02-24 15:30:00.123456"
        "%Y-%m-%dT%H:%M:%S%.f", // "2026-02-24T15:30:00.123456"
        "%Y-%m-%d %H:%M:%S",    // "2026-02-24 15:30:00"
        "%Y-%m-%dT%H:%M:%S",    // "2026-02-24T15:30:00"
        "%Y-%m-%d",             // "2026-02-24"
    ];

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    // Try parsing with timezone (RFC 3339 / ISO 8601 with offset)
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Ok(Some(dt.timestamp_micros()));
    }

    // Strip timezone suffix if present (e.g., "+00:00", "Z")
    let without_tz = strip_timezone_suffix(trimmed);

    for fmt in FORMATS {
        if let Ok(dt) = NaiveDateTime::parse_from_str(without_tz, fmt) {
            return Ok(Some(crate::timestamps::naive_to_micros(dt)));
        }
    }

    // Special case: date-only strings won't parse as NaiveDateTime,
    // try NaiveDate and convert to midnight.
    if let Ok(date) = chrono::NaiveDate::parse_from_str(without_tz, "%Y-%m-%d") {
        let dt = date.and_hms_opt(0, 0, 0).expect("midnight is always valid");
        return Ok(Some(crate::timestamps::naive_to_micros(dt)));
    }

    Err(MigrationError::TimestampParse {
        table: table.to_string(),
        column: column.to_string(),
        row_id,
        value: text.to_string(),
    })
}

/// Strip common timezone suffixes from a timestamp string.
fn strip_timezone_suffix(s: &str) -> &str {
    // Strip trailing "Z"
    let s = s.strip_suffix('Z').unwrap_or(s);
    // Strip "+HH:MM" or "-HH:MM" offset at end
    if s.len() >= 6 {
        let tail = &s[s.len() - 6..];
        if (tail.starts_with('+') || tail.starts_with('-'))
            && tail[1..3].chars().all(|c| c.is_ascii_digit())
            && tail.as_bytes()[3] == b':'
            && tail[4..6].chars().all(|c| c.is_ascii_digit())
        {
            return &s[..s.len() - 6];
        }
    }
    s
}

/// Summary of a single-column conversion pass.
#[derive(Debug, Clone)]
pub struct ColumnConversionResult {
    /// Table name.
    pub table: String,
    /// Column name.
    pub column: String,
    /// Number of rows successfully converted.
    pub converted: u64,
    /// Number of rows skipped due to parse errors.
    pub skipped: u64,
    /// Number of NULL values left as-is.
    pub nulls: u64,
    /// Parse error details for skipped rows (table, column, `row_id`, value).
    pub errors: Vec<String>,
}

/// Convert all TEXT timestamps in a single column to i64 microseconds.
///
/// Reads all rows where the column is TEXT (not NULL, not already integer),
/// converts each value, and updates the row in-place.
///
/// Uses explicit column names (not `SELECT *`) for `FrankenSQLite` compatibility.
///
/// # Errors
///
/// Returns `MigrationError` if the query or update fails critically.
/// Individual row parse errors are collected in the result and do NOT abort
/// the conversion — we skip and continue.
pub fn convert_column(
    conn: &DbConn,
    table: &str,
    column: &str,
) -> Result<ColumnConversionResult, MigrationError> {
    use sqlmodel_core::Value;

    let mut result = ColumnConversionResult {
        table: table.to_string(),
        column: column.to_string(),
        converted: 0,
        skipped: 0,
        nulls: 0,
        errors: Vec::new(),
    };

    // Determine the primary key column. All our tables use `id` except
    // message_recipients which uses (message_id, agent_id).
    let is_composite_pk = table == "message_recipients";

    // Read all rows where the column is TEXT.
    let select_sql = if is_composite_pk {
        format!(
            "SELECT message_id, agent_id, {column} FROM {table} \
             WHERE typeof({column}) = 'text'"
        )
    } else {
        format!(
            "SELECT id, {column} FROM {table} \
             WHERE typeof({column}) = 'text'"
        )
    };

    let rows = conn
        .query_sync(&select_sql, &[])
        .map_err(|e| MigrationError::Query(format!("failed to read {table}.{column}: {e}")))?;

    for row in &rows {
        let (row_id, pk_values): (i64, Vec<Value>) = if is_composite_pk {
            let msg_id: i64 = row.get_named("message_id").unwrap_or(0);
            let agent_id: i64 = row.get_named("agent_id").unwrap_or(0);
            (msg_id, vec![Value::BigInt(msg_id), Value::BigInt(agent_id)])
        } else {
            let id: i64 = row.get_named("id").unwrap_or(0);
            (id, vec![Value::BigInt(id)])
        };

        let text_val: String = row.get_named(column).unwrap_or_default();

        if text_val.is_empty() {
            result.nulls += 1;
            // Update empty string to NULL
            let update_sql = if is_composite_pk {
                format!(
                    "UPDATE {table} SET {column} = NULL \
                     WHERE message_id = ? AND agent_id = ?"
                )
            } else {
                format!("UPDATE {table} SET {column} = NULL WHERE id = ?")
            };
            let _ = conn.query_sync(&update_sql, &pk_values);
            continue;
        }

        match text_to_micros(&text_val, table, column, row_id) {
            Ok(Some(micros)) => {
                let update_sql = if is_composite_pk {
                    format!(
                        "UPDATE {table} SET {column} = ? \
                         WHERE message_id = ? AND agent_id = ?"
                    )
                } else {
                    format!("UPDATE {table} SET {column} = ? WHERE id = ?")
                };
                let mut params = vec![Value::BigInt(micros)];
                params.extend(pk_values);
                if let Err(e) = conn.query_sync(&update_sql, &params) {
                    result.skipped += 1;
                    result
                        .errors
                        .push(format!("{table}.{column} id={row_id}: update failed: {e}"));
                } else {
                    result.converted += 1;
                }
            }
            Ok(None) => {
                result.nulls += 1;
            }
            Err(e) => {
                result.skipped += 1;
                result.errors.push(format!("{e}"));
            }
        }
    }

    Ok(result)
}

/// Summary of a full database migration.
#[derive(Debug, Clone)]
pub struct MigrationSummary {
    /// Per-column conversion results.
    pub columns: Vec<ColumnConversionResult>,
    /// Total rows converted across all tables.
    pub total_converted: u64,
    /// Total rows skipped across all tables.
    pub total_skipped: u64,
    /// Total NULL values across all tables.
    pub total_nulls: u64,
    /// Whether migration completed successfully (no critical errors).
    pub success: bool,
}

/// Convert all TEXT timestamp columns in the database to i64 microseconds.
///
/// Iterates over all known timestamp columns and converts each one.
/// Returns a summary of the migration.
///
/// # Errors
///
/// Returns `MigrationError` if a critical query fails. Individual row
/// parse errors are collected in the summary, not propagated.
pub fn convert_all_timestamps(conn: &DbConn) -> Result<MigrationSummary, MigrationError> {
    let mut summary = MigrationSummary {
        columns: Vec::new(),
        total_converted: 0,
        total_skipped: 0,
        total_nulls: 0,
        success: true,
    };

    for &(table, column, _nullable) in TIMESTAMP_COLUMNS {
        // Skip tables that don't exist or have no TEXT timestamps.
        if let Some(fmt) = detect_column_format(conn, table, column)? {
            if fmt != "text" {
                continue; // Already integer, skip
            }
        } else {
            continue; // Table empty or doesn't exist
        }

        match convert_column(conn, table, column) {
            Ok(result) => {
                summary.total_converted += result.converted;
                summary.total_skipped += result.skipped;
                summary.total_nulls += result.nulls;
                if result.skipped > 0 {
                    summary.success = false;
                }
                summary.columns.push(result);
            }
            Err(e) => {
                summary.success = false;
                summary.columns.push(ColumnConversionResult {
                    table: table.to_string(),
                    column: column.to_string(),
                    converted: 0,
                    skipped: 0,
                    nulls: 0,
                    errors: vec![e.to_string()],
                });
            }
        }
    }

    Ok(summary)
}

// ── Database path resolution ───────────────────────────────────────────────

/// Common locations where the Python mcp-agent-mail stored its database.
///
/// The Python version uses a relative path (`./storage.sqlite3`) and its shell
/// alias `cd`s to the clone directory, so the DB ends up in the clone dir.
const PYTHON_DB_CANDIDATES: &[&str] = &[
    "~/mcp_agent_mail/storage.sqlite3",
    "~/mcp-agent-mail/storage.sqlite3",
    "~/projects/mcp_agent_mail/storage.sqlite3",
    "~/code/mcp_agent_mail/storage.sqlite3",
];

/// Search common locations for a Python mcp-agent-mail database file.
///
/// Returns the absolute path to the first valid `SQLite` database found,
/// or `None` if no Python database was detected.
///
/// Checks:
/// 1. An explicit path (if provided, e.g. from alias detection)
/// 2. Common clone directory locations
/// 3. The `DATABASE_URL` environment variable
pub fn find_python_database(
    explicit_clone_path: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    let mut candidates: Vec<PathBuf> = Vec::new();

    // 1. Explicit clone path (highest priority)
    if let Some(clone) = explicit_clone_path {
        candidates.push(clone.join("storage.sqlite3"));
        candidates.push(clone.join("db/storage.sqlite3"));
    }

    // 2. Common clone locations
    for pattern in PYTHON_DB_CANDIDATES {
        let expanded = pattern.replace('~', &home.to_string_lossy());
        candidates.push(PathBuf::from(expanded));
    }

    // 3. DATABASE_URL env var
    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Some(path) = mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(&url)
    {
        candidates.push(path);
    }

    // Check each candidate
    for candidate in &candidates {
        if candidate.is_file() {
            // Verify it's a SQLite file by checking the magic header
            if is_sqlite_file(candidate) {
                return Some(candidate.clone());
            }
        }
    }

    None
}

/// Check if a file has the `SQLite` magic header bytes.
fn is_sqlite_file(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut header = [0u8; 16];
    if f.read_exact(&mut header).is_err() {
        return false;
    }
    // SQLite magic: "SQLite format 3\0"
    header.starts_with(b"SQLite format 3\0")
}

/// Copy a Python database to the Rust storage root, preserving WAL/SHM files.
///
/// Returns the destination path if successful, or `None` if:
/// - The destination already exists (won't overwrite)
/// - The copy fails
///
/// # Errors
///
/// Returns `MigrationError` if filesystem operations fail critically.
pub fn copy_python_database_to_rust(
    python_db: &std::path::Path,
    rust_storage_root: &std::path::Path,
) -> Result<Option<std::path::PathBuf>, MigrationError> {
    let dest = rust_storage_root.join("storage.sqlite3");

    // Don't overwrite existing Rust DB
    if dest.exists() {
        return Ok(None);
    }

    // Create storage root if needed
    std::fs::create_dir_all(rust_storage_root).map_err(|e| {
        MigrationError::Aborted(format!(
            "cannot create storage root {}: {e}",
            rust_storage_root.display()
        ))
    })?;

    // Copy main DB file
    std::fs::copy(python_db, &dest).map_err(|e| {
        MigrationError::Aborted(format!(
            "cannot copy {} -> {}: {e}",
            python_db.display(),
            dest.display()
        ))
    })?;

    // Copy WAL and SHM if present
    let wal = python_db.with_extension("sqlite3-wal");
    if wal.exists() {
        let dest_wal = dest.with_extension("sqlite3-wal");
        let _ = std::fs::copy(&wal, &dest_wal);
    }
    let shm = python_db.with_extension("sqlite3-shm");
    if shm.exists() {
        let dest_shm = dest.with_extension("sqlite3-shm");
        let _ = std::fs::copy(&shm, &dest_shm);
    }

    Ok(Some(dest))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── text_to_micros tests ───────────────────────────────────────────

    fn parse(s: &str) -> Option<i64> {
        text_to_micros(s, "test", "col", 0).unwrap()
    }

    fn parse_err(s: &str) -> bool {
        text_to_micros(s, "test", "col", 0).is_err()
    }

    #[test]
    fn space_separator_with_microseconds() {
        let micros = parse("2026-02-24 15:30:00.123456").unwrap();
        // 2026-02-24 15:30:00.123456 UTC
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 2, 24)
            .unwrap()
            .and_hms_micro_opt(15, 30, 0, 123_456)
            .unwrap();
        assert_eq!(micros, crate::timestamps::naive_to_micros(expected));
    }

    #[test]
    fn t_separator_with_microseconds() {
        let m1 = parse("2026-02-24 15:30:00.123456").unwrap();
        let m2 = parse("2026-02-24T15:30:00.123456").unwrap();
        assert_eq!(m1, m2);
    }

    #[test]
    fn no_fractional_seconds() {
        let micros = parse("2026-02-24 15:30:00").unwrap();
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 2, 24)
            .unwrap()
            .and_hms_opt(15, 30, 0)
            .unwrap();
        assert_eq!(micros, crate::timestamps::naive_to_micros(expected));
    }

    #[test]
    fn date_only() {
        let micros = parse("2026-02-24").unwrap();
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 2, 24)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert_eq!(micros, crate::timestamps::naive_to_micros(expected));
    }

    #[test]
    fn epoch() {
        let micros = parse("1970-01-01 00:00:00").unwrap();
        assert_eq!(micros, 0);
    }

    #[test]
    fn pre_epoch() {
        let micros = parse("1969-12-31 23:59:59").unwrap();
        assert!(micros < 0, "pre-epoch should be negative: {micros}");
        assert_eq!(micros, -1_000_000); // -1 second in microseconds
    }

    #[test]
    fn empty_string_is_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("  "), None);
    }

    #[test]
    fn invalid_string_is_error() {
        assert!(parse_err("not-a-date"));
        assert!(parse_err("hello world"));
        assert!(parse_err("2026-13-45 99:99:99"));
    }

    #[test]
    fn with_timezone_utc() {
        // RFC 3339 with Z
        let micros = parse("2026-02-24T15:30:00.123456Z").unwrap();
        let no_tz = parse("2026-02-24T15:30:00.123456").unwrap();
        assert_eq!(micros, no_tz);
    }

    #[test]
    fn with_timezone_offset() {
        // RFC 3339 with +00:00
        let micros = parse("2026-02-24T15:30:00+00:00").unwrap();
        let no_tz = parse("2026-02-24T15:30:00").unwrap();
        assert_eq!(micros, no_tz);
    }

    #[test]
    fn roundtrip_python_to_rust_to_iso() {
        let python_ts = "2026-02-24 15:30:00.123456";
        let micros = parse(python_ts).unwrap();
        let iso = crate::timestamps::micros_to_iso(micros);
        // The ISO output uses T separator and Z suffix
        assert_eq!(iso, "2026-02-24T15:30:00.123456Z");
    }

    #[test]
    fn with_milliseconds() {
        // Python sometimes stores with 3 fractional digits instead of 6
        let micros = parse("2026-02-24 15:30:00.123").unwrap();
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 2, 24)
            .unwrap()
            .and_hms_milli_opt(15, 30, 0, 123)
            .unwrap();
        assert_eq!(micros, crate::timestamps::naive_to_micros(expected));
    }

    #[test]
    fn t_separator_no_fractional() {
        let m1 = parse("2026-02-24 15:30:00").unwrap();
        let m2 = parse("2026-02-24T15:30:00").unwrap();
        assert_eq!(m1, m2);
    }

    // ── strip_timezone_suffix tests ────────────────────────────────────

    #[test]
    fn strip_tz_z() {
        assert_eq!(
            strip_timezone_suffix("2026-02-24T15:30:00Z"),
            "2026-02-24T15:30:00"
        );
    }

    #[test]
    fn strip_tz_offset() {
        assert_eq!(
            strip_timezone_suffix("2026-02-24T15:30:00+00:00"),
            "2026-02-24T15:30:00"
        );
        assert_eq!(
            strip_timezone_suffix("2026-02-24T15:30:00-05:00"),
            "2026-02-24T15:30:00"
        );
    }

    #[test]
    fn strip_tz_noop() {
        assert_eq!(
            strip_timezone_suffix("2026-02-24 15:30:00"),
            "2026-02-24 15:30:00"
        );
    }

    // ── detect_timestamp_format tests ──────────────────────────────────

    #[test]
    fn detect_empty_database() {
        let conn = DbConn::open_memory().expect("open in-memory DB");
        conn.execute_raw(crate::schema::CREATE_TABLES_SQL)
            .expect("create tables");
        let format = detect_timestamp_format(&conn).expect("detect format");
        assert_eq!(format, TimestampFormat::Empty);
    }

    #[test]
    fn detect_rust_format() {
        let conn = DbConn::open_memory().expect("open in-memory DB");
        conn.execute_raw(crate::schema::CREATE_TABLES_SQL)
            .expect("create tables");
        // Insert a project with integer timestamp
        conn.query_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('test', '/tmp/test', 1740000000000000)",
            &[],
        )
        .expect("insert project");
        let format = detect_timestamp_format(&conn).expect("detect format");
        assert_eq!(format, TimestampFormat::RustMicros);
    }

    #[test]
    fn detect_python_format() {
        let conn = DbConn::open_memory().expect("open in-memory DB");
        conn.execute_raw(crate::schema::CREATE_TABLES_SQL)
            .expect("create tables");
        // Insert a project with TEXT timestamp (Python style)
        conn.query_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('test', '/tmp/test', '2026-02-24 15:30:00.123456')",
            &[],
        )
        .expect("insert project");
        let format = detect_timestamp_format(&conn).expect("detect format");
        assert_eq!(format, TimestampFormat::PythonText);
        assert!(format.needs_migration());
    }

    #[test]
    fn detect_mixed_format() {
        let conn = DbConn::open_memory().expect("open in-memory DB");
        conn.execute_raw(crate::schema::CREATE_TABLES_SQL)
            .expect("create tables");
        // Insert a project with INTEGER timestamp
        conn.query_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('test', '/tmp/test', 1740000000000000)",
            &[],
        )
        .expect("insert project");
        // Insert a product with TEXT timestamp
        conn.query_sync(
            "INSERT INTO products (product_uid, name, created_at) VALUES ('uid1', 'prod1', '2026-02-24 15:30:00')",
            &[],
        )
        .expect("insert product");
        let format = detect_timestamp_format(&conn).expect("detect format");
        match format {
            TimestampFormat::Mixed { text_tables } => {
                assert!(text_tables.contains(&"products".to_string()));
            }
            other => panic!("expected Mixed, got {other:?}"),
        }
    }

    #[test]
    fn needs_migration_variants() {
        assert!(!TimestampFormat::RustMicros.needs_migration());
        assert!(!TimestampFormat::Empty.needs_migration());
        assert!(TimestampFormat::PythonText.needs_migration());
        assert!(
            TimestampFormat::Mixed {
                text_tables: vec!["test".to_string()],
            }
            .needs_migration()
        );
        assert!(!TimestampFormat::Unknown("blob".to_string()).needs_migration());
    }

    // ── convert_column tests ───────────────────────────────────────────

    #[test]
    fn convert_column_text_to_integer() {
        let conn = DbConn::open_memory().expect("open in-memory DB");
        conn.execute_raw(crate::schema::CREATE_TABLES_SQL)
            .expect("create tables");

        // Insert projects with TEXT timestamps
        for i in 1..=5 {
            conn.query_sync(
                &format!(
                    "INSERT INTO projects (slug, human_key, created_at) VALUES ('p{i}', '/tmp/p{i}', '2026-02-{i:02} 10:00:00.000000')"
                ),
                &[],
            )
            .expect("insert");
        }

        let result = convert_column(&conn, "projects", "created_at").expect("convert");
        assert_eq!(result.converted, 5);
        assert_eq!(result.skipped, 0);

        // Verify conversion
        let format = detect_column_format(&conn, "projects", "created_at")
            .expect("detect")
            .unwrap();
        assert_eq!(format, "integer");
    }

    #[test]
    fn convert_column_preserves_nulls() {
        let conn = DbConn::open_memory().expect("open in-memory DB");
        conn.execute_raw(crate::schema::CREATE_TABLES_SQL)
            .expect("create tables");

        // Insert project and agent
        conn.query_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('test', '/tmp', 1740000000000000)",
            &[],
        )
        .expect("insert project");
        conn.query_sync(
            "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) VALUES (1, 'A', 'p', 'm', 1740000000000000, 1740000000000000)",
            &[],
        )
        .expect("insert agent");

        // Insert reservation with NULL released_ts (active reservation)
        conn.query_sync(
            "INSERT INTO file_reservations (project_id, agent_id, path_pattern, created_ts, expires_ts, released_ts) VALUES (1, 1, '*.rs', '2026-02-24 10:00:00', '2026-02-25 10:00:00', NULL)",
            &[],
        )
        .expect("insert reservation");

        let result = convert_column(&conn, "file_reservations", "released_ts").expect("convert");
        // released_ts is NULL, so no TEXT rows to convert
        assert_eq!(result.converted, 0);

        // But created_ts should convert
        let result2 = convert_column(&conn, "file_reservations", "created_ts").expect("convert");
        assert_eq!(result2.converted, 1);
    }

    #[test]
    fn convert_all_timestamps_full_migration() {
        let conn = DbConn::open_memory().expect("open in-memory DB");
        conn.execute_raw(crate::schema::CREATE_TABLES_SQL)
            .expect("create tables");

        // Insert Python-format data across multiple tables
        conn.query_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('test', '/tmp', '2026-02-24 15:30:00.123456')",
            &[],
        )
        .expect("insert project");
        conn.query_sync(
            "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) VALUES (1, 'A', 'p', 'm', '2026-02-24 15:30:00', '2026-02-24 16:00:00')",
            &[],
        )
        .expect("insert agent");
        conn.query_sync(
            "INSERT INTO messages (project_id, sender_id, subject, body_md, created_ts) VALUES (1, 1, 'test', 'body', '2026-02-24 15:30:00.000000')",
            &[],
        )
        .expect("insert message");

        // Verify starts as Python format
        let before = detect_timestamp_format(&conn).expect("detect");
        assert_eq!(before, TimestampFormat::PythonText);

        // Run full migration
        let summary = convert_all_timestamps(&conn).expect("migrate");
        assert!(summary.success);
        assert!(summary.total_converted > 0);
        assert_eq!(summary.total_skipped, 0);

        // Verify now in Rust format
        let after = detect_timestamp_format(&conn).expect("detect");
        assert_eq!(after, TimestampFormat::RustMicros);
    }

    #[test]
    fn convert_is_idempotent() {
        let conn = DbConn::open_memory().expect("open in-memory DB");
        conn.execute_raw(crate::schema::CREATE_TABLES_SQL)
            .expect("create tables");
        conn.query_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('test', '/tmp', '2026-02-24 15:30:00.123456')",
            &[],
        )
        .expect("insert");

        // First conversion
        let s1 = convert_all_timestamps(&conn).expect("migrate 1");
        assert!(s1.total_converted > 0);

        // Second conversion — nothing to convert
        let s2 = convert_all_timestamps(&conn).expect("migrate 2");
        assert_eq!(s2.total_converted, 0);
        assert!(s2.success);
    }

    #[test]
    fn display_format_variants() {
        // Just exercise Display impls to ensure they don't panic
        let _ = format!("{}", TimestampFormat::RustMicros);
        let _ = format!("{}", TimestampFormat::PythonText);
        let _ = format!("{}", TimestampFormat::Empty);
        let _ = format!(
            "{}",
            TimestampFormat::Mixed {
                text_tables: vec!["projects".to_string()],
            }
        );
        let _ = format!("{}", TimestampFormat::Unknown("blob".to_string()));
    }

    // ── is_sqlite_file tests ──────────────────────────────────────────

    #[test]
    fn is_sqlite_file_detects_valid_db() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("migrate_test_sqlite_header");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.sqlite3");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 84]).unwrap();
        drop(f);
        assert!(is_sqlite_file(&path));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn is_sqlite_file_rejects_non_sqlite() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("migrate_test_not_sqlite");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("not_a_db.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"This is not a SQLite file").unwrap();
        drop(f);
        assert!(!is_sqlite_file(&path));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn is_sqlite_file_nonexistent() {
        assert!(!is_sqlite_file(std::path::Path::new(
            "/nonexistent/path.db"
        )));
    }

    // ── find_python_database tests ────────────────────────────────────

    #[test]
    fn find_python_database_with_explicit_path() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("migrate_test_find_db");
        let _ = std::fs::create_dir_all(&dir);
        let db_path = dir.join("storage.sqlite3");
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 84]).unwrap();
        drop(f);

        let found = find_python_database(Some(&dir));
        assert_eq!(found, Some(db_path.clone()));

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn find_python_database_explicit_path_miss() {
        // Explicit path with no storage.sqlite3 won't match from that dir.
        // The function also probes well-known locations (~/mcp_agent_mail/...),
        // so we verify any match does NOT come from our temp dir.
        let dir = std::env::temp_dir().join("migrate_test_find_none");
        let _ = std::fs::create_dir_all(&dir);
        let found = find_python_database(Some(&dir));
        if let Some(ref path) = found {
            assert!(
                !path.starts_with(&dir),
                "should not have found a DB inside the empty temp dir"
            );
        }
        let _ = std::fs::remove_dir(&dir);
    }

    // ── copy_python_database_to_rust tests ────────────────────────────

    #[test]
    fn copy_database_to_rust_storage() {
        use std::io::Write;
        let base = std::env::temp_dir().join("migrate_test_copy_db");
        let src_dir = base.join("python");
        let dst_dir = base.join("rust_storage");
        let _ = std::fs::create_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&dst_dir);

        let src_db = src_dir.join("storage.sqlite3");
        let mut f = std::fs::File::create(&src_db).unwrap();
        f.write_all(b"SQLite format 3\0test data here").unwrap();
        drop(f);

        let result = copy_python_database_to_rust(&src_db, &dst_dir).unwrap();
        assert!(result.is_some());
        let dest = result.unwrap();
        assert!(dest.exists());
        assert_eq!(dest, dst_dir.join("storage.sqlite3"));

        let content = std::fs::read(&dest).unwrap();
        assert!(content.starts_with(b"SQLite format 3\0"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn copy_database_skips_if_rust_db_exists() {
        use std::io::Write;
        let base = std::env::temp_dir().join("migrate_test_copy_skip");
        let src_dir = base.join("python");
        let dst_dir = base.join("rust_storage");
        let _ = std::fs::create_dir_all(&src_dir);
        let _ = std::fs::create_dir_all(&dst_dir);

        let src_db = src_dir.join("storage.sqlite3");
        let mut f = std::fs::File::create(&src_db).unwrap();
        f.write_all(b"SQLite format 3\0python data").unwrap();
        drop(f);

        let dst_db = dst_dir.join("storage.sqlite3");
        let mut f2 = std::fs::File::create(&dst_db).unwrap();
        f2.write_all(b"SQLite format 3\0rust data").unwrap();
        drop(f2);

        let result = copy_python_database_to_rust(&src_db, &dst_dir).unwrap();
        assert!(result.is_none());

        let content = std::fs::read(&dst_db).unwrap();
        assert!(content.ends_with(b"rust data"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
