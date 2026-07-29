//! Host-pressure health sampling — "this is *not* Agent Mail".
//!
//! Agents repeatedly misclassified host-level resource exhaustion (full disk,
//! exhausted inodes, an overloaded scheduler, memory pressure, an unwritable
//! data directory) as *mailbox corruption*. Under extreme host load the `SQLite`
//! and search probes time out, big WALs pile up, and the natural-but-wrong
//! conclusion is "Agent Mail is broken." It usually is not — the *host* is.
//!
//! This module provides a cheap, bounded, best-effort host-pressure section for
//! `am robot health --include-host`. It never *fixes* host pressure; it only
//! stops the misattribution by surfacing the evidence and emitting a single
//! conservative verdict (`host_pressure_likely`) that fires **only** on concrete
//! threshold breaches — never as a catch-all.
//!
//! Design notes:
//! - `#![forbid(unsafe_code)]` is honored: filesystem stats come from `fs2`
//!   (cross-platform) and `nix::sys::statvfs` (safe wrapper, Unix only); load
//!   average and host memory come from `/proc` parsing on Linux. Every signal is
//!   best-effort: an unavailable signal is `None` and never inflates the verdict.
//! - The verdict logic is a pure function ([`evaluate`]) over a plain
//!   [`HostHealthInputs`] struct so it can be unit-tested with simulated
//!   low-disk / high-load / inode-exhausted inputs without touching the host.

use crate::Config;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Convert any unsigned-ish integer into a `u64` without `as`-casts (keeps the
/// crate clippy-clean across platforms where `fsfilcnt_t`/`fsblkcnt_t` width
/// differs).
fn to_u64<T: TryInto<u64>>(v: T) -> Option<u64> {
    v.try_into().ok()
}

/// Coarse host-pressure severity, mirroring [`crate::disk::DiskPressure`]
/// semantics but limited to the three levels the verdict needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostPressure {
    /// No threshold breached (or no evidence available).
    Ok,
    /// At least one signal crossed its warning threshold.
    Warning,
    /// At least one signal crossed its critical threshold.
    Critical,
}

impl HostPressure {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }

    #[must_use]
    const fn max(self, other: Self) -> Self {
        // `Ord` is derived, but a `const fn` keeps this usable in const contexts
        // and avoids importing `cmp::max`.
        if (self as u8) >= (other as u8) {
            self
        } else {
            other
        }
    }
}

/// Thresholds governing when a host signal is considered "pressured".
///
/// Percentages are of the relevant total (free disk %, free inodes %, available
/// memory %). Load is expressed *per CPU* (`load1m / cpu_count`). A threshold of
/// `0` for a percentage floor or load ceiling disables that level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostHealthThresholds {
    /// Warn when free disk drops below this percent of total.
    pub disk_free_warn_pct: f64,
    /// Critical when free disk drops below this percent of total.
    pub disk_free_crit_pct: f64,
    /// Warn when free inodes drop below this percent of total.
    pub inodes_free_warn_pct: f64,
    /// Critical when free inodes drop below this percent of total.
    pub inodes_free_crit_pct: f64,
    /// Warn when 1-minute load per CPU exceeds this.
    pub load_per_cpu_warn: f64,
    /// Critical when 1-minute load per CPU exceeds this.
    pub load_per_cpu_crit: f64,
    /// Warn when available memory drops below this percent of total.
    pub mem_available_warn_pct: f64,
    /// Critical when available memory drops below this percent of total.
    pub mem_available_crit_pct: f64,
    /// Warn when the WAL has not been modified for this many seconds (stale).
    pub wal_stale_warn_secs: u64,
    /// Critical when the WAL has not been modified for this many seconds.
    pub wal_stale_crit_secs: u64,
}

impl Default for HostHealthThresholds {
    fn default() -> Self {
        Self {
            disk_free_warn_pct: 10.0,
            disk_free_crit_pct: 3.0,
            inodes_free_warn_pct: 10.0,
            inodes_free_crit_pct: 3.0,
            load_per_cpu_warn: 4.0,
            load_per_cpu_crit: 8.0,
            mem_available_warn_pct: 10.0,
            mem_available_crit_pct: 3.0,
            wal_stale_warn_secs: 300,
            wal_stale_crit_secs: 1800,
        }
    }
}

