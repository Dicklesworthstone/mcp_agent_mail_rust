//! Process memory (RSS) sampling and pressure classification.
//!
//! Mirrors `disk.rs`: reads `/proc/self/status` on Linux (zero-cost, no unsafe)
//! and returns a classified pressure level used by the background health worker
//! to trigger adaptive cache eviction and load shedding.

use crate::Config;
use std::time::{SystemTime, UNIX_EPOCH};

/// Bytes per MiB.
const MIB: u64 = 1024 * 1024;

/// Memory pressure levels, matching `DiskPressure` semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPressure {
    /// RSS below warning threshold — normal operation.
    Ok,
    /// RSS above warning threshold — log warning, consider reducing cache TTL.
    Warning,
    /// RSS above critical threshold — evict cache, increase drain rates.
    Critical,
    /// RSS above fatal threshold — reject new work, shed load.
    Fatal,
}

impl MemoryPressure {
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

/// Snapshot of process memory state at a point in time.
#[derive(Debug, Clone)]
pub struct MemorySample {
    /// Resident Set Size in bytes (physical RAM used by this process).
    pub rss_bytes: Option<u64>,
    /// Classified pressure level based on config thresholds.
    pub pressure: MemoryPressure,
    /// Best-effort error if RSS could not be read.
    pub error: Option<String>,
}

/// Classify memory pressure from RSS bytes and config thresholds (in MB).
/// A threshold of 0 means that level is disabled.
#[must_use]
pub const fn classify_pressure(
    rss_bytes: u64,
    warning_mb: u64,
    critical_mb: u64,
    fatal_mb: u64,
) -> MemoryPressure {
    let fatal = fatal_mb.saturating_mul(MIB);
    let critical = critical_mb.saturating_mul(MIB);
    let warning = warning_mb.saturating_mul(MIB);

    if fatal > 0 && rss_bytes > fatal {
        MemoryPressure::Fatal
    } else if critical > 0 && rss_bytes > critical {
        MemoryPressure::Critical
    } else if warning > 0 && rss_bytes > warning {
        MemoryPressure::Warning
    } else {
        MemoryPressure::Ok
    }
}

/// Read current process RSS from `/proc/self/status` (Linux).
///
/// Parses the `VmRSS:` line and converts kB to bytes.
/// Returns `None` with an error string on non-Linux platforms or if the file
/// cannot be parsed.
pub(crate) fn read_rss_bytes() -> Result<u64, String> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status")
            .map_err(|e| format!("read /proc/self/status: {e}"))?;

        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let trimmed = rest.trim();
                // Format: "123456 kB"
                let kb_str = trimmed
                    .strip_suffix("kB")
                    .or_else(|| trimmed.strip_suffix("KB"))
                    .unwrap_or(trimmed)
                    .trim();
                let kb: u64 = kb_str
                    .parse()
                    .map_err(|e| format!("parse VmRSS '{kb_str}': {e}"))?;
                return Ok(kb * 1024);
            }
        }
        Err("VmRSS line not found in /proc/self/status".to_string())
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err("RSS reading not implemented on this platform".to_string())
    }
}

/// Sample current process memory usage and classify pressure.
#[must_use]
pub fn sample_memory(config: &Config) -> MemorySample {
    match read_rss_bytes() {
        Ok(rss_bytes) => {
            let pressure = classify_pressure(
                rss_bytes,
                config.memory_warning_mb,
                config.memory_critical_mb,
                config.memory_fatal_mb,
            );
            MemorySample {
                rss_bytes: Some(rss_bytes),
                pressure,
                error: None,
            }
        }
        Err(e) => MemorySample {
            rss_bytes: None,
            pressure: MemoryPressure::Ok,
            error: Some(e),
        },
    }
}

