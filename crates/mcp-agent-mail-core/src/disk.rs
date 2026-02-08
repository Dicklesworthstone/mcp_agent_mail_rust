//! Disk space sampling and pressure classification.
//!
//! This module is used by background workers (HTTP/TUI server) to proactively
//! detect low-disk conditions and apply graceful degradation policies.

#![forbid(unsafe_code)]

use crate::Config;
use std::cmp;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bytes per MiB.
const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskPressure {
    Ok,
    Warning,
    Critical,
    Fatal,
}

impl DiskPressure {
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        match self {
            Self::Ok => 0,
            Self::Warning => 1,
            Self::Critical => 2,
            Self::Fatal => 3,
        }
    }

    #[must_use]
    pub const fn from_u64(v: u64) -> Self {
        match v {
            1 => Self::Warning,
            2 => Self::Critical,
            3 => Self::Fatal,
            _ => Self::Ok,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Critical => "critical",
            Self::Fatal => "fatal",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiskSample {
    /// The path used for the storage statvfs probe (directory or file).
    pub storage_probe_path: PathBuf,
    /// The path used for the DB statvfs probe (directory or file), when local.
    pub db_probe_path: Option<PathBuf>,

    pub storage_free_bytes: Option<u64>,
    pub db_free_bytes: Option<u64>,
    /// Minimum of the available free bytes across the known probe paths.
    pub effective_free_bytes: Option<u64>,

    pub pressure: DiskPressure,
    /// Best-effort errors encountered during sampling.
    pub errors: Vec<String>,
}

fn now_unix_micros_u64() -> u64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(dur.as_micros().min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

#[must_use]
pub const fn classify_pressure(
    free_bytes: u64,
    warning_mb: u64,
    critical_mb: u64,
    fatal_mb: u64,
) -> DiskPressure {
    let warning = warning_mb.saturating_mul(MIB);
    let critical = critical_mb.saturating_mul(MIB);
    let fatal = fatal_mb.saturating_mul(MIB);

    if fatal > 0 && free_bytes < fatal {
        DiskPressure::Fatal
    } else if critical > 0 && free_bytes < critical {
        DiskPressure::Critical
    } else if warning > 0 && free_bytes < warning {
        DiskPressure::Warning
    } else {
        DiskPressure::Ok
    }
}

fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(cmp::min(x, y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

fn normalize_probe_path(path: &Path) -> PathBuf {
    // statvfs typically requires the path to exist; probe the closest existing parent.
    if path.exists() {
        return path.to_path_buf();
    }
    let mut cur = path;
    while let Some(parent) = cur.parent() {
        if parent.as_os_str().is_empty() {
            break;
        }
        if parent.exists() {
            return parent.to_path_buf();
        }
        cur = parent;
    }
    PathBuf::from(".")
}

/// Return available bytes for the filesystem containing `path`.
///
/// Uses `fs2::available_space` (cross-platform) and never requires unsafe code.
pub fn disk_free_bytes(path: &Path) -> std::io::Result<u64> {
    fs2::available_space(path)
}

/// Parse a local `SQLite` file path from a database URL.
///
/// Supports the legacy Python form `sqlite+aiosqlite:///./path.db` as well as
/// common Rust/SQLAlchemy formats. Returns `None` for in-memory DBs or non-sqlite
/// URLs.
fn sqlite_path_component(database_url: &str) -> Option<&str> {
    let url = database_url.trim();
    let stripped = if let Some(rest) = url.strip_prefix("sqlite+aiosqlite://") {
        rest
    } else if let Some(rest) = url.strip_prefix("sqlite://") {
        rest
    } else {
        return None;
    };
    Some(stripped.split(['?', '#']).next().unwrap_or(stripped))
}

/// Return `true` when the database URL points to an in-memory `SQLite` database.
#[must_use]
pub fn is_sqlite_memory_database_url(database_url: &str) -> bool {
    matches!(
        sqlite_path_component(database_url),
        Some("/:memory:" | ":memory:")
    )
}

#[must_use]
pub fn sqlite_file_path_from_database_url(database_url: &str) -> Option<PathBuf> {
    let stripped = sqlite_path_component(database_url)?;

    if stripped.is_empty() {
        return None;
    }

    // In-memory DB.
    if is_sqlite_memory_database_url(database_url) {
        return None;
    }

    // After stripping, examples:
    // - /./path.db        -> ./path.db
    // - //abs/path.db     -> /abs/path.db
    // - /relative/path.db -> relative/path.db
    // - relative/path.db  -> relative/path.db
    let mut path = stripped.to_string();
    if path.starts_with("//") {
        // Absolute path (sqlite:////abs/path.db).
        path.remove(0);
    } else if path.starts_with('/') {
        // Relative path (sqlite:///relative/path.db).
        path.remove(0);
    }

    if path.is_empty() {
        return None;
    }

    Some(PathBuf::from(path))
}

/// Sample disk space for the key local paths (storage root and `SQLite` file, if
/// applicable) and classify pressure using the config thresholds.
#[must_use]
pub fn sample_disk(config: &Config) -> DiskSample {
    let storage_probe_path = normalize_probe_path(&config.storage_root);
    let db_path = sqlite_file_path_from_database_url(&config.database_url);
    let db_probe_path = db_path.as_deref().map(normalize_probe_path);

    let mut errors = Vec::new();

    let storage_free_bytes = match disk_free_bytes(&storage_probe_path) {
        Ok(v) => Some(v),
        Err(e) => {
            errors.push(format!(
                "statvfs(storage) failed path={} err={e}",
                storage_probe_path.display()
            ));
            None
        }
    };

    let db_free_bytes = db_probe_path
        .as_deref()
        .and_then(|p| match disk_free_bytes(p) {
            Ok(v) => Some(v),
            Err(e) => {
                errors.push(format!("statvfs(db) failed path={} err={e}", p.display()));
                None
            }
        });

    let effective_free_bytes = min_opt(storage_free_bytes, db_free_bytes);
    let pressure = effective_free_bytes.map_or(DiskPressure::Ok, |free| {
        classify_pressure(
            free,
            config.disk_space_warning_mb,
            config.disk_space_critical_mb,
            config.disk_space_fatal_mb,
        )
    });

    DiskSample {
        storage_probe_path,
        db_probe_path,
        storage_free_bytes,
        db_free_bytes,
        effective_free_bytes,
        pressure,
        errors,
    }
}

/// Sample disk space and update core system metrics gauges.
#[must_use]
pub fn sample_and_record(config: &Config) -> DiskSample {
    let sample = sample_disk(config);
    let metrics = crate::global_metrics();

    if let Some(bytes) = sample.storage_free_bytes {
        metrics.system.disk_storage_free_bytes.set(bytes);
    }
    if let Some(bytes) = sample.db_free_bytes {
        metrics.system.disk_db_free_bytes.set(bytes);
    }
    metrics
        .system
        .disk_effective_free_bytes
        .set(sample.effective_free_bytes.unwrap_or(0));
    metrics
        .system
        .disk_pressure_level
        .set(sample.pressure.as_u64());
    metrics
        .system
        .disk_last_sample_us
        .set(now_unix_micros_u64());
    if !sample.errors.is_empty() {
        metrics
            .system
            .disk_sample_errors_total
            .add(u64::try_from(sample.errors.len()).unwrap_or(u64::MAX));
    }

    sample
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_url_parsing_variants() {
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite+aiosqlite:///./storage.sqlite3")
                .unwrap()
                .to_string_lossy(),
            "./storage.sqlite3"
        );
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:///./storage.sqlite3")
                .unwrap()
                .to_string_lossy(),
            "./storage.sqlite3"
        );
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:///storage.sqlite3")
                .unwrap()
                .to_string_lossy(),
            "storage.sqlite3"
        );
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:///storage.sqlite3?mode=rwc")
                .unwrap()
                .to_string_lossy(),
            "storage.sqlite3"
        );
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:////abs/path.db")
                .unwrap()
                .to_string_lossy(),
            "/abs/path.db"
        );
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:////abs/path.db?cache=shared")
                .unwrap()
                .to_string_lossy(),
            "/abs/path.db"
        );
        assert!(sqlite_file_path_from_database_url("sqlite3:///storage.sqlite3").is_none());
        assert!(sqlite_file_path_from_database_url("sqlite:///:memory:").is_none());
        assert!(sqlite_file_path_from_database_url("sqlite:///:memory:?cache=shared").is_none());
        assert!(is_sqlite_memory_database_url("sqlite:///:memory:"));
        assert!(is_sqlite_memory_database_url(
            "sqlite:///:memory:?cache=shared"
        ));
        assert!(sqlite_file_path_from_database_url("postgres://localhost/db").is_none());
        assert!(!is_sqlite_memory_database_url("postgres://localhost/db"));
        // Edge case: bare sqlite:/// with no path after stripping → None
        assert!(sqlite_file_path_from_database_url("sqlite:///").is_none());
    }

    #[test]
    fn pressure_classification_thresholds() {
        let free = 600 * MIB;
        assert_eq!(classify_pressure(free, 500, 100, 10), DiskPressure::Ok);
        assert_eq!(
            classify_pressure(400 * MIB, 500, 100, 10),
            DiskPressure::Warning
        );
        assert_eq!(
            classify_pressure(50 * MIB, 500, 100, 10),
            DiskPressure::Critical
        );
        assert_eq!(
            classify_pressure(5 * MIB, 500, 100, 10),
            DiskPressure::Fatal
        );
    }
}
