//! Background worker for disk space monitoring and pressure classification.
//!
//! Updates core system metrics so operators can see disk free space and the
//! current pressure tier in `health_check` and `resource://tooling/metrics_core`.

#![forbid(unsafe_code)]

use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::disk::DiskPressure;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WORKER: OnceLock<std::thread::JoinHandle<()>> = OnceLock::new();
const STARTUP_WARN_BYTES: u64 = 1024 * 1024 * 1024; // 1GiB

#[inline]
fn monitor_interval_seconds(seconds: u64) -> Duration {
    Duration::from_secs(seconds.max(5))
}

#[inline]
fn should_emit_startup_warning(effective_free_bytes: Option<u64>) -> bool {
    effective_free_bytes.is_some_and(|free| free < STARTUP_WARN_BYTES)
}

#[inline]
fn should_emit_pressure_change_alert(previous: DiskPressure, current: DiskPressure) -> bool {
    previous != current
}

pub fn start(config: &Config) {
    if !config.disk_space_monitor_enabled {
        return;
    }

    // Seed the gauges synchronously so tool paths can consult disk pressure
    // immediately after startup.
    let _ = mcp_agent_mail_core::disk::sample_and_record(config);

    let config = config.clone();
    let _ = WORKER.get_or_init(|| {
        SHUTDOWN.store(false, Ordering::Release);
        std::thread::Builder::new()
            .name("disk-monitor".into())
            .spawn(move || monitor_loop(&config))
            .expect("failed to spawn disk monitor worker")
    });
}

pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
}