impl HostHealthThresholds {
    /// Build thresholds from the environment, falling back to [`Default`] for any
    /// unset/invalid override. Only a small, bounded set of knobs is exposed.
    #[must_use]
    pub fn from_env() -> Self {
        let mut t = Self::default();
        if let Some(v) = env_f64("AM_HOST_DISK_FREE_WARN_PCT") {
            t.disk_free_warn_pct = v;
        }
        if let Some(v) = env_f64("AM_HOST_DISK_FREE_CRIT_PCT") {
            t.disk_free_crit_pct = v;
        }
        if let Some(v) = env_f64("AM_HOST_INODES_FREE_WARN_PCT") {
            t.inodes_free_warn_pct = v;
        }
        if let Some(v) = env_f64("AM_HOST_INODES_FREE_CRIT_PCT") {
            t.inodes_free_crit_pct = v;
        }
        if let Some(v) = env_f64("AM_HOST_LOAD_PER_CPU_WARN") {
            t.load_per_cpu_warn = v;
        }
        if let Some(v) = env_f64("AM_HOST_LOAD_PER_CPU_CRIT") {
            t.load_per_cpu_crit = v;
        }
        if let Some(v) = env_f64("AM_HOST_MEM_AVAIL_WARN_PCT") {
            t.mem_available_warn_pct = v;
        }
        if let Some(v) = env_f64("AM_HOST_MEM_AVAIL_CRIT_PCT") {
            t.mem_available_crit_pct = v;
        }
        if let Some(v) = env_u64("AM_HOST_WAL_STALE_WARN_SECS") {
            t.wal_stale_warn_secs = v;
        }
        if let Some(v) = env_u64("AM_HOST_WAL_STALE_CRIT_SECS") {
            t.wal_stale_crit_secs = v;
        }
        t
    }
}

fn env_f64(key: &str) -> Option<f64> {
    std::env::var(key).ok()?.trim().parse::<f64>().ok()
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse::<u64>().ok()
}

/// Raw, best-effort host samples. Every field is `Option` so a probe failure or
/// unsupported platform degrades to "unknown" rather than a false verdict.
#[derive(Debug, Clone, Default)]
pub struct HostHealthInputs {
    /// Free bytes on the filesystem holding the data directory.
    pub disk_free_bytes: Option<u64>,
    /// Total bytes on that filesystem.
    pub disk_total_bytes: Option<u64>,
    /// Available inodes on that filesystem.
    pub inodes_free: Option<u64>,
    /// Total inodes on that filesystem.
    pub inodes_total: Option<u64>,
    /// 1-minute load average.
    pub load_avg_1m: Option<f64>,
    /// Logical CPU count.
    pub cpu_count: Option<u64>,
    /// Host available memory in bytes (Linux `MemAvailable`).
    pub mem_available_bytes: Option<u64>,
    /// Host total memory in bytes (Linux `MemTotal`).
    pub mem_total_bytes: Option<u64>,
    /// Whether the data directory accepts a probe write.
    pub db_dir_writable: Option<bool>,
    /// Detail when the writability probe failed.
    pub db_dir_write_error: Option<String>,
    /// Size of the `SQLite` database file in bytes.
    pub db_file_bytes: Option<u64>,
    /// Size of the `-wal` sidecar in bytes.
    pub wal_file_bytes: Option<u64>,
    /// Size of the `-shm` sidecar in bytes.
    pub shm_file_bytes: Option<u64>,
    /// Seconds since the WAL was last modified (staleness proxy).
    pub wal_age_secs: Option<u64>,
    /// Recorded writer/recovery-lock PID, if any.
    pub writer_pid: Option<u32>,
    /// Whether that recorded PID is currently alive.
    pub writer_pid_alive: Option<bool>,
    /// Path that was probed (for diagnostics).
    pub probe_path: Option<String>,
    /// Best-effort collection errors.
    pub errors: Vec<String>,
}

