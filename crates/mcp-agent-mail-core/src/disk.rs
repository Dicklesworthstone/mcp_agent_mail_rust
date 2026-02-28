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
    } else {
        url.strip_prefix("sqlite://")?
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

/// Read cumulative process I/O bytes from `/proc/self/io` (Linux).
///
/// Returns `(read_bytes, write_bytes)`. On non-Linux platforms, returns `(0, 0)`.
/// The `write_bytes` field corresponds to the kernel's `write_bytes` counter,
/// which tracks actual storage writes (post page-cache), giving a real signal
/// under SQLite + git archive workloads.
///
/// See: <https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/17>
#[must_use]
pub fn read_proc_io_bytes() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        let content = match std::fs::read_to_string("/proc/self/io") {
            Ok(c) => c,
            Err(_) => return (0, 0),
        };

        let mut read_bytes = 0u64;
        let mut write_bytes = 0u64;

        for line in content.lines() {
            if let Some(val) = line.strip_prefix("read_bytes: ") {
                read_bytes = val.trim().parse().unwrap_or(0);
            } else if let Some(val) = line.strip_prefix("write_bytes: ") {
                write_bytes = val.trim().parse().unwrap_or(0);
            }
        }

        (read_bytes, write_bytes)
    }

    #[cfg(not(target_os = "linux"))]
    {
        (0, 0)
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

    // Sample process I/O bytes (Linux only; 0 on other platforms).
    let (io_read, io_write) = read_proc_io_bytes();
    metrics.system.disk_io_read_bytes.set(io_read);
    metrics.system.disk_io_write_bytes.set(io_write);

    sample
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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

    #[test]
    fn min_opt_combinations() {
        assert_eq!(min_opt(Some(3), Some(9)), Some(3));
        assert_eq!(min_opt(Some(9), Some(3)), Some(3));
        assert_eq!(min_opt(Some(7), None), Some(7));
        assert_eq!(min_opt(None, Some(7)), Some(7));
        assert_eq!(min_opt(None, None), None);
    }

    #[test]
    fn normalize_probe_path_prefers_existing_parent_and_dot_fallback() {
        let tmp = tempdir().expect("tempdir should be created");
        let missing_leaf = tmp.path().join("missing").join("nested").join("db.sqlite3");
        assert_eq!(
            normalize_probe_path(&missing_leaf),
            tmp.path().to_path_buf()
        );

        let unique = PathBuf::from(format!(
            "definitely_missing_probe_path_{}",
            now_unix_micros_u64()
        ));
        assert!(
            !unique.exists(),
            "unique missing probe path unexpectedly exists: {}",
            unique.display()
        );
        assert_eq!(normalize_probe_path(&unique), PathBuf::from("."));
    }

    #[test]
    fn sample_disk_uses_effective_min_and_applies_thresholds() {
        let tmp = tempdir().expect("tempdir should be created");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("storage root should be created");

        let db_file = tmp.path().join("db").join("storage.sqlite3");
        std::fs::create_dir_all(
            db_file
                .parent()
                .expect("db file parent should exist after create_dir_all"),
        )
        .expect("db parent should be created");

        // Force warning classification for any realistic free-byte value.
        let config = Config {
            storage_root: storage_root.clone(),
            database_url: format!(
                "sqlite:////{}",
                db_file.to_string_lossy().trim_start_matches('/')
            ),
            disk_space_warning_mb: u64::MAX,
            disk_space_critical_mb: 0,
            disk_space_fatal_mb: 0,
            ..Config::default()
        };

        let sample = sample_disk(&config);
        assert_eq!(sample.storage_probe_path, storage_root);
        assert_eq!(
            sample.db_probe_path,
            db_file.parent().map(std::path::Path::to_path_buf)
        );
        assert!(sample.storage_free_bytes.is_some());
        assert!(sample.db_free_bytes.is_some());

        let storage_free = sample
            .storage_free_bytes
            .expect("storage free bytes expected");
        let db_free = sample.db_free_bytes.expect("db free bytes expected");
        assert_eq!(
            sample.effective_free_bytes,
            Some(std::cmp::min(storage_free, db_free))
        );
        assert_eq!(sample.pressure, DiskPressure::Warning);
        assert!(sample.errors.is_empty());
    }

    // ── DiskPressure enum coverage ──────────────────────────────────────

    #[test]
    fn disk_pressure_as_u64_roundtrip() {
        for &(variant, expected) in &[
            (DiskPressure::Ok, 0u64),
            (DiskPressure::Warning, 1),
            (DiskPressure::Critical, 2),
            (DiskPressure::Fatal, 3),
        ] {
            assert_eq!(variant.as_u64(), expected);
            assert_eq!(DiskPressure::from_u64(expected), variant);
        }
    }

    #[test]
    fn disk_pressure_from_u64_unknown_maps_to_ok() {
        // Any value outside 1..=3 maps to Ok (the catch-all)
        assert_eq!(DiskPressure::from_u64(4), DiskPressure::Ok);
        assert_eq!(DiskPressure::from_u64(255), DiskPressure::Ok);
        assert_eq!(DiskPressure::from_u64(u64::MAX), DiskPressure::Ok);
    }

    #[test]
    fn disk_pressure_label_covers_all_variants() {
        assert_eq!(DiskPressure::Ok.label(), "ok");
        assert_eq!(DiskPressure::Warning.label(), "warning");
        assert_eq!(DiskPressure::Critical.label(), "critical");
        assert_eq!(DiskPressure::Fatal.label(), "fatal");
    }

    #[test]
    fn disk_free_bytes_succeeds_on_existing_dir() {
        let dir = tempdir().unwrap();
        let bytes = disk_free_bytes(dir.path());
        assert!(
            bytes.is_ok(),
            "disk_free_bytes should succeed for existing dir"
        );
        assert!(bytes.unwrap() > 0, "available space should be > 0");
    }

    #[test]
    fn disk_free_bytes_fails_on_nonexistent_path() {
        let result = disk_free_bytes(Path::new("/nonexistent_path_that_does_not_exist_12345"));
        assert!(
            result.is_err(),
            "disk_free_bytes should fail for nonexistent path"
        );
    }

    #[test]
    fn classify_pressure_all_zeros_is_ok() {
        // When all thresholds are 0, everything is Ok
        assert_eq!(classify_pressure(0, 0, 0, 0), DiskPressure::Ok);
        assert_eq!(classify_pressure(1_000_000, 0, 0, 0), DiskPressure::Ok);
    }

    #[test]
    fn classify_pressure_saturating_mul_no_panic() {
        // Huge MB values should not overflow due to saturating_mul
        assert_eq!(
            classify_pressure(0, u64::MAX, u64::MAX, u64::MAX),
            DiskPressure::Fatal
        );
    }

    // ── br-3h13: Additional disk.rs test coverage ─────────────────

    #[test]
    fn sqlite_url_fragment_stripped() {
        // Fragment (#) should be stripped just like query (?)
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:///db.sqlite3#frag")
                .unwrap()
                .to_string_lossy(),
            "db.sqlite3"
        );
    }

    #[test]
    fn sqlite_url_aiosqlite_memory() {
        assert!(is_sqlite_memory_database_url(
            "sqlite+aiosqlite:///:memory:"
        ));
        assert!(sqlite_file_path_from_database_url("sqlite+aiosqlite:///:memory:").is_none());
    }

    #[test]
    fn sqlite_url_aiosqlite_absolute_path() {
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite+aiosqlite:////var/data/db.sqlite3")
                .unwrap()
                .to_string_lossy(),
            "/var/data/db.sqlite3"
        );
    }

    #[test]
    fn sqlite_path_component_bare_returns_none_for_non_sqlite() {
        assert!(sqlite_path_component("mysql://localhost/db").is_none());
        assert!(sqlite_path_component("postgres://host/db").is_none());
    }

    #[test]
    fn sqlite_path_component_strips_query_and_fragment() {
        assert_eq!(
            sqlite_path_component("sqlite:///path.db?mode=rwc#frag"),
            Some("/path.db")
        );
    }

    #[test]
    fn classify_pressure_exactly_at_warning_boundary() {
        // When free_bytes == warning threshold exactly, it's not below so should be Ok
        let threshold = 500;
        let at_threshold = threshold * MIB;
        assert_eq!(
            classify_pressure(at_threshold, threshold, 100, 10),
            DiskPressure::Ok
        );
        assert_eq!(
            classify_pressure(at_threshold - 1, threshold, 100, 10),
            DiskPressure::Warning
        );
    }

    #[test]
    fn classify_pressure_exactly_at_critical_boundary() {
        let threshold = 100;
        let at_threshold = threshold * MIB;
        assert_eq!(
            classify_pressure(at_threshold, 500, threshold, 10),
            DiskPressure::Warning // above critical but below warning
        );
        assert_eq!(
            classify_pressure(at_threshold - 1, 500, threshold, 10),
            DiskPressure::Critical
        );
    }

    #[test]
    fn classify_pressure_exactly_at_fatal_boundary() {
        let threshold = 10;
        let at_threshold = threshold * MIB;
        assert_eq!(
            classify_pressure(at_threshold, 500, 100, threshold),
            DiskPressure::Critical // above fatal but below critical
        );
        assert_eq!(
            classify_pressure(at_threshold - 1, 500, 100, threshold),
            DiskPressure::Fatal
        );
    }

    #[test]
    fn sample_disk_with_memory_database_url() {
        let tmp = tempdir().expect("tempdir");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();

        let config = Config {
            storage_root,
            database_url: "sqlite:///:memory:".to_string(),
            disk_space_warning_mb: 0,
            disk_space_critical_mb: 0,
            disk_space_fatal_mb: 0,
            ..Config::default()
        };

        let sample = sample_disk(&config);
        assert!(sample.storage_free_bytes.is_some());
        assert!(
            sample.db_probe_path.is_none(),
            "memory DB has no probe path"
        );
        assert!(sample.db_free_bytes.is_none());
        // effective should be storage-only
        assert_eq!(sample.effective_free_bytes, sample.storage_free_bytes);
    }

    #[test]
    fn normalize_probe_path_existing_file() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("existing.db");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(normalize_probe_path(&file), file);
    }

    #[test]
    fn disk_sample_clone() {
        let sample = DiskSample {
            storage_probe_path: PathBuf::from("/tmp"),
            db_probe_path: Some(PathBuf::from("/var")),
            storage_free_bytes: Some(1000),
            db_free_bytes: Some(2000),
            effective_free_bytes: Some(1000),
            pressure: DiskPressure::Warning,
            errors: vec!["test error".to_string()],
        };
        let cloned = sample.clone();
        assert_eq!(cloned.pressure, DiskPressure::Warning);
        assert_eq!(cloned.effective_free_bytes, Some(1000));
        assert_eq!(cloned.errors.len(), 1);
        // Use `sample` after clone to prove it produced an independent copy.
        assert_eq!(sample.errors.len(), 1);
    }

    #[test]
    fn sample_and_record_updates_disk_metrics() {
        let tmp = tempdir().expect("tempdir should be created");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("storage root should be created");

        let db_file = tmp.path().join("db").join("storage.sqlite3");
        std::fs::create_dir_all(
            db_file
                .parent()
                .expect("db file parent should exist after create_dir_all"),
        )
        .expect("db parent should be created");

        let config = Config {
            storage_root,
            database_url: format!(
                "sqlite:////{}",
                db_file.to_string_lossy().trim_start_matches('/')
            ),
            disk_space_warning_mb: 0,
            disk_space_critical_mb: 0,
            disk_space_fatal_mb: 0,
            ..Config::default()
        };

        let metrics = crate::global_metrics();
        metrics.system.disk_storage_free_bytes.set(0);
        metrics.system.disk_db_free_bytes.set(0);
        metrics.system.disk_effective_free_bytes.set(0);
        metrics.system.disk_pressure_level.set(0);
        metrics.system.disk_last_sample_us.set(0);
        metrics.system.disk_sample_errors_total.store(0);

        let sample = sample_and_record(&config);
        assert_eq!(sample.pressure, DiskPressure::Ok);

        if let Some(storage_free) = sample.storage_free_bytes {
            assert_eq!(metrics.system.disk_storage_free_bytes.load(), storage_free);
        }
        if let Some(db_free) = sample.db_free_bytes {
            assert_eq!(metrics.system.disk_db_free_bytes.load(), db_free);
        }
        assert_eq!(
            metrics.system.disk_effective_free_bytes.load(),
            sample.effective_free_bytes.unwrap_or(0)
        );
        assert_eq!(
            metrics.system.disk_pressure_level.load(),
            sample.pressure.as_u64()
        );
        assert!(metrics.system.disk_last_sample_us.load() > 0);
        assert_eq!(
            metrics.system.disk_sample_errors_total.load(),
            u64::try_from(sample.errors.len()).expect("error count should fit u64")
        );
    }

    #[test]
    fn read_proc_io_bytes_returns_non_zero_on_linux() {
        let (read, write) = read_proc_io_bytes();
        // On Linux, the test process itself has done I/O, so at least read > 0.
        // On non-Linux, both are 0 (no /proc/self/io).
        #[cfg(target_os = "linux")]
        {
            assert!(read > 0, "process should have read bytes on Linux");
            // write may be 0 in some CI envs if no fsyncs have happened yet.
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert_eq!(read, 0);
            assert_eq!(write, 0);
        }
        // Suppress unused variable warning.
        let _ = write;
    }

    #[test]
    fn sample_and_record_updates_io_bytes_metrics() {
        let tmp = tempdir().expect("tempdir should be created");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("storage root should be created");

        let config = Config {
            storage_root,
            database_url: "sqlite:///:memory:".to_string(),
            disk_space_warning_mb: 0,
            disk_space_critical_mb: 0,
            disk_space_fatal_mb: 0,
            ..Config::default()
        };

        let metrics = crate::global_metrics();
        metrics.system.disk_io_read_bytes.set(0);
        metrics.system.disk_io_write_bytes.set(0);

        let _sample = sample_and_record(&config);

        // On Linux, the I/O gauges should have been updated.
        #[cfg(target_os = "linux")]
        {
            assert!(
                metrics.system.disk_io_read_bytes.load() > 0,
                "disk_io_read_bytes should be updated after sample_and_record on Linux"
            );
        }
    }
}