fn monitor_loop(config: &Config) {
    let interval = monitor_interval_seconds(config.disk_space_check_interval_seconds);
    tracing::info!(
        interval_secs = interval.as_secs(),
        "disk monitor worker started"
    );

    let first = mcp_agent_mail_core::disk::sample_and_record(config);
    let mut last_pressure = first.pressure;
    if should_emit_startup_warning(first.effective_free_bytes) {
        tracing::warn!(
            free_bytes = first.effective_free_bytes,
            pressure = last_pressure.label(),
            "low disk space detected (startup warning threshold)"
        );
    }

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            tracing::info!("disk monitor worker shutting down");
            return;
        }

        let sample = mcp_agent_mail_core::disk::sample_and_record(config);
        let pressure = sample.pressure;

        if should_emit_pressure_change_alert(last_pressure, pressure) {
            tracing::warn!(
                from = last_pressure.label(),
                to = pressure.label(),
                storage_free_bytes = sample.storage_free_bytes,
                db_free_bytes = sample.db_free_bytes,
                effective_free_bytes = sample.effective_free_bytes,
                "disk pressure level changed"
            );
            last_pressure = pressure;
        }

        // Sleep in small increments to allow quick shutdown.
        let mut remaining = interval;
        while !remaining.is_zero() {
            if SHUTDOWN.load(Ordering::Acquire) {
                return;
            }
            let chunk = remaining.min(Duration::from_secs(1));
            std::thread::sleep(chunk);
            remaining = remaining.saturating_sub(chunk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_interval_seconds_enforces_minimum() {
        assert_eq!(monitor_interval_seconds(0), Duration::from_secs(5));
        assert_eq!(monitor_interval_seconds(1), Duration::from_secs(5));
        assert_eq!(monitor_interval_seconds(4), Duration::from_secs(5));
        assert_eq!(monitor_interval_seconds(5), Duration::from_secs(5));
        assert_eq!(monitor_interval_seconds(7), Duration::from_secs(7));
    }

    #[test]
    fn startup_warning_threshold_behavior() {
        assert!(should_emit_startup_warning(Some(STARTUP_WARN_BYTES - 1)));
        assert!(!should_emit_startup_warning(Some(STARTUP_WARN_BYTES)));
        assert!(!should_emit_startup_warning(Some(STARTUP_WARN_BYTES + 1)));
        assert!(!should_emit_startup_warning(None));
    }

    #[test]
    fn startup_warning_zero_free_space_triggers() {
        assert!(should_emit_startup_warning(Some(0)));
    }

    #[test]
    fn pressure_change_alert_only_when_level_changes() {
        assert!(!should_emit_pressure_change_alert(
            DiskPressure::Ok,
            DiskPressure::Ok
        ));
        assert!(!should_emit_pressure_change_alert(
            DiskPressure::Warning,
            DiskPressure::Warning
        ));
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Ok,
            DiskPressure::Warning
        ));
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Critical,
            DiskPressure::Fatal
        ));
    }

    #[test]
    fn warning_to_critical_transition_alerts() {
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Warning,
            DiskPressure::Critical
        ));
    }

    #[test]
    fn rapid_fluctuation_alerts_every_transition() {
        let sequence = [
            DiskPressure::Ok,
            DiskPressure::Warning,
            DiskPressure::Critical,
            DiskPressure::Warning,
            DiskPressure::Warning,
            DiskPressure::Ok,
        ];
        let transitions: Vec<bool> = sequence
            .windows(2)
            .map(|window| should_emit_pressure_change_alert(window[0], window[1]))
            .collect();
        assert_eq!(transitions, vec![true, true, true, false, true]);
    }

    #[test]
    fn all_disk_pressure_variants_same_level_no_alert() {
        let all = [
            DiskPressure::Ok,
            DiskPressure::Warning,
            DiskPressure::Critical,
            DiskPressure::Fatal,
        ];
        for p in &all {
            assert!(
                !should_emit_pressure_change_alert(*p, *p),
                "same-level {p:?} should not alert"
            );
        }
    }

    #[test]
    fn recovery_transitions_all_alerted() {
        // Fatal → Critical → Warning → Ok: each step is a recovery alert.
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Fatal,
            DiskPressure::Critical
        ));
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Critical,
            DiskPressure::Warning
        ));
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Warning,
            DiskPressure::Ok
        ));
        // Big jumps also alert.
        assert!(should_emit_pressure_change_alert(
            DiskPressure::Fatal,
            DiskPressure::Ok
        ));
    }

    #[test]
    fn monitor_interval_large_value_preserved() {
        assert_eq!(
            monitor_interval_seconds(u64::MAX),
            Duration::from_secs(u64::MAX)
        );
        assert_eq!(monitor_interval_seconds(3600), Duration::from_hours(1));
    }

    #[test]
    fn startup_warning_u64_max_no_warning() {
        // Very large free space should never trigger a warning.
        assert!(!should_emit_startup_warning(Some(u64::MAX)));
    }

    #[test]
    fn startup_warning_handles_unmounted_storage() {
        // When storage paths don't exist, normalize_probe_path falls back to
        // the root "/" (or "." on some systems) which still provides valid
        // disk stats. The system gracefully degrades rather than failing.
        let mut config = Config::from_env();
        config.storage_root =
            std::path::PathBuf::from("/definitely/nonexistent/mcp-agent-mail-root");
        config.database_url = "sqlite:///definitely/nonexistent/mcp-agent-mail.sqlite3".to_string();

        let sample = mcp_agent_mail_core::disk::sample_and_record(&config);

        // The fallback path mechanism means we still get a disk sample from
        // an existing parent directory (usually "/" or ".").
        // This is intentional: always provide best-effort monitoring.
        assert!(
            sample.effective_free_bytes.is_some(),
            "fallback probe paths should still produce disk stats"
        );

        // Verify the fallback doesn't erroneously trigger warnings when there's
        // plenty of disk space (which "/" typically has).
        // This test may be flaky on systems with <1GB free space.
        if sample.effective_free_bytes.unwrap_or(0) >= STARTUP_WARN_BYTES {
            assert!(
                !should_emit_startup_warning(sample.effective_free_bytes),
                "fallback with sufficient space should not trigger warning"
            );
        }
    }
}