/// Computed host-health section, serialized into `am robot health` JSON/TOON.
#[derive(Debug, Clone, Serialize)]
pub struct HostHealthReport {
    /// Worst-of severity across all signals (`ok`/`warning`/`critical`).
    pub status: String,
    /// Conservative verdict: `true` only when at least one resource-pressure
    /// signal breached a threshold. Never fires on missing data alone.
    pub host_pressure_likely: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_free_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inodes_free: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inodes_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inodes_free_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_avg_1m: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_per_cpu: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_available_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_available_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_dir_writable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_file_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wal_file_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shm_file_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wal_age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writer_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writer_pid_alive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_path: Option<String>,
    /// Concrete threshold-breach explanations backing the verdict.
    pub reasons: Vec<String>,
    /// Best-effort collection errors.
    pub errors: Vec<String>,
}

/// Round a float to two decimal places for stable serialized output.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Compute `numerator / denominator * 100`, guarding division by zero.
fn pct(numerator: u64, denominator: u64) -> Option<f64> {
    if denominator == 0 {
        return None;
    }
    // Lossless within realistic disk/inode/memory magnitudes; precision-only.
    #[allow(clippy::cast_precision_loss)]
    Some(round2(numerator as f64 / denominator as f64 * 100.0))
}

/// Pure verdict function over raw samples.
///
/// Easy to unit-test with simulated pressure. The verdict
/// (`host_pressure_likely`) fires only when a *resource-pressure* signal
/// (disk/inodes/load/memory) breaches a threshold; informational signals
/// (writer PID, file sizes, stale WAL, dir writability) never set it.
#[must_use]
pub fn evaluate(inputs: &HostHealthInputs, thresholds: &HostHealthThresholds) -> HostHealthReport {
    let mut reasons: Vec<String> = Vec::new();

    // Computed percentages / ratios (None when either operand is unavailable).
    let disk_free_pct = ratio_pct(inputs.disk_free_bytes, inputs.disk_total_bytes);
    let inodes_free_pct = ratio_pct(inputs.inodes_free, inputs.inodes_total);
    let mem_available_pct = ratio_pct(inputs.mem_available_bytes, inputs.mem_total_bytes);
    let load_per_cpu = match (inputs.load_avg_1m, inputs.cpu_count) {
        (Some(load), Some(cpus)) if cpus > 0 =>
        {
            #[allow(clippy::cast_precision_loss)]
            Some(round2(load / cpus as f64))
        }
        _ => None,
    };

    // Resource-pressure verdict = worst-of the four floor/ceiling signals.
    let mut pressure = HostPressure::Ok;
    pressure = pressure.max(floor_signal(
        "disk free",
        disk_free_pct,
        thresholds.disk_free_warn_pct,
        thresholds.disk_free_crit_pct,
        &mut reasons,
    ));
    pressure = pressure.max(floor_signal(
        "inodes free",
        inodes_free_pct,
        thresholds.inodes_free_warn_pct,
        thresholds.inodes_free_crit_pct,
        &mut reasons,
    ));
    pressure = pressure.max(ceiling_signal(
        "load/cpu",
        load_per_cpu,
        thresholds.load_per_cpu_warn,
        thresholds.load_per_cpu_crit,
        &mut reasons,
    ));
    pressure = pressure.max(floor_signal(
        "memory available",
        mem_available_pct,
        thresholds.mem_available_warn_pct,
        thresholds.mem_available_crit_pct,
        &mut reasons,
    ));

    // `host_pressure_likely` reflects *only* resource-pressure evidence above.
    let host_pressure_likely = pressure != HostPressure::Ok;

    // ── Informational escalations (do NOT set host_pressure_likely) ──────
    // A non-writable data directory is a host/permission fault that explains
    // write failures; surface it and escalate overall status to critical, but
    // keep it out of the pressure verdict (it is not "pressure").
    let mut status = pressure;
    if inputs.db_dir_writable == Some(false) {
        let detail = inputs
            .db_dir_write_error
            .clone()
            .unwrap_or_else(|| "probe write failed".to_string());
        reasons.push(format!("data directory not writable: {detail}"));
        status = status.max(HostPressure::Critical);
    }

    // Stale WAL is a soft signal — report and bump status, but never the verdict.
    if let Some(age) = inputs.wal_age_secs
        && thresholds.wal_stale_warn_secs > 0
    {
        let level = if thresholds.wal_stale_crit_secs > 0 && age >= thresholds.wal_stale_crit_secs {
            HostPressure::Critical
        } else if age >= thresholds.wal_stale_warn_secs {
            HostPressure::Warning
        } else {
            HostPressure::Ok
        };
        if level != HostPressure::Ok {
            reasons.push(format!("WAL stale for {age}s (not checkpointed)"));
            status = status.max(level);
        }
    }

    if inputs.writer_pid_alive == Some(false)
        && let Some(pid) = inputs.writer_pid
    {
        reasons.push(format!(
            "recorded writer pid {pid} is not alive (stale lock hint)"
        ));
    }

    HostHealthReport {
        status: status.label().to_string(),
        host_pressure_likely,
        disk_free_bytes: inputs.disk_free_bytes,
        disk_total_bytes: inputs.disk_total_bytes,
        disk_free_pct,
        inodes_free: inputs.inodes_free,
        inodes_total: inputs.inodes_total,
        inodes_free_pct,
        load_avg_1m: inputs.load_avg_1m,
        cpu_count: inputs.cpu_count,
        load_per_cpu,
        mem_available_bytes: inputs.mem_available_bytes,
        mem_total_bytes: inputs.mem_total_bytes,
        mem_available_pct,
        db_dir_writable: inputs.db_dir_writable,
        db_file_bytes: inputs.db_file_bytes,
        wal_file_bytes: inputs.wal_file_bytes,
        shm_file_bytes: inputs.shm_file_bytes,
        wal_age_secs: inputs.wal_age_secs,
        writer_pid: inputs.writer_pid,
        writer_pid_alive: inputs.writer_pid_alive,
        probe_path: inputs.probe_path.clone(),
        reasons,
        errors: inputs.errors.clone(),
    }
}