fn now_unix_micros_u64() -> u64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(dur.as_micros().min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

/// Sample memory and update global system metrics gauges.
#[must_use]
pub fn sample_and_record(config: &Config) -> MemorySample {
    let sample = sample_memory(config);
    let metrics = crate::global_metrics();

    if let Some(rss) = sample.rss_bytes {
        metrics.system.memory_rss_bytes.set(rss);
    }
    metrics
        .system
        .memory_pressure_level
        .set(sample.pressure.as_u64());
    metrics
        .system
        .memory_last_sample_us
        .set(now_unix_micros_u64());
    if sample.error.is_some() {
        metrics.system.memory_sample_errors_total.add(1);
    }

    sample
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_classification_thresholds() {
        // All thresholds in MB, RSS in bytes
        assert_eq!(
            classify_pressure(500 * MIB, 2048, 4096, 8192),
            MemoryPressure::Ok
        );
        assert_eq!(
            classify_pressure(3000 * MIB, 2048, 4096, 8192),
            MemoryPressure::Warning
        );
        assert_eq!(
            classify_pressure(5000 * MIB, 2048, 4096, 8192),
            MemoryPressure::Critical
        );
        assert_eq!(
            classify_pressure(9000 * MIB, 2048, 4096, 8192),
            MemoryPressure::Fatal
        );
    }

    #[test]
    fn pressure_disabled_thresholds() {
        // Threshold of 0 means disabled
        assert_eq!(classify_pressure(10_000 * MIB, 0, 0, 0), MemoryPressure::Ok,);
        // Only warning enabled
        assert_eq!(
            classify_pressure(3000 * MIB, 2048, 0, 0),
            MemoryPressure::Warning,
        );
    }

    #[test]
    fn pressure_label_roundtrip() {
        for (level, expected) in [
            (MemoryPressure::Ok, "ok"),
            (MemoryPressure::Warning, "warning"),
            (MemoryPressure::Critical, "critical"),
            (MemoryPressure::Fatal, "fatal"),
        ] {
            assert_eq!(level.label(), expected);
            assert_eq!(MemoryPressure::from_u64(level.as_u64()), level);
        }
    }

    #[test]
    fn pressure_u64_roundtrip() {
        for v in 0..=3 {
            let p = MemoryPressure::from_u64(v);
            assert_eq!(p.as_u64(), v);
        }
        // Unknown values default to Ok
        assert_eq!(MemoryPressure::from_u64(99), MemoryPressure::Ok);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_rss_returns_nonzero() {
        let rss = read_rss_bytes().expect("should read RSS on Linux");
        assert!(rss > 0, "RSS should be > 0, got {rss}");
        // Sanity check: a Rust test process should use at least 1MB
        assert!(rss > MIB, "RSS {rss} seems too small");
    }

    #[test]
    fn sample_memory_with_default_config() {
        let config = Config::default();
        let sample = sample_memory(&config);
        // On Linux, rss_bytes should be Some; on other platforms, error is expected
        if cfg!(target_os = "linux") {
            assert!(sample.rss_bytes.is_some());
            assert!(sample.error.is_none());
        }
    }

    #[test]
    fn sample_and_record_updates_memory_metrics() {
        let config = Config::default();
        let metrics = crate::global_metrics();
        metrics.system.memory_rss_bytes.set(0);
        metrics.system.memory_pressure_level.set(0);
        metrics.system.memory_last_sample_us.set(0);
        metrics.system.memory_sample_errors_total.store(0);

        let sample = sample_and_record(&config);

        assert_eq!(
            metrics.system.memory_pressure_level.load(),
            sample.pressure.as_u64()
        );
        assert!(metrics.system.memory_last_sample_us.load() > 0);

        if let Some(rss) = sample.rss_bytes {
            assert_eq!(metrics.system.memory_rss_bytes.load(), rss);
            assert_eq!(metrics.system.memory_sample_errors_total.load(), 0);
        } else {
            assert_eq!(metrics.system.memory_rss_bytes.load(), 0);
            assert_eq!(metrics.system.memory_sample_errors_total.load(), 1);
            assert!(sample.error.is_some());
        }
    }
}
