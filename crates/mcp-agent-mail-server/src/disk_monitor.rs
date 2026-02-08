//! Background worker for disk space monitoring and pressure classification.
//!
//! Updates core system metrics so operators can see disk free space and the
//! current pressure tier in `health_check` and `resource://tooling/metrics_core`.

#![forbid(unsafe_code)]

use mcp_agent_mail_core::Config;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WORKER: OnceLock<std::thread::JoinHandle<()>> = OnceLock::new();

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
    const STARTUP_WARN_BYTES: u64 = 1024 * 1024 * 1024; // 1GiB

    let interval = Duration::from_secs(config.disk_space_check_interval_seconds.max(5));
    tracing::info!(
        interval_secs = interval.as_secs(),
        "disk monitor worker started"
    );

    let first = mcp_agent_mail_core::disk::sample_and_record(config);
    let mut last_pressure = first.pressure;
    if let Some(free) = first.effective_free_bytes {
        if free < STARTUP_WARN_BYTES {
            tracing::warn!(
                free_bytes = free,
                pressure = last_pressure.label(),
                "low disk space detected (startup warning threshold)"
            );
        }
    }

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            tracing::info!("disk monitor worker shutting down");
            return;
        }

        let sample = mcp_agent_mail_core::disk::sample_and_record(config);
        let pressure = sample.pressure;

        if pressure != last_pressure {
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