/// Compute `numerator / denominator * 100` when both operands are present.
fn ratio_pct(numerator: Option<u64>, denominator: Option<u64>) -> Option<f64> {
    match (numerator, denominator) {
        (Some(n), Some(d)) => pct(n, d),
        _ => None,
    }
}

/// Evaluate a "floor" percentage signal (smaller is worse): classify and, on
/// breach, append an explanatory reason. A `None` value is a no-op (`Ok`).
fn floor_signal(
    name: &str,
    value: Option<f64>,
    warn: f64,
    crit: f64,
    reasons: &mut Vec<String>,
) -> HostPressure {
    let Some(p) = value else {
        return HostPressure::Ok;
    };
    let level = classify_floor(p, warn, crit);
    if level != HostPressure::Ok {
        reasons.push(format!(
            "{name} {p:.2}% < {} threshold",
            threshold_label(level, warn, crit)
        ));
    }
    level
}

/// Evaluate a "ceiling" signal (larger is worse): classify and, on breach,
/// append an explanatory reason. A `None` value is a no-op (`Ok`).
fn ceiling_signal(
    name: &str,
    value: Option<f64>,
    warn: f64,
    crit: f64,
    reasons: &mut Vec<String>,
) -> HostPressure {
    let Some(v) = value else {
        return HostPressure::Ok;
    };
    let level = classify_ceiling(v, warn, crit);
    if level != HostPressure::Ok {
        reasons.push(format!(
            "{name} {v:.2} > {} threshold",
            threshold_label_ceiling(level, warn, crit)
        ));
    }
    level
}

/// Classify a "floor" signal where *smaller is worse* (free %).
fn classify_floor(value: f64, warn_below: f64, crit_below: f64) -> HostPressure {
    if crit_below > 0.0 && value < crit_below {
        HostPressure::Critical
    } else if warn_below > 0.0 && value < warn_below {
        HostPressure::Warning
    } else {
        HostPressure::Ok
    }
}

/// Classify a "ceiling" signal where *larger is worse* (load).
fn classify_ceiling(value: f64, warn_above: f64, crit_above: f64) -> HostPressure {
    if crit_above > 0.0 && value > crit_above {
        HostPressure::Critical
    } else if warn_above > 0.0 && value > warn_above {
        HostPressure::Warning
    } else {
        HostPressure::Ok
    }
}

fn threshold_label(level: HostPressure, warn: f64, crit: f64) -> String {
    match level {
        HostPressure::Critical => format!("critical ({crit:.1}%)"),
        HostPressure::Warning => format!("warning ({warn:.1}%)"),
        HostPressure::Ok => "ok".to_string(),
    }
}

fn threshold_label_ceiling(level: HostPressure, warn: f64, crit: f64) -> String {
    match level {
        HostPressure::Critical => format!("critical ({crit:.1})"),
        HostPressure::Warning => format!("warning ({warn:.1})"),
        HostPressure::Ok => "ok".to_string(),
    }
}

/// Pick the most write-relevant existing directory to probe: the `SQLite` file's
/// parent when the DB is file-backed, otherwise the storage root. Falls back to
/// the nearest existing ancestor so `statvfs`/`fs2` calls do not fail on a
/// not-yet-created path.
fn primary_probe_path(config: &Config) -> PathBuf {
    let candidate = crate::disk::sqlite_file_path_from_database_url(&config.database_url)
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| config.storage_root.clone());
    nearest_existing_dir(&candidate)
}

fn nearest_existing_dir(path: &Path) -> PathBuf {
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

/// Read inode totals via `statvfs` (Unix only). Returns `(free, total)`.
#[cfg(unix)]
fn read_inodes(path: &Path) -> Result<(Option<u64>, Option<u64>), String> {
    let stat = nix::sys::statvfs::statvfs(path)
        .map_err(|e| format!("statvfs(inodes) failed path={} err={e}", path.display()))?;
    Ok((to_u64(stat.files_available()), to_u64(stat.files())))
}

#[cfg(not(unix))]
fn read_inodes(_path: &Path) -> Result<(Option<u64>, Option<u64>), String> {
    Err("inode probe not supported on this platform".to_string())
}

/// Read the 1-minute load average (Linux `/proc/loadavg`).
fn read_load_avg_1m() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let content = std::fs::read_to_string("/proc/loadavg").ok()?;
        content.split_whitespace().next()?.parse::<f64>().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Read host available + total memory (Linux `/proc/meminfo`). Returns
/// `(available_bytes, total_bytes)`.
fn read_host_memory() -> (Option<u64>, Option<u64>) {
    #[cfg(target_os = "linux")]
    {
        let Ok(content) = std::fs::read_to_string("/proc/meminfo") else {
            return (None, None);
        };
        let mut available = None;
        let mut total = None;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                available = parse_meminfo_kb(rest);
            } else if let Some(rest) = line.strip_prefix("MemTotal:") {
                total = parse_meminfo_kb(rest);
            }
        }
        (available, total)
    }
    #[cfg(not(target_os = "linux"))]
    {
        (None, None)
    }
}

/// Parse a `/proc/meminfo` value line like ` 16384000 kB` into bytes.
#[cfg(target_os = "linux")]
fn parse_meminfo_kb(rest: &str) -> Option<u64> {
    let kb: u64 = rest
        .trim()
        .strip_suffix("kB")
        .or_else(|| rest.trim().strip_suffix("KB"))
        .unwrap_or_else(|| rest.trim())
        .trim()
        .parse()
        .ok()?;
    Some(kb.saturating_mul(1024))
}

/// Probe whether `dir` accepts a write by creating, writing, and removing a
/// uniquely-named scratch file. Removing this self-created probe file is the
/// only deletion performed and never touches user data.
fn probe_dir_writable(dir: &Path) -> (bool, Option<String>) {
    use std::io::Write;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let name = format!(".am-hosthealth-probe-{}-{nanos}", std::process::id());
    let probe = dir.join(name);
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)
    {
        Ok(mut f) => {
            let write_ok = f.write_all(b"ok").is_ok();
            drop(f);
            // Best-effort cleanup of our own probe file.
            let _ = std::fs::remove_file(&probe);
            if write_ok {
                (true, None)
            } else {
                (
                    false,
                    Some("probe file opened but write failed".to_string()),
                )
            }
        }
        Err(e) => (
            false,
            Some(format!("create probe in {}: {e}", dir.display())),
        ),
    }
}

fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

fn file_age_secs(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|d| d.as_secs())
}

/// Collect all host signals available from `core` alone. The writer-PID fields
/// are left unset; callers with mailbox-ownership access populate them before
/// calling [`evaluate`].
#[must_use]
pub fn collect_inputs(config: &Config) -> HostHealthInputs {
    let mut inputs = HostHealthInputs::default();
    let probe = primary_probe_path(config);
    inputs.probe_path = Some(probe.display().to_string());

    // Disk free/total via fs2 (cross-platform).
    match fs2::available_space(&probe) {
        Ok(free) => inputs.disk_free_bytes = Some(free),
        Err(e) => inputs.errors.push(format!(
            "disk free probe failed path={} err={e}",
            probe.display()
        )),
    }
    match fs2::total_space(&probe) {
        Ok(total) => inputs.disk_total_bytes = Some(total),
        Err(e) => inputs.errors.push(format!(
            "disk total probe failed path={} err={e}",
            probe.display()
        )),
    }

    // Inodes via statvfs (Unix).
    match read_inodes(&probe) {
        Ok((free, total)) => {
            inputs.inodes_free = free;
            inputs.inodes_total = total;
        }
        Err(e) => inputs.errors.push(e),
    }

    // Load and CPU count.
    inputs.load_avg_1m = read_load_avg_1m();
    inputs.cpu_count = std::thread::available_parallelism()
        .ok()
        .and_then(|n| to_u64(n.get()));

    // Host memory.
    let (mem_avail, mem_total) = read_host_memory();
    inputs.mem_available_bytes = mem_avail;
    inputs.mem_total_bytes = mem_total;

    // Data-directory writability.
    let (writable, write_err) = probe_dir_writable(&probe);
    inputs.db_dir_writable = Some(writable);
    inputs.db_dir_write_error = write_err;

    // DB / WAL / SHM file sizes + WAL staleness.
    if let Some(db_path) = crate::disk::sqlite_file_path_from_database_url(&config.database_url) {
        inputs.db_file_bytes = file_len(&db_path);
        let wal = crate::disk::sqlite_sidecar_path(&db_path, "-wal");
        let shm = crate::disk::sqlite_sidecar_path(&db_path, "-shm");
        inputs.wal_file_bytes = file_len(&wal);
        inputs.shm_file_bytes = file_len(&shm);
        inputs.wal_age_secs = file_age_secs(&wal);
    }

    inputs
}

/// Convenience: collect inputs from the live host and evaluate against the
/// env-configured thresholds. Writer-PID fields are not populated.
#[must_use]
pub fn sample_host_health(config: &Config) -> HostHealthReport {
    let inputs = collect_inputs(config);
    evaluate(&inputs, &HostHealthThresholds::from_env())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_inputs() -> HostHealthInputs {
        HostHealthInputs {
            disk_free_bytes: Some(500 * 1024 * 1024 * 1024),
            disk_total_bytes: Some(1000 * 1024 * 1024 * 1024),
            inodes_free: Some(9_000_000),
            inodes_total: Some(10_000_000),
            load_avg_1m: Some(2.0),
            cpu_count: Some(8),
            mem_available_bytes: Some(8 * 1024 * 1024 * 1024),
            mem_total_bytes: Some(16 * 1024 * 1024 * 1024),
            db_dir_writable: Some(true),
            db_dir_write_error: None,
            db_file_bytes: Some(1024),
            wal_file_bytes: Some(0),
            shm_file_bytes: Some(0),
            wal_age_secs: Some(5),
            writer_pid: None,
            writer_pid_alive: None,
            probe_path: Some("/data".to_string()),
            errors: Vec::new(),
        }
    }

    #[test]
    fn healthy_host_has_no_pressure() {
        let report = evaluate(&healthy_inputs(), &HostHealthThresholds::default());
        assert!(
            !report.host_pressure_likely,
            "reasons: {:?}",
            report.reasons
        );
        assert_eq!(report.status, "ok");
        assert!(report.reasons.is_empty());
        // Percentages computed.
        assert_eq!(report.disk_free_pct, Some(50.0));
        assert_eq!(report.inodes_free_pct, Some(90.0));
        assert_eq!(report.load_per_cpu, Some(0.25));
        assert_eq!(report.mem_available_pct, Some(50.0));
    }

    #[test]
    fn simulated_low_disk_fires_pressure() {
        let mut inputs = healthy_inputs();
        // 2% free of total -> below 3% critical.
        inputs.disk_free_bytes = Some(20 * 1024 * 1024 * 1024);
        inputs.disk_total_bytes = Some(1000 * 1024 * 1024 * 1024);
        let report = evaluate(&inputs, &HostHealthThresholds::default());
        assert!(report.host_pressure_likely);
        assert_eq!(report.status, "critical");
        assert!(report.reasons.iter().any(|r| r.contains("disk free")));
    }

    #[test]
    fn simulated_high_load_fires_pressure() {
        let mut inputs = healthy_inputs();
        // 8 cores, load 40 -> load/cpu 5.0 -> above 4.0 warn, below 8.0 crit.
        inputs.load_avg_1m = Some(40.0);
        inputs.cpu_count = Some(8);
        let report = evaluate(&inputs, &HostHealthThresholds::default());
        assert!(report.host_pressure_likely);
        assert_eq!(report.status, "warning");
        assert_eq!(report.load_per_cpu, Some(5.0));
        assert!(report.reasons.iter().any(|r| r.contains("load/cpu")));
    }

    #[test]
    fn simulated_inode_exhaustion_fires_pressure() {
        let mut inputs = healthy_inputs();
        // 1% inodes free -> below 3% critical.
        inputs.inodes_free = Some(100_000);
        inputs.inodes_total = Some(10_000_000);
        let report = evaluate(&inputs, &HostHealthThresholds::default());
        assert!(report.host_pressure_likely);
        assert_eq!(report.status, "critical");
        assert!(report.reasons.iter().any(|r| r.contains("inodes free")));
    }

    #[test]
    fn simulated_memory_pressure_fires() {
        let mut inputs = healthy_inputs();
        // ~5% memory available -> below 10% warn, above 3% crit.
        inputs.mem_available_bytes = Some(800 * 1024 * 1024);
        inputs.mem_total_bytes = Some(16 * 1024 * 1024 * 1024);
        let report = evaluate(&inputs, &HostHealthThresholds::default());
        assert!(report.host_pressure_likely);
        assert_eq!(report.status, "warning");
        assert!(
            report
                .reasons
                .iter()
                .any(|r| r.contains("memory available"))
        );
    }

    #[test]
    fn missing_data_never_fires_pressure() {
        // No resource signals at all -> verdict must stay false.
        let inputs = HostHealthInputs::default();
        let report = evaluate(&inputs, &HostHealthThresholds::default());
        assert!(!report.host_pressure_likely);
        assert_eq!(report.status, "ok");
        assert!(report.disk_free_pct.is_none());
        assert!(report.load_per_cpu.is_none());
    }

    #[test]
    fn unwritable_dir_escalates_status_but_not_pressure_verdict() {
        let mut inputs = healthy_inputs();
        inputs.db_dir_writable = Some(false);
        inputs.db_dir_write_error = Some("EACCES".to_string());
        let report = evaluate(&inputs, &HostHealthThresholds::default());
        // Writability is not "pressure", so the pressure verdict stays false...
        assert!(!report.host_pressure_likely);
        // ...but overall status escalates and the reason is surfaced.
        assert_eq!(report.status, "critical");
        assert!(report.reasons.iter().any(|r| r.contains("not writable")));
    }

    #[test]
    fn dead_writer_pid_surfaced_without_pressure() {
        let mut inputs = healthy_inputs();
        inputs.writer_pid = Some(4242);
        inputs.writer_pid_alive = Some(false);
        let report = evaluate(&inputs, &HostHealthThresholds::default());
        assert!(!report.host_pressure_likely);
        assert!(report.reasons.iter().any(|r| r.contains("4242")));
    }

    #[test]
    fn stale_wal_escalates_status_only() {
        let mut inputs = healthy_inputs();
        inputs.wal_age_secs = Some(2000); // > 1800 crit
        let report = evaluate(&inputs, &HostHealthThresholds::default());
        assert!(!report.host_pressure_likely);
        assert_eq!(report.status, "critical");
        assert!(report.reasons.iter().any(|r| r.contains("WAL stale")));
    }

    #[test]
    fn pct_guards_zero_denominator() {
        assert_eq!(pct(5, 0), None);
        assert_eq!(pct(0, 100), Some(0.0));
        assert_eq!(pct(50, 200), Some(25.0));
    }

    #[test]
    fn classify_floor_and_ceiling_levels() {
        assert_eq!(classify_floor(50.0, 10.0, 3.0), HostPressure::Ok);
        assert_eq!(classify_floor(5.0, 10.0, 3.0), HostPressure::Warning);
        assert_eq!(classify_floor(1.0, 10.0, 3.0), HostPressure::Critical);
        assert_eq!(classify_ceiling(0.5, 4.0, 8.0), HostPressure::Ok);
        assert_eq!(classify_ceiling(5.0, 4.0, 8.0), HostPressure::Warning);
        assert_eq!(classify_ceiling(9.0, 4.0, 8.0), HostPressure::Critical);
    }

    #[test]
    fn host_pressure_max_orders_severity() {
        assert_eq!(
            HostPressure::Ok.max(HostPressure::Warning),
            HostPressure::Warning
        );
        assert_eq!(
            HostPressure::Warning.max(HostPressure::Critical),
            HostPressure::Critical
        );
        assert_eq!(
            HostPressure::Critical.max(HostPressure::Ok),
            HostPressure::Critical
        );
    }

    #[test]
    fn default_thresholds_are_sane() {
        // `core` forbids unsafe, so we cannot mutate the environment here; the
        // `from_env` parsing helpers are exercised below without mutation. Verify
        // the defaults are positive and ordered (crit floor < warn floor; warn
        // ceiling < crit ceiling).
        let t = HostHealthThresholds::default();
        assert!(t.disk_free_crit_pct > 0.0 && t.disk_free_crit_pct < t.disk_free_warn_pct);
        assert!(t.inodes_free_crit_pct < t.inodes_free_warn_pct);
        assert!(t.load_per_cpu_warn < t.load_per_cpu_crit);
        assert!(t.mem_available_crit_pct < t.mem_available_warn_pct);
        assert!(t.wal_stale_warn_secs < t.wal_stale_crit_secs);
    }

    #[test]
    fn env_parse_helpers_reject_garbage_and_missing() {
        // A name that is almost certainly unset returns None rather than panicking.
        assert_eq!(env_f64("AM_HOST_DEFINITELY_UNSET_XYZ"), None);
        assert_eq!(env_u64("AM_HOST_DEFINITELY_UNSET_XYZ"), None);
        // `from_env` with no relevant overrides equals the defaults (the test
        // process does not set the AM_HOST_* knobs).
        assert_eq!(
            HostHealthThresholds::from_env(),
            HostHealthThresholds::default()
        );
    }

    #[test]
    fn collect_inputs_runs_against_real_host() {
        let config = Config::default();
        let inputs = collect_inputs(&config);
        // CPU count is always available; writability probe on cwd should pass.
        assert!(inputs.cpu_count.is_some());
        assert!(inputs.probe_path.is_some());
        // On Linux, load + memory should be populated.
        if cfg!(target_os = "linux") {
            assert!(inputs.load_avg_1m.is_some());
            assert!(inputs.mem_total_bytes.is_some());
        }
        // The report serializes cleanly.
        let report = evaluate(&inputs, &HostHealthThresholds::default());
        let json = serde_json::to_string(&report).expect("serialize host report");
        assert!(json.contains("host_pressure_likely"));
    }

    #[test]
    fn report_serializes_optional_fields_skip_when_none() {
        let inputs = HostHealthInputs::default();
        let report = evaluate(&inputs, &HostHealthThresholds::default());
        let json = serde_json::to_string(&report).expect("serialize");
        // None-valued optional fields are skipped.
        assert!(!json.contains("disk_free_bytes"));
        // Always-present fields remain.
        assert!(json.contains("host_pressure_likely"));
        assert!(json.contains("\"status\""));
    }
}
