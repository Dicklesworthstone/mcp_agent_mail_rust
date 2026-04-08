//! Startup verification probes for `AgentMailTUI`.
//!
//! Each probe checks one aspect of the runtime environment and returns
//! a [`ProbeResult`] with a human-friendly error message and remediation
//! hints when something is wrong.

use crate::{
    MailboxActivityLockMode, acquire_mailbox_activity_lock_for_database_url,
    resolve_server_database_url_sqlite_path,
};
use mcp_agent_mail_core::{Config, disk::is_sqlite_memory_database_url};
use mcp_agent_mail_db::DbPoolConfig;
use std::collections::BTreeSet;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

// ──────────────────────────────────────────────────────────────────────
// Database lock detection (br-db-lock)
// ──────────────────────────────────────────────────────────────────────

/// Result of checking whether the database is available or locked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbLockStatus {
    /// Database is available and not exclusively locked.
    Available,
    /// Database is currently locked by another process (retryable).
    Locked,
    /// Database file is missing (not yet initialized).
    Missing,
    /// Could not determine database status due to an error.
    Error(String),
}

/// Check if the database file is exclusively locked by another process.
#[must_use]
pub fn check_db_lock_status(config: &Config) -> DbLockStatus {
    if is_sqlite_memory_database_url(&config.database_url) {
        return DbLockStatus::Available;
    }

    let Some(sqlite_path) = resolve_server_database_url_sqlite_path(&config.database_url) else {
        return DbLockStatus::Error("Failed to resolve sqlite path from DATABASE_URL".to_string());
    };

    if !sqlite_path.exists() {
        return DbLockStatus::Missing;
    }

    // Try to open the file with an exclusive flock to see if another am is holding it
    // during a sensitive operation (like backfill or migration).
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&sqlite_path)
    {
        Ok(file) => {
            use fs2::FileExt;
            // We use try_lock_exclusive() to see if we CAN take it.
            // If it's already held by another process using flock, this will fail.
            if file.try_lock_exclusive().is_ok() {
                let _ = file.unlock();
                DbLockStatus::Available
            } else {
                DbLockStatus::Locked
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("Resource temporarily unavailable")
                || msg.contains("database is locked")
            {
                DbLockStatus::Locked
            } else {
                DbLockStatus::Error(msg)
            }
        }
    }
}

/// Information about a process holding a database lock.
#[derive(Debug)]
struct LockHolder {
    pid: u32,
    cmdline: String,
    is_python: bool,
}

/// Attempt to identify the process holding an flock on the given file.
///
/// On Linux, reads `/proc/locks` to find FLOCK entries whose inode matches the
/// database file, then reads `/proc/<pid>/cmdline` for the command line. Returns
/// `None` on non-Linux platforms or if identification fails for any reason.
fn identify_lock_holder(db_path: &std::path::Path) -> Option<LockHolder> {
    identify_lock_holder_via_proc(db_path)
}

#[cfg(target_os = "linux")]
fn identify_lock_holder_via_proc(db_path: &std::path::Path) -> Option<LockHolder> {
    use std::os::unix::fs::MetadataExt;

    // Get the inode and device of the database file.
    let meta = std::fs::metadata(db_path).ok()?;
    let target_ino = meta.ino();
    let target_dev = meta.dev();
    // Extract major/minor from dev_t (Linux encoding).
    let target_major = ((target_dev >> 8) & 0xfff) as u32;
    let target_minor = ((target_dev & 0xff) | ((target_dev >> 12) & 0xfff00)) as u32;

    // Parse /proc/locks line by line looking for FLOCK entries that match.
    // Format: "1: FLOCK  ADVISORY  WRITE 12345 08:01:654321 0 EOF"
    // The device major:minor fields are in hexadecimal, inode is decimal.
    let locks_content = std::fs::read_to_string("/proc/locks").ok()?;
    for line in locks_content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        // fields[1] = lock type (FLOCK/POSIX), fields[4] = PID, fields[5] = maj:min:ino
        if fields[1] != "FLOCK" {
            continue;
        }
        let dev_ino = fields[5];
        let parts: Vec<&str> = dev_ino.split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let Ok(major) = u32::from_str_radix(parts[0], 16) else {
            continue;
        };
        let Ok(minor) = u32::from_str_radix(parts[1], 16) else {
            continue;
        };
        let Ok(ino) = parts[2].parse::<u64>() else {
            continue;
        };
        if ino != target_ino || major != target_major || minor != target_minor {
            continue;
        }
        // Found a matching flock — extract the PID.
        let Ok(pid) = fields[4].parse::<u32>() else {
            continue;
        };
        // Read the command line from /proc/<pid>/cmdline.
        let cmdline = pid_command_line(pid).unwrap_or_else(|| format!("<PID {pid}>"));
        let is_python = cmdline
            .split_whitespace()
            .next()
            .map(|argv0| {
                let basename = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0);
                basename.starts_with("python")
            })
            .unwrap_or(false);
        tracing::debug!(pid, %cmdline, is_python, "identified lock holder for db file");
        return Some(LockHolder {
            pid,
            cmdline,
            is_python,
        });
    }
    None
}

/// macOS/BSD fallback: use `lsof` to find the process holding the DB file.
#[cfg(not(target_os = "linux"))]
fn identify_lock_holder_via_proc(db_path: &std::path::Path) -> Option<LockHolder> {
    let pids = pids_holding_file(db_path);
    let pid = *pids.first()?;
    let cmdline = pid_command_line(pid).unwrap_or_else(|| format!("<PID {pid}>"));
    let is_python = cmdline
        .split_whitespace()
        .next()
        .map(|argv0| {
            let basename = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0);
            basename.starts_with("python")
        })
        .unwrap_or(false);
    Some(LockHolder {
        pid,
        cmdline,
        is_python,
    })
}

// ──────────────────────────────────────────────────────────────────────
// Database file holder detection via /proc/*/fd (br-db-lock)
// ──────────────────────────────────────────────────────────────────────

/// Find all PIDs that have the given file open (via `/proc/*/fd/` symlink targets).
///
/// This is more comprehensive than the flock-based `identify_lock_holder` because
/// it catches SQLite WAL-mode readers/writers that hold the file open without an
/// explicit flock.  Automatically excludes the current process.
#[cfg(target_os = "linux")]
#[must_use]
pub fn pids_holding_file(path: &std::path::Path) -> Vec<u32> {
    use std::os::unix::fs::MetadataExt;

    let Ok(target_meta) = std::fs::metadata(path) else {
        return Vec::new();
    };
    let target_ino = target_meta.ino();
    let target_dev = target_meta.dev();
    let my_pid = std::process::id();

    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    let mut holders = Vec::new();
    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid == my_pid {
            continue;
        }
        let fd_dir = format!("/proc/{pid}/fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd_entry in fds.flatten() {
            // readlink on /proc/<pid>/fd/<N> gives the target path.
            let Ok(link_target) = std::fs::read_link(fd_entry.path()) else {
                continue;
            };
            // Compare by inode+device to handle symlinks and bind mounts.
            if let Ok(link_meta) = std::fs::metadata(&link_target) {
                if link_meta.ino() == target_ino && link_meta.dev() == target_dev {
                    holders.push(pid);
                    break; // One match per PID is enough
                }
            }
        }
    }

    holders
}

/// Find all PIDs that have the given file open (via `lsof`).
///
/// macOS has no `/proc` filesystem, so we shell out to `lsof` and parse the
/// output.  Automatically excludes the current process.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn pids_holding_file(path: &std::path::Path) -> Vec<u32> {
    let my_pid = std::process::id();
    let Ok(output) = std::process::Command::new("lsof")
        .args(["-t", "-w"])
        .arg(path)
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .filter(|&pid| pid != my_pid)
        .collect()
}

/// Find Agent Mail PIDs that have the given database file open.
///
/// Filters `pids_holding_file` results to only include processes whose
/// executable or command line matches the Agent Mail signature.
#[must_use]
pub fn agent_mail_pids_holding_file(path: &std::path::Path) -> Vec<u32> {
    pids_holding_file(path)
        .into_iter()
        .filter(|pid| pid_is_agent_mail(*pid))
        .collect()
}

// ──────────────────────────────────────────────────────────────────────
// Port detection types (br-7ri2)
// ──────────────────────────────────────────────────────────────────────

/// Result of checking whether a port is available or already in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortStatus {
    /// Port is free and available for binding.
    Free,
    /// Port is in use by an Agent Mail server (can be reused).
    AgentMailServer,
    /// Port is in use by another process (cannot be reused).
    OtherProcess {
        /// Description of what we know about the other process.
        description: String,
    },
    /// Could not determine port status due to an error.
    Error {
        /// The error kind.
        kind: std::io::ErrorKind,
        /// Human-readable error description.
        message: String,
    },
}

impl PortStatus {
    /// Returns true if the port can be used (either free or Agent Mail server reuse).
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        matches!(self, Self::Free | Self::AgentMailServer)
    }

    /// Returns true if an Agent Mail server is already running.
    #[must_use]
    pub const fn is_agent_mail_server(&self) -> bool {
        matches!(self, Self::AgentMailServer)
    }
}

/// Default timeout for health check connections.
///
/// Keep this short to avoid multi-second startup stalls when probing a port
/// occupied by an unrelated process that accepts TCP but does not speak HTTP.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_HEALTH_BODY_BYTES: usize = 4096;
const LISTENER_PID_HINT_DIR: &str = "mcp-agent-mail-port-pids";
pub(crate) const HEALTH_SIGNATURE_HEADER_NAME: &str = "x-agent-mail-health";
pub(crate) const HEALTH_SIGNATURE_HEADER_VALUE: &str = "1";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListenerPidHint {
    pid: u32,
    exe_path: Option<String>,
    /// Unix timestamp (seconds) when the hint was written.
    /// Used to reject stale hints from recycled PIDs.
    created_epoch_secs: Option<u64>,
}

/// Maximum age of a PID hint file before it's considered stale (seconds).
/// Stale hints are ignored to prevent PID recycling attacks.
/// Override via `AM_PID_HINT_MAX_AGE_SECS` env var.
fn pid_hint_max_age_secs() -> u64 {
    std::env::var("AM_PID_HINT_MAX_AGE_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(86400) // 24 hours default
        .max(60) // safety floor: never less than 1 minute
}

/// Check the status of a port: free, occupied by Agent Mail, or occupied by another process.
///
/// This is a cross-platform replacement for lsof-based detection. It uses:
/// 1. `TcpListener::bind()` to check if the port is available
/// 2. HTTP health check to identify if an existing listener is Agent Mail
///
/// # Arguments
/// * `host` - The host address to check (e.g., "127.0.0.1")
/// * `port` - The port number to check
///
/// # Returns
/// A `PortStatus` indicating whether the port is free, has an Agent Mail server, or is in use by
/// another process.
#[must_use]
pub fn check_port_status(host: &str, port: u16) -> PortStatus {
    let addr = format!("{host}:{port}");
    let bind_host = normalize_bind_host_for_socket(host);

    // Step 1: Try to bind to the port
    match TcpListener::bind((bind_host.as_ref(), port)) {
        Ok(_listener) => {
            // Port is free (listener is dropped immediately, releasing the port)
            return PortStatus::Free;
        }
        Err(e) => {
            match e.kind() {
                std::io::ErrorKind::AddrInUse => {
                    // Port is in use - check if it's an Agent Mail server
                }
                kind => {
                    // Other error (permission denied, address not available, etc.)
                    return PortStatus::Error {
                        kind,
                        message: e.to_string(),
                    };
                }
            }
        }
    }

    // Step 2: Port is in use - try to identify if it's Agent Mail via health check
    if is_agent_mail_health_check(host, port) {
        return PortStatus::AgentMailServer;
    }

    // Step 3: Health check failed (timeout, server busy, etc.) — fall back to
    // process-level identification via listener PID lookup + /proc/{pid}/cmdline.
    if is_agent_mail_by_pid(host, port) {
        return PortStatus::AgentMailServer;
    }

    PortStatus::OtherProcess {
        description: format!("Unknown process listening on {addr}"),
    }
}

fn normalize_bind_host_for_socket(host: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = host.trim();
    trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .map_or_else(
            || std::borrow::Cow::Borrowed(trimmed),
            std::borrow::Cow::Borrowed,
        )
}

fn normalize_connect_host_for_health_check(host: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return std::borrow::Cow::Borrowed("127.0.0.1");
    }

    let unbracketed = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(trimmed);

    match unbracketed {
        "0.0.0.0" => std::borrow::Cow::Borrowed("127.0.0.1"),
        "::" => std::borrow::Cow::Borrowed("[::1]"),
        _ => {
            if unbracketed.contains(':') && !trimmed.starts_with('[') {
                std::borrow::Cow::Owned(format!("[{unbracketed}]"))
            } else {
                std::borrow::Cow::Borrowed(trimmed)
            }
        }
    }
}

/// Attempt to connect to a port and verify it's an Agent Mail server via health check.
///
/// Sends a minimal HTTP GET request to `/health` and checks for a valid response.
fn is_agent_mail_health_check(host: &str, port: u16) -> bool {
    let connect_host = normalize_connect_host_for_health_check(host);
    let host_for_resolution = connect_host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or_else(|| connect_host.as_ref());
    let Ok(addrs) = (host_for_resolution, port).to_socket_addrs() else {
        return false;
    };
    is_agent_mail_health_check_addrs(connect_host.as_ref(), port, addrs)
}

fn is_agent_mail_health_check_addrs(
    connect_host: &str,
    port: u16,
    addrs: impl IntoIterator<Item = std::net::SocketAddr>,
) -> bool {
    for addr in addrs {
        if probe_agent_mail_health_addr(connect_host, port, addr) {
            return true;
        }
    }
    false
}

fn probe_agent_mail_health_addr(connect_host: &str, port: u16, addr: std::net::SocketAddr) -> bool {
    // Try to connect with a short timeout
    let Ok(stream) = TcpStream::connect_timeout(&addr, HEALTH_CHECK_TIMEOUT) else {
        return false;
    };

    // Set read/write timeouts
    let _ = stream.set_read_timeout(Some(HEALTH_CHECK_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HEALTH_CHECK_TIMEOUT));

    // Send HTTP GET /health request
    let request = format!(
        "GET /health HTTP/1.1\r\n\
         Host: {connect_host}:{port}\r\n\
         Connection: close\r\n\
         User-Agent: mcp-agent-mail-startup-check\r\n\
         \r\n"
    );

    let mut stream = stream;
    let result = (|| -> bool {
        if stream.write_all(request.as_bytes()).is_err() {
            return false;
        }

        // Read response, bounded by MAX_HEALTH_BODY_BYTES to prevent memory exhaustion DoS
        // from malicious or misbehaving services listening on this port.
        let mut reader = BufReader::new((&stream).take((MAX_HEALTH_BODY_BYTES + 4096) as u64));
        let mut status_line = String::new();
        if reader.read_line(&mut status_line).is_err() {
            return false;
        }

        // Check for valid HTTP response (2xx or 3xx status codes are acceptable)
        // Agent Mail returns 200 OK for /health
        if !status_line.starts_with("HTTP/1.") {
            return false;
        }

        // Parse status code
        let parts: Vec<&str> = status_line.split_whitespace().collect();
        if parts.len() < 2 {
            return false;
        }

        let status_code: u16 = match parts[1].parse() {
            Ok(code) => code,
            Err(_) => return false,
        };

        if !(200..=399).contains(&status_code) {
            return false;
        }

        let mut headers = String::new();
        let mut header_bytes = 0_usize;
        loop {
            let mut line = String::new();
            let Ok(bytes) = reader.read_line(&mut line) else {
                return false;
            };
            if bytes == 0 {
                return false;
            }
            if line == "\r\n" {
                break;
            }
            header_bytes = header_bytes.saturating_add(bytes);
            if header_bytes > MAX_HEALTH_BODY_BYTES {
                return false;
            }
            headers.push_str(&line);
        }

        has_agent_mail_signature(&headers)
    })();

    // Ensure we explicitly close the connection per UBS warning.
    let _ = stream.shutdown(std::net::Shutdown::Both);

    result
}

#[allow(dead_code)]
fn parse_content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    })
}

fn has_agent_mail_signature(headers: &str) -> bool {
    headers.lines().any(|line| {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        let name = name.trim();
        let value = value.trim();
        name.eq_ignore_ascii_case(HEALTH_SIGNATURE_HEADER_NAME)
            && value.eq_ignore_ascii_case(HEALTH_SIGNATURE_HEADER_VALUE)
    })
}

/// Fallback: identify the process holding `port` by PID.
///
/// Uses bounded listener PID discovery (`ss` on Linux, `lsof` elsewhere), then
/// reads `/proc/{pid}/cmdline` or `/proc/{pid}/exe` to check if it's an Agent
/// Mail binary. This catches cases where the HTTP health check times out or the
/// server is temporarily unresponsive but IS an `am` process.
fn is_agent_mail_by_pid(host: &str, port: u16) -> bool {
    !agent_mail_port_holder_pids_with_hint(host, port).is_empty()
}

#[must_use]
pub fn write_listener_pid_hint(host: &str, port: u16) -> PathBuf {
    let path = listener_pid_hint_path(host, port);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
        // Restrict directory permissions to owner-only (0o700) so other
        // users cannot plant symlinks inside it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let hint = ListenerPidHint {
        pid: std::process::id(),
        exe_path: current_executable_hint_path(),
        created_epoch_secs: Some(now_secs),
    };
    let content = format_listener_pid_hint(&hint);
    // Write via temp file + rename to avoid symlink TOCTOU attacks:
    // rename() is atomic on POSIX, so there's no window for an attacker
    // to substitute a symlink between creation and write.
    let tmp_path = path.with_extension("pid.tmp");
    // Remove stale symlinks or files at the target path first.
    let _ = std::fs::remove_file(&path);
    if std::fs::write(&tmp_path, &content).is_ok() {
        if std::fs::rename(&tmp_path, &path).is_err() {
            let _ = std::fs::write(&path, &content);
        }
    } else {
        let _ = std::fs::write(&path, &content);
    }
    path
}

/// Return Agent Mail PIDs currently listening on `port`.
#[must_use]
pub fn agent_mail_port_holder_pids(port: u16) -> Vec<u32> {
    port_holder_pids(port)
        .into_iter()
        .filter(|pid| pid_is_agent_mail(*pid))
        .collect()
}

/// Return Agent Mail PIDs currently listening on `host:port`, preferring a
/// previously recorded PID hint before falling back to system-wide listener
/// discovery.
#[must_use]
pub fn agent_mail_port_holder_pids_with_hint(host: &str, port: u16) -> Vec<u32> {
    if let Some(pid) = hinted_agent_mail_pid(host, port) {
        return vec![pid];
    }
    listener_port_holder_pids(host, port)
        .into_iter()
        .filter(|pid| pid_is_agent_mail(*pid))
        .collect()
}

/// Return listener PIDs currently holding `host:port`, preferring a recorded
/// hint before falling back to system-wide listener discovery.
#[must_use]
pub fn listener_port_holder_pids_with_hint(host: &str, port: u16) -> Vec<u32> {
    if let Some(pid) = hinted_agent_mail_pid(host, port) {
        return vec![pid];
    }
    listener_port_holder_pids(host, port)
}

#[cfg(target_os = "linux")]
#[must_use]
pub fn agent_mail_pids_all_stopped(pids: &[u32]) -> bool {
    !pids.is_empty() && pids.iter().all(|pid| pid_is_stopped(*pid))
}

/// Check if all the given PIDs have exited (macOS/BSD version).
///
/// Uses `ps -p <pid>` to probe whether each process is still alive.
/// Returns `true` when every PID is gone.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn agent_mail_pids_all_stopped(pids: &[u32]) -> bool {
    !pids.is_empty()
        && pids.iter().all(|&pid| {
            // `ps -p <pid>` exits 0 if alive, 1 if gone.
            !std::process::Command::new("ps")
                .args(["-p", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
}

fn listener_pid_hint_path(host: &str, port: u16) -> PathBuf {
    std::env::temp_dir()
        .join(LISTENER_PID_HINT_DIR)
        .join(format!("{}-{port}.pid", encode_pid_hint_component(host)))
}

fn current_executable_hint_path() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let canonical = std::fs::canonicalize(&exe).unwrap_or(exe);
    Some(canonical.display().to_string())
}

fn format_listener_pid_hint(hint: &ListenerPidHint) -> String {
    let ts = hint.created_epoch_secs.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    });
    match hint.exe_path.as_deref() {
        Some(exe_path) if !exe_path.trim().is_empty() => {
            format!("{}\n{exe_path}\n{ts}\n", hint.pid)
        }
        _ => format!("{}\n\n{ts}\n", hint.pid),
    }
}

fn encode_pid_hint_component(value: &str) -> String {
    use std::fmt::Write as _;

    let trimmed = value.trim();
    let raw = if trimmed.is_empty() {
        b"host".as_slice()
    } else {
        trimmed.as_bytes()
    };
    let mut encoded = String::with_capacity(raw.len().saturating_mul(2));
    for byte in raw {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn parse_listener_pid_hint(content: &str) -> Option<ListenerPidHint> {
    let mut lines = content.lines();
    let pid = lines.next()?.trim().parse::<u32>().ok()?;
    // Line 2: exe_path (may be empty)
    let exe_line = lines.next().unwrap_or("");
    let exe_path = if exe_line.trim().is_empty() {
        None
    } else {
        Some(exe_line.trim().to_string())
    };
    // Line 3: creation timestamp (epoch seconds) — optional for
    // backwards compatibility with older hint files.
    let created_epoch_secs = lines
        .next()
        .and_then(|line| line.trim().parse::<u64>().ok());
    Some(ListenerPidHint {
        pid,
        exe_path,
        created_epoch_secs,
    })
}

fn read_listener_pid_hint(host: &str, port: u16) -> Option<ListenerPidHint> {
    let content = std::fs::read_to_string(listener_pid_hint_path(host, port)).ok()?;
    let hint = parse_listener_pid_hint(&content)?;
    // Reject stale hints to prevent PID recycling attacks.
    // If no timestamp is present (old format), accept the hint but log a warning.
    if let Some(created) = hint.created_epoch_secs {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let max_age = pid_hint_max_age_secs();
        if now.saturating_sub(created) > max_age {
            tracing::debug!(
                pid = hint.pid,
                age_secs = now.saturating_sub(created),
                max_age_secs = max_age,
                "rejecting stale PID hint file"
            );
            return None;
        }
    }
    Some(hint)
}

fn hinted_pid_matches_listener(hint: &ListenerPidHint, listeners: &[u32]) -> bool {
    if !listeners.contains(&hint.pid) {
        return false;
    }

    hint.exe_path.as_deref().map_or_else(
        || pid_is_agent_mail(hint.pid),
        |expected_path| {
            pid_executable_path_matches(hint.pid, expected_path) || pid_is_agent_mail(hint.pid)
        },
    )
}

#[cfg(target_os = "linux")]
fn hinted_agent_mail_pid(host: &str, port: u16) -> Option<u32> {
    let hint = read_listener_pid_hint(host, port)?;
    let listeners = listener_port_holder_pids(host, port);
    hinted_pid_matches_listener(&hint, &listeners).then_some(hint.pid)
}

#[cfg(not(target_os = "linux"))]
fn hinted_agent_mail_pid(host: &str, port: u16) -> Option<u32> {
    let hint = read_listener_pid_hint(host, port)?;
    let listeners = listener_port_holder_pids(host, port);
    hinted_pid_matches_listener(&hint, &listeners).then_some(hint.pid)
}

#[cfg(target_os = "linux")]
fn pid_is_stopped(pid: u32) -> bool {
    matches!(pid_process_state(pid), Some('T' | 't'))
}

#[cfg(target_os = "linux")]
fn pid_process_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_proc_stat_state(&stat)
}

#[cfg(target_os = "linux")]
fn parse_proc_stat_state(stat: &str) -> Option<char> {
    let close_paren = stat.rfind(')')?;
    stat.get(close_paren + 2..)?.chars().next()
}

fn port_holder_pids(port: u16) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        let pids = port_holder_pids_via_ss(port);
        if !pids.is_empty() {
            return pids;
        }
    }

    port_holder_pids_via_lsof(port)
}

fn listener_port_holder_pids(host: &str, port: u16) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        let pids = port_holder_pids_via_ss_for_host(host, port);
        if !pids.is_empty() {
            return pids;
        }
    }

    port_holder_pids_via_lsof_for_host(host, port)
}

#[cfg(target_os = "linux")]
fn port_holder_pids_via_ss(port: u16) -> Vec<u32> {
    let output = match std::process::Command::new("ss")
        .args(["-H", "-ltnp", &format!("sport = :{port}")])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    parse_ss_port_holder_pids(String::from_utf8_lossy(&output.stdout).as_ref())
}

#[cfg(target_os = "linux")]
fn port_holder_pids_via_ss_for_host(host: &str, port: u16) -> Vec<u32> {
    let output = match std::process::Command::new("ss")
        .args(["-H", "-ltnp", &format!("sport = :{port}")])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    parse_ss_port_holder_pids_for_host(String::from_utf8_lossy(&output.stdout).as_ref(), host)
}

fn port_holder_pids_via_lsof(port: u16) -> Vec<u32> {
    let output = match std::process::Command::new("lsof")
        .args(["-ti", &format!("tcp:{port}")])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() || !output.stdout.is_empty() => output,
        _ => return Vec::new(),
    };

    parse_lsof_port_holder_pids(String::from_utf8_lossy(&output.stdout).as_ref())
}

fn port_holder_pids_via_lsof_for_host(host: &str, port: u16) -> Vec<u32> {
    let output = match std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpn"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() || !output.stdout.is_empty() => output,
        _ => return Vec::new(),
    };

    parse_lsof_port_holder_pids_for_host(String::from_utf8_lossy(&output.stdout).as_ref(), host)
}

fn parse_ss_port_holder_pids(output: &str) -> Vec<u32> {
    let mut pids = BTreeSet::new();
    for segment in output.split("pid=").skip(1) {
        let digits: String = segment.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(pid) = digits.parse::<u32>() {
            pids.insert(pid);
        }
    }
    pids.into_iter().collect()
}

#[cfg(target_os = "linux")]
fn parse_ss_port_holder_pids_for_host(output: &str, host: &str) -> Vec<u32> {
    let mut pids = BTreeSet::new();
    for line in output.lines() {
        let Some(local_addr) = line.split_whitespace().nth(3) else {
            continue;
        };
        let Some(listener_host) = extract_socket_host(local_addr) else {
            continue;
        };
        if !listener_host_matches_request(listener_host, host) {
            continue;
        }
        for segment in line.split("pid=").skip(1) {
            let digits: String = segment.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(pid) = digits.parse::<u32>() {
                pids.insert(pid);
            }
        }
    }
    pids.into_iter().collect()
}

fn parse_lsof_port_holder_pids(output: &str) -> Vec<u32> {
    let mut pids = BTreeSet::new();
    for token in output.split_whitespace() {
        if let Ok(pid) = token.trim().parse::<u32>() {
            pids.insert(pid);
        }
    }
    pids.into_iter().collect()
}

fn parse_lsof_port_holder_pids_for_host(output: &str, host: &str) -> Vec<u32> {
    let mut pids = BTreeSet::new();
    let mut current_pid = None;

    for line in output.lines() {
        let Some(prefix) = line.chars().next() else {
            continue;
        };
        let value = &line[prefix.len_utf8()..];
        match prefix {
            'p' => {
                current_pid = value.trim().parse::<u32>().ok();
            }
            'n' => {
                let Some(pid) = current_pid else {
                    continue;
                };
                let endpoint = value
                    .trim()
                    .strip_prefix("TCP ")
                    .unwrap_or_else(|| value.trim())
                    .split_whitespace()
                    .next()
                    .unwrap_or_default();
                let Some(listener_host) = extract_socket_host(endpoint) else {
                    continue;
                };
                if listener_host_matches_request(listener_host, host) {
                    pids.insert(pid);
                }
            }
            _ => {}
        }
    }

    pids.into_iter().collect()
}

fn extract_socket_host(endpoint: &str) -> Option<&str> {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let (host, _) = rest.split_once("]:")?;
        return Some(host);
    }
    let (host, _) = trimmed.rsplit_once(':')?;
    Some(host)
}

fn listener_host_matches_request(listener_host: &str, requested_host: &str) -> bool {
    let listener_host = normalize_socket_host(listener_host);
    let requested_host = normalize_socket_host(requested_host);

    if is_wildcard_host(&requested_host) {
        return wildcard_request_conflicts_with_listener(&listener_host, &requested_host);
    }
    if is_wildcard_host(&listener_host) {
        return true;
    }
    if requested_host.eq_ignore_ascii_case("localhost") {
        return is_loopback_host(&listener_host);
    }
    if listener_host.eq_ignore_ascii_case("localhost") {
        return is_loopback_host(&requested_host);
    }
    match (
        parse_canonical_ip(&listener_host),
        parse_canonical_ip(&requested_host),
    ) {
        (Some(listener_ip), Some(requested_ip)) => {
            // Direct IP match (covers IPv4-mapped IPv6 via canonicalization).
            listener_ip == requested_ip
            // Cross-family loopback: on dual-stack systems 127.0.0.1 and ::1
            // both serve loopback traffic and conflict with each other.
            // Only match cross-family (V4↔V6), not same-family different-address
            // (e.g. 127.0.0.1 vs 127.0.0.2 are distinct listeners).
            || (listener_ip.is_loopback()
                && requested_ip.is_loopback()
                && listener_ip.is_ipv4() != requested_ip.is_ipv4())
        }
        _ => listener_host.eq_ignore_ascii_case(&requested_host),
    }
}

fn wildcard_request_conflicts_with_listener(listener_host: &str, requested_host: &str) -> bool {
    if is_wildcard_host(listener_host) || listener_host.eq_ignore_ascii_case("localhost") {
        return true;
    }

    match (
        parse_canonical_ip(listener_host),
        parse_canonical_ip(requested_host),
    ) {
        (Some(listener_ip), Some(requested_ip)) => {
            if requested_ip.is_ipv6() && requested_ip.is_unspecified() {
                // IPv6 wildcard often covers IPv4 on dual-stack OS
                true
            } else {
                listener_ip.is_ipv4() == requested_ip.is_ipv4()
            }
        }
        // Be conservative for named hosts: if we cannot prove they do not conflict,
        // keep them in the candidate set so restart logic can inspect the listener PID.
        _ => true,
    }
}

fn normalize_socket_host(host: &str) -> String {
    host.trim().trim_matches(['[', ']']).to_string()
}

fn parse_canonical_ip(host: &str) -> Option<IpAddr> {
    let ip = host.parse::<IpAddr>().ok()?;
    Some(canonicalize_ip(ip))
}

fn canonicalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
    }
}

fn is_wildcard_host(host: &str) -> bool {
    host == "*"
        || matches!(parse_canonical_ip(host), Some(IpAddr::V4(v4)) if v4.is_unspecified())
        || matches!(parse_canonical_ip(host), Some(IpAddr::V6(v6)) if v6.is_unspecified())
}

fn is_loopback_host(host: &str) -> bool {
    parse_canonical_ip(host).is_some_and(|ip| ip.is_loopback())
}

/// Check if a PID belongs to an Agent Mail process by inspecting its
/// command line or executable path. This intentionally requires an explicit
/// Agent Mail binary signature; ambiguous names like `am` are not sufficient.
fn pid_is_agent_mail(pid: u32) -> bool {
    pid_command_line(pid).is_some_and(|command| command_line_has_agent_mail_signature(&command))
        || pid_executable_basename(pid)
            .is_some_and(|basename| executable_name_has_agent_mail_signature(&basename))
}

fn pid_executable_path_matches(pid: u32, expected_path: &str) -> bool {
    let Some(actual_path) = pid_executable_path(pid) else {
        return false;
    };

    canonicalize_process_path(expected_path)
        == canonicalize_process_path(actual_path.to_string_lossy().as_ref())
}

fn canonicalize_process_path(path: &str) -> PathBuf {
    let candidate = PathBuf::from(path);
    std::fs::canonicalize(&candidate).unwrap_or(candidate)
}

fn command_line_has_agent_mail_signature(command: &str) -> bool {
    let Some(argv0) = command.split_whitespace().next() else {
        return false;
    };
    let basename = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0);
    executable_name_has_agent_mail_signature(basename)
}

fn executable_name_has_agent_mail_signature(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "am" | "am.exe"
            | "agent-mail"
            | "agent-mail.exe"
            | "agent_mail"
            | "agent_mail.exe"
            | "mcp-agent-mail"
            | "mcp_agent_mail"
            | "mcp-agent-mail.exe"
            | "mcp_agent_mail.exe"
            | "mcp-agent-mail-cli"
            | "mcp_agent_mail_cli"
            | "mcp-agent-mail-cli.exe"
            | "mcp_agent_mail_cli.exe"
    )
}

#[cfg(target_os = "linux")]
fn pid_command_line(pid: u32) -> Option<String> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let segments: Vec<String> = cmdline
        .split(|&b| b == 0)
        .filter(|segment| !segment.is_empty())
        .map(|segment| String::from_utf8_lossy(segment).into_owned())
        .collect();
    (!segments.is_empty()).then(|| segments.join(" "))
}

#[cfg(not(target_os = "linux"))]
fn pid_command_line(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!command.is_empty()).then_some(command)
}

#[cfg(target_os = "linux")]
fn pid_executable_basename(pid: u32) -> Option<String> {
    let exe = pid_executable_path(pid)?;
    exe.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "linux"))]
fn pid_executable_basename(pid: u32) -> Option<String> {
    let exe = pid_executable_path(pid)?;
    exe.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

#[cfg(target_os = "linux")]
fn pid_executable_path(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(target_os = "linux"))]
fn pid_executable_path(pid: u32) -> Option<PathBuf> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout);
    let argv0 = command.split_whitespace().next()?.trim();
    (!argv0.is_empty()).then(|| PathBuf::from(argv0))
}

// ──────────────────────────────────────────────────────────────────────
// Probe result types
// ──────────────────────────────────────────────────────────────────────

/// Outcome of a single startup probe.
#[derive(Debug, Clone)]
pub enum ProbeResult {
    /// Probe passed.
    Ok { name: &'static str },
    /// Probe failed with remediation guidance.
    Fail(ProbeFailure),
}

/// Details of a failed probe.
#[derive(Debug, Clone)]
pub struct ProbeFailure {
    /// Short probe identifier (e.g., "port", "database", "storage").
    pub name: &'static str,
    /// One-line problem description.
    pub problem: String,
    /// Actionable remediation steps.
    pub fix: String,
}

impl fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] Problem: {}\n        Fix: {}",
            self.name, self.problem, self.fix
        )
    }
}

/// Aggregate result of all startup probes.
#[derive(Debug)]
pub struct StartupReport {
    pub results: Vec<ProbeResult>,
}

const STARTUP_PRIMARY_FAILURE_PRIORITY: &[&str] = &[
    "database",
    "db-lock",
    "storage",
    "integrity",
    "port",
    "auth",
    "http_path",
];

fn startup_failure_priority(name: &str) -> usize {
    STARTUP_PRIMARY_FAILURE_PRIORITY
        .iter()
        .position(|candidate| *candidate == name)
        .unwrap_or(STARTUP_PRIMARY_FAILURE_PRIORITY.len())
}

impl StartupReport {
    /// Returns all failures.
    #[must_use]
    pub fn failures(&self) -> Vec<&ProbeFailure> {
        self.results
            .iter()
            .filter_map(|r| match r {
                ProbeResult::Fail(f) => Some(f),
                ProbeResult::Ok { .. } => None,
            })
            .collect()
    }

    /// Whether all probes passed.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.failures().is_empty()
    }

    /// Returns the failure that best explains why startup is blocked.
    #[must_use]
    pub fn primary_failure(&self) -> Option<&ProbeFailure> {
        self.failures()
            .into_iter()
            .min_by_key(|failure| startup_failure_priority(failure.name))
    }

    /// Format a human-readable error block for terminal output.
    ///
    /// When the primary issue is database/integrity/storage related, appends
    /// a recovery context block showing the active recovery mode, lock owner,
    /// next recommended action, and forensic bundle path.
    #[must_use]
    pub fn format_errors(&self) -> String {
        use fmt::Write;
        let failures = self.failures();
        let Some(primary) = self.primary_failure() else {
            return String::new();
        };
        let secondary: Vec<_> = failures
            .into_iter()
            .filter(|failure| !std::ptr::eq(*failure, primary))
            .collect();
        let mut out = String::new();
        out.push_str("\n  Startup probe summary:\n\n");
        let _ = writeln!(
            out,
            "  Primary issue: [{}] {}",
            primary.name, primary.problem
        );
        let _ = writeln!(out, "  Next action: {}", primary.fix);
        if !secondary.is_empty() {
            let _ = writeln!(
                out,
                "\n  Secondary findings: {} additional failing probe(s).",
                secondary.len()
            );
            for (i, fail) in secondary.iter().enumerate() {
                let _ = writeln!(out, "  {}. [{}] {}", i + 1, fail.name, fail.problem);
                let _ = writeln!(out, "     Next action: {}", fail.fix);
            }
        }

        // Append recovery context when the failure is durability-related.
        if matches!(
            primary.name,
            "database" | "db-lock" | "integrity" | "storage"
        ) {
            if let Some(ctx) = format_recovery_context() {
                let _ = write!(out, "{ctx}");
            }
        }

        out
    }
}

/// Threshold (seconds) beyond which a held recovery lock is considered stalled
/// for the purpose of startup error context.
const STARTUP_RECOVERY_STALL_THRESHOLD_SECS: u64 = 300; // 5 minutes

/// Build the recovery-context section appended to startup errors.
///
/// Inspects the recovery lock, ownership, durability state, admission
/// controller, and deferred-write backlog to surface:
/// - Active recovery mode and phase
/// - Current lock owner / competing PIDs
/// - Elapsed time since recovery started
/// - Stall detection and reason
/// - Deferred-write backlog summary
/// - Next recommended action
/// - Most recent forensic bundle path (if any)
fn format_recovery_context() -> Option<String> {
    use fmt::Write;
    use mcp_agent_mail_db::mailbox_verdict::{
        DurabilityState, VerdictOptions, compute_mailbox_verdict,
    };
    use mcp_agent_mail_db::pool::{
        MailboxOwnershipDisposition, inspect_mailbox_ownership, inspect_mailbox_recovery_lock,
        resolve_mailbox_sqlite_path,
    };

    let config = mcp_agent_mail_core::Config::get();
    let resolved = resolve_mailbox_sqlite_path(&config.database_url).ok()?;
    let db_path = PathBuf::from(&resolved.canonical_path);
    let storage_root = &config.storage_root;

    let recovery_lock = inspect_mailbox_recovery_lock(&db_path);
    let ownership = inspect_mailbox_ownership(&db_path, storage_root.as_path());

    let verdict = compute_mailbox_verdict(
        &config.database_url,
        storage_root.as_path(),
        &VerdictOptions {
            skip_integrity_check: true,
            ..VerdictOptions::default()
        },
    );
    let durability = DurabilityState::from_mailbox_state(verdict.state);

    // Only emit context when something is wrong.
    if durability == DurabilityState::Healthy && !recovery_lock.active {
        return None;
    }

    let mut out = String::new();
    out.push_str("\n  Recovery context:\n");
    let _ = writeln!(out, "    Mode:   {durability}");

    // Phase descriptor.
    let phase = if recovery_lock.active {
        "lock_held"
    } else if recovery_lock.exists {
        "lock_stale"
    } else {
        match durability {
            DurabilityState::Corrupt => "corrupt_no_lock",
            _ => "degraded_no_lock",
        }
    };
    let _ = writeln!(out, "    Phase:  {phase}");

    let owner_desc = match ownership.disposition {
        MailboxOwnershipDisposition::Unowned => "none".to_string(),
        MailboxOwnershipDisposition::ActiveOtherOwner => ownership.processes.first().map_or_else(
            || "active (unknown pid)".to_string(),
            |proc| format!("pid {} (active)", proc.pid),
        ),
        MailboxOwnershipDisposition::StaleLiveProcess => ownership.processes.first().map_or_else(
            || "stale (unknown pid)".to_string(),
            |proc| format!("pid {} (stale)", proc.pid),
        ),
        MailboxOwnershipDisposition::DeletedExecutable => ownership.processes.first().map_or_else(
            || "deleted executable".to_string(),
            |proc| format!("pid {} (deleted executable)", proc.pid),
        ),
        MailboxOwnershipDisposition::SplitBrain => format!(
            "split-brain ({} competing pids)",
            ownership.competing_pids.len()
        ),
    };
    let _ = writeln!(out, "    Owner:  {owner_desc}");

    if recovery_lock.active {
        let lock_holder = recovery_lock
            .pid
            .map_or("unknown".to_string(), |pid| format!("pid {pid}"));
        let _ = writeln!(out, "    Lock:   recovery lock held by {lock_holder}");
    }

    // ── Elapsed time since recovery lock created ─────────────────────────
    let lock_path = PathBuf::from(&recovery_lock.lock_path);
    let elapsed_secs = if lock_path.exists() {
        std::fs::metadata(&lock_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|mtime| {
                std::time::SystemTime::now()
                    .duration_since(mtime)
                    .ok()
                    .map(|d| d.as_secs())
            })
    } else {
        None
    };
    if let Some(age) = elapsed_secs {
        let _ = writeln!(out, "    Elapsed: {}", format_recovery_elapsed(age));
    }

    // ── Stall detection ──────────────────────────────────────────────────
    let adm = mcp_agent_mail_db::recovery_admission().status();
    let dw_status = mcp_agent_mail_db::deferred_write_queue().status();
    let mut stall_reasons: Vec<&str> = Vec::new();

    if recovery_lock.active {
        if let Some(age) = elapsed_secs {
            if age >= STARTUP_RECOVERY_STALL_THRESHOLD_SECS {
                stall_reasons.push("lock held beyond stall threshold");
            }
        }
    }
    if adm.suppressed {
        stall_reasons.push("admission suppressed after repeated failures");
    }
    if matches!(
        dw_status.pressure,
        mcp_agent_mail_db::BacklogPressure::HardStop
    ) {
        stall_reasons.push("deferred-write queue at hard-stop");
    }
    if recovery_lock.exists && !recovery_lock.active && durability != DurabilityState::Healthy {
        stall_reasons.push("stale recovery lock from dead process");
    }

    if !stall_reasons.is_empty() {
        let _ = writeln!(out, "    Stall:  YES ({})", stall_reasons.join("; "));
    } else if recovery_lock.active {
        let _ = writeln!(out, "    Stall:  no (still within budget)");
    }

    // ── Deferred-write backlog ───────────────────────────────────────────
    if dw_status.active || dw_status.sealed || dw_status.queued > 0 {
        let pressure_label = match dw_status.pressure {
            mcp_agent_mail_db::BacklogPressure::Normal => "normal",
            mcp_agent_mail_db::BacklogPressure::Elevated => "elevated",
            mcp_agent_mail_db::BacklogPressure::Critical => "critical",
            mcp_agent_mail_db::BacklogPressure::HardStop => "hard_stop",
        };
        let _ = writeln!(
            out,
            "    Backlog: {}/{} writes queued, oldest {}s, pressure={pressure_label}",
            dw_status.queued, dw_status.capacity, dw_status.oldest_age_secs,
        );
    }

    // ── Admission controller ─────────────────────────────────────────────
    if adm.consecutive_failures > 0 || adm.suppressed {
        let _ = writeln!(
            out,
            "    Admission: {} consecutive failures, {} attempts in window{}",
            adm.consecutive_failures,
            adm.attempts_in_window,
            if adm.suppressed { ", SUPPRESSED" } else { "" },
        );
    }

    // ── Next action (enriched with stall context) ────────────────────────
    let stalled = !stall_reasons.is_empty();
    let next_action = if stalled {
        match durability {
            DurabilityState::Recovering | DurabilityState::DegradedReadOnly => {
                if recovery_lock.exists && !recovery_lock.active {
                    "Recovery lock is stale (process exited); run `am doctor repair` to restart"
                } else if adm.suppressed {
                    "Recovery suppressed after repeated failures; run `am doctor repair --yes` to override"
                } else {
                    "Recovery appears stalled; investigate lock holder or run `am doctor repair --yes`"
                }
            }
            DurabilityState::Corrupt => {
                "Run `am doctor repair --yes` or restore from archive backup"
            }
            DurabilityState::Healthy => "No action required",
        }
    } else {
        match durability {
            DurabilityState::Healthy => "No action required",
            DurabilityState::DegradedReadOnly => {
                if recovery_lock.active {
                    "Recovery in progress; still within budget, no action needed yet"
                } else {
                    "Run `am doctor repair` to attempt automatic recovery"
                }
            }
            DurabilityState::Recovering => {
                "Recovery in progress; wait for completion or investigate stall"
            }
            DurabilityState::Corrupt => {
                "Run `am doctor repair --yes` or restore from archive backup"
            }
        }
    };
    let _ = writeln!(out, "    Action: {next_action}");

    // Find latest forensic bundle.
    let forensics_dir = if storage_root.is_dir() {
        storage_root.join("doctor").join("forensics")
    } else {
        db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("doctor")
            .join("forensics")
    };
    if forensics_dir.is_dir() {
        if let Some(bundle) = find_latest_bundle_in(&forensics_dir) {
            let _ = writeln!(out, "    Bundle: {}", bundle.display());
        }
    }

    Some(out)
}

/// Format elapsed recovery time as a compact human-readable string.
fn format_recovery_elapsed(total_secs: u64) -> String {
    if total_secs < 60 {
        format!("{total_secs}s")
    } else if total_secs < 3600 {
        let m = total_secs / 60;
        let s = total_secs % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
    } else {
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        format!("{h}h {m}m")
    }
}

/// Scan a forensics root for the most recently modified bundle directory.
fn find_latest_bundle_in(forensics_dir: &std::path::Path) -> Option<PathBuf> {
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    let families = std::fs::read_dir(forensics_dir).ok()?;
    for family_entry in families.flatten() {
        let family_path = family_entry.path();
        if !family_path.is_dir() {
            continue;
        }
        let Ok(bundles) = std::fs::read_dir(&family_path) else {
            continue;
        };
        for bundle_entry in bundles.flatten() {
            let bundle_path = bundle_entry.path();
            if !bundle_path.is_dir() {
                continue;
            }
            let mtime = bundle_entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if latest.as_ref().is_none_or(|(prev, _)| mtime > *prev) {
                latest = Some((mtime, bundle_path));
            }
        }
    }
    latest.map(|(_, path)| path)
}

// ──────────────────────────────────────────────────────────────────────
// Individual probes
// ──────────────────────────────────────────────────────────────────────

/// Check that the HTTP path starts with `/` and ends with `/`.
fn probe_http_path(config: &Config) -> ProbeResult {
    let path = &config.http_path;
    if path.is_empty() || !path.starts_with('/') {
        return ProbeResult::Fail(ProbeFailure {
            name: "http-path",
            problem: format!("HTTP path {path:?} must start with '/'"),
            fix: "Set HTTP_PATH to a value like '/mcp/' or '/api/'".into(),
        });
    }
    if !path.ends_with('/') {
        return ProbeResult::Fail(ProbeFailure {
            name: "http-path",
            problem: format!("HTTP path {path:?} should end with '/'"),
            fix: format!("Set HTTP_PATH=\"{path}/\" (append trailing slash)"),
        });
    }
    ProbeResult::Ok { name: "http-path" }
}

/// Check that the configured port is available for binding.
///
/// Uses cross-platform port detection via `check_port_status()`:
/// - If the port is free, the probe passes.
/// - If an Agent Mail server is already running, the probe fails with reuse guidance.
/// - If another process is using the port, the probe fails with guidance.
fn probe_port(config: &Config) -> ProbeResult {
    match check_port_status(&config.http_host, config.http_port) {
        PortStatus::Free => ProbeResult::Ok { name: "port" },

        PortStatus::AgentMailServer => ProbeResult::Fail(ProbeFailure {
            name: "port",
            problem: format!(
                "An Agent Mail server is already running on {}:{}",
                config.http_host, config.http_port
            ),
            fix: "Reuse the running server (for CLI: use --reuse-running), stop the existing server, or choose a different HTTP_PORT".into(),
        }),

        PortStatus::OtherProcess { description } => ProbeResult::Fail(ProbeFailure {
            name: "port",
            problem: format!(
                "Port {} is already in use on {} by another process. {}",
                config.http_port, config.http_host, description
            ),
            fix: format!(
                "Stop the other process using port {}, or set HTTP_PORT to a different port",
                config.http_port
            ),
        }),

        PortStatus::Error { kind, message } => {
            let (problem, fix) = match kind {
                std::io::ErrorKind::PermissionDenied => (
                    format!(
                        "Permission denied binding to {}:{}",
                        config.http_host, config.http_port
                    ),
                    if config.http_port < 1024 {
                        format!(
                            "Ports below 1024 require elevated privileges. Use HTTP_PORT={} or higher",
                            1024
                        )
                    } else {
                        "Check your firewall or OS security settings".into()
                    },
                ),
                std::io::ErrorKind::AddrNotAvailable => (
                    format!(
                        "Address {}:{} is not available",
                        config.http_host, config.http_port
                    ),
                    format!(
                        "The host {:?} may not be a valid local address. Try HTTP_HOST=127.0.0.1 or HTTP_HOST=0.0.0.0",
                        config.http_host
                    ),
                ),
                _ => (
                    format!(
                        "Cannot bind to {}:{}: {}",
                        config.http_host, config.http_port, message
                    ),
                    "Check network configuration and try a different port/host".into(),
                ),
            };
            ProbeResult::Fail(ProbeFailure {
                name: "port",
                problem,
                fix,
            })
        }
    }
}

/// Check that the storage root directory exists (or can be created) and is writable.
fn probe_storage_root(config: &Config) -> ProbeResult {
    let root = &config.storage_root;

    // Try to create if it doesn't exist
    if !root.exists()
        && let Err(e) = std::fs::create_dir_all(root)
    {
        return ProbeResult::Fail(ProbeFailure {
            name: "storage",
            problem: format!("Cannot create storage directory {}: {e}", root.display()),
            fix: format!("Create the directory manually: mkdir -p {}", root.display()),
        });
    }

    // Check it is a directory
    if !root.is_dir() {
        return ProbeResult::Fail(ProbeFailure {
            name: "storage",
            problem: format!("{} exists but is not a directory", root.display()),
            fix: format!(
                "Remove the file at {} and let the server create the directory",
                root.display()
            ),
        });
    }

    // Check writability via a unique, create_new probe to avoid clobbering files.
    let probe_nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let probe_path = root.join(format!(
        ".am_startup_probe-{}-{probe_nonce}",
        std::process::id()
    ));
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe_path)
    {
        Ok(mut file) => {
            if let Err(e) = file.write_all(b"ok") {
                drop(file);
                let _ = std::fs::remove_file(&probe_path);
                return ProbeResult::Fail(ProbeFailure {
                    name: "storage",
                    problem: format!("Storage directory {} is not writable: {e}", root.display()),
                    fix: format!("Check permissions: chmod u+w {}", root.display()),
                });
            }
            drop(file);
            let _ = std::fs::remove_file(&probe_path);
            ProbeResult::Ok { name: "storage" }
        }
        Err(e) => ProbeResult::Fail(ProbeFailure {
            name: "storage",
            problem: format!("Storage directory {} is not writable: {e}", root.display()),
            fix: format!("Check permissions: chmod u+w {}", root.display()),
        }),
    }
}

/// Check that the database URL is plausible and the database is reachable.
fn probe_database(config: &Config) -> ProbeResult {
    let url = &config.database_url;

    // Basic URL format check
    if url.is_empty() {
        return ProbeResult::Fail(ProbeFailure {
            name: "database",
            problem: "DATABASE_URL is empty".into(),
            fix: "Set DATABASE_URL to a SQLite path like 'sqlite:///./storage.sqlite3'".into(),
        });
    }

    // For SQLite URLs, check parent directory exists.
    if url.starts_with("sqlite://") || url.starts_with("sqlite+aiosqlite://") {
        if is_sqlite_memory_database_url(url) {
            return ProbeResult::Ok { name: "database" };
        }
        let Some(path) = resolve_server_database_url_sqlite_path(url) else {
            return ProbeResult::Fail(ProbeFailure {
                name: "database",
                problem: format!("Invalid SQLite database URL: {url}"),
                fix: "Use a valid SQLite URL like 'sqlite:///./storage.sqlite3'".into(),
            });
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            return ProbeResult::Fail(ProbeFailure {
                name: "database",
                problem: format!(
                    "Database parent directory does not exist: {}",
                    parent.display()
                ),
                fix: format!("Create it: mkdir -p {}", parent.display()),
            });
        }
    }

    ProbeResult::Ok { name: "database" }
}

/// Run `PRAGMA quick_check` on the database to detect corruption.
///
/// When corruption is detected, attempts automatic recovery:
///
/// 1. Restore from a healthy `.bak` / `.backup-*` / `.recovery*` file.
/// 2. If no healthy backup exists, reinitialize an empty database.
///
/// Startup only fails if recovery itself fails. Successful recovery
/// logs a warning and allows startup to continue.
///
/// Skipped when `INTEGRITY_CHECK_ON_STARTUP=false` or for in-memory databases.
#[allow(dead_code)]
fn probe_integrity(config: &Config) -> ProbeResult {
    if !config.integrity_check_on_startup {
        return ProbeResult::Ok { name: "integrity" };
    }

    if is_sqlite_memory_database_url(&config.database_url) {
        return ProbeResult::Ok { name: "integrity" };
    }

    // Skip integrity probe for fresh installs to avoid noisy recovery warnings.
    if let Some(path) = resolve_server_database_url_sqlite_path(&config.database_url)
        && !path.exists()
        && !std::path::Path::new(&config.storage_root)
            .join("projects")
            .is_dir()
    {
        return ProbeResult::Ok { name: "integrity" };
    }

    // Retry the activity lock with short backoff.  After auto_clear_db_blockers
    // kills a stale Agent Mail process, the kernel may need a few hundred
    // milliseconds to fully release flock() descriptors (especially on macOS).
    // Without this retry the integrity probe would immediately fail with a
    // misleading "mailbox activity lock is busy" error.
    let _integrity_activity_lock = {
        const MAX_LOCK_ATTEMPTS: u32 = 8;
        const LOCK_BACKOFF_MS: u64 = 150;
        let mut last_err = String::new();
        let mut acquired = None;
        for attempt in 0..MAX_LOCK_ATTEMPTS {
            match acquire_mailbox_activity_lock_for_database_url(
                &config.database_url,
                MailboxActivityLockMode::Exclusive,
            ) {
                Ok(lock) => {
                    acquired = Some(lock);
                    break;
                }
                Err(err) => {
                    last_err = err.to_string();
                    if attempt + 1 < MAX_LOCK_ATTEMPTS {
                        tracing::debug!(
                            attempt = attempt + 1,
                            max = MAX_LOCK_ATTEMPTS,
                            "activity lock busy during integrity probe; retrying after {}ms",
                            LOCK_BACKOFF_MS
                        );
                        std::thread::sleep(std::time::Duration::from_millis(LOCK_BACKOFF_MS));
                    }
                }
            }
        }
        match acquired {
            Some(lock) => lock,
            None => return integrity_busy_probe_failure(config, &last_err),
        }
    };

    let pool_config = DbPoolConfig {
        database_url: config.database_url.clone(),
        storage_root: Some(config.storage_root.clone()),
        min_connections: 1,
        max_connections: 1,
        run_migrations: false,
        warmup_connections: 0,
        ..DbPoolConfig::default()
    };

    // Helper: returns true when the error looks like a stale WAL / snapshot
    // conflict that can be fixed by removing WAL+SHM sidecars. These are NOT
    // genuine "another process is busy" situations — they mean the DB file has
    // a WAL that is inconsistent with the main file (e.g. after a crash or
    // SIGKILL). Treating them as hard "busy" failures causes `am` to refuse
    // to start when `doctor repair --yes` would succeed immediately.
    let is_snapshot_conflict =
        |msg: &str| -> bool { mcp_agent_mail_db::is_sqlite_snapshot_conflict_error_message(msg) };

    // Helper: attempt WAL removal + pool retry, falling back to full recovery.
    let try_wal_cleanup_and_retry = |pool_config: &DbPoolConfig, config: &Config| -> ProbeResult {
        if let Some(db_path) = resolve_server_database_url_sqlite_path(&config.database_url) {
            if try_remove_corrupt_wal(&db_path) {
                tracing::info!(
                    path = %db_path.display(),
                    "removed corrupt/stale WAL/SHM sidecars; retrying pool open"
                );
                if let Ok(p) = mcp_agent_mail_db::DbPool::new(pool_config) {
                    tracing::info!("pool open succeeded after WAL removal");
                    match p.run_startup_integrity_check() {
                        Ok(_) => return ProbeResult::Ok { name: "integrity" },
                        Err(ref e) => {
                            tracing::warn!(
                                error = %e,
                                "integrity check failed after WAL removal; falling back to recovery"
                            );
                        }
                    }
                }
            }
        }
        attempt_probe_recovery(config)
    };

    let pool = match mcp_agent_mail_db::DbPool::new(&pool_config) {
        Ok(p) => p,
        Err(e) => {
            let err_str = e.to_string();

            // Snapshot conflicts (stale WAL) are recoverable — remove the WAL
            // and retry before giving up.  Must check BEFORE the generic
            // is_lock_error test because snapshot conflicts match both.
            if is_snapshot_conflict(&err_str) {
                tracing::warn!(
                    error = %err_str,
                    "pool open hit snapshot conflict; attempting WAL cleanup"
                );
                return try_wal_cleanup_and_retry(&pool_config, config);
            }

            if mcp_agent_mail_db::is_lock_error(&err_str) {
                return integrity_busy_probe_failure(config, &err_str);
            }

            // Corruption (malformed schema, WAL too small, etc.) — try WAL
            // cleanup first, fall back to full archive reconstruction.
            if mcp_agent_mail_db::is_corruption_error_message(&err_str) {
                return try_wal_cleanup_and_retry(&pool_config, config);
            }

            return ProbeResult::Fail(ProbeFailure {
                name: "integrity",
                problem: format!("Cannot create pool for integrity check: {e}"),
                fix: "Check DATABASE_URL or set INTEGRITY_CHECK_ON_STARTUP=false to skip".into(),
            });
        }
    };

    match pool.run_startup_integrity_check() {
        Ok(_) => ProbeResult::Ok { name: "integrity" },
        Err(ref e) => {
            let err_str = e.to_string();

            // Snapshot conflict during integrity check — same recovery as above.
            if is_snapshot_conflict(&err_str) {
                tracing::warn!(
                    error = %err_str,
                    "integrity check hit snapshot conflict; attempting WAL cleanup"
                );
                return try_wal_cleanup_and_retry(&pool_config, config);
            }

            if mcp_agent_mail_db::is_lock_error(&err_str) {
                return integrity_busy_probe_failure(config, &err_str);
            }

            tracing::warn!(
                error = %err_str,
                "startup integrity check failed; attempting automatic recovery"
            );
            attempt_probe_recovery(config)
        }
    }
}

/// Try to remove corrupt WAL/SHM sidecars so SQLite can fall back to the
/// main database file.  Returns `true` if at least one sidecar was removed.
fn try_remove_corrupt_wal(db_path: &std::path::Path) -> bool {
    let mut removed = false;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar_os = db_path.as_os_str().to_os_string();
        sidecar_os.push(suffix);
        let sidecar = std::path::PathBuf::from(sidecar_os);
        if sidecar.exists() {
            tracing::warn!(
                path = %sidecar.display(),
                "removing corrupt/stale WAL/SHM sidecar during startup recovery"
            );
            let _ = std::fs::remove_file(&sidecar);
            removed = true;
        }
    }
    removed
}

fn integrity_busy_probe_failure(config: &Config, detail: &str) -> ProbeResult {
    let db_target = resolve_server_database_url_sqlite_path(&config.database_url).map_or_else(
        || config.database_url.clone(),
        |path| path.display().to_string(),
    );
    ProbeResult::Fail(ProbeFailure {
        name: "integrity",
        problem: format!("SQLite integrity check could not run because {db_target} is busy: {detail}"),
        fix: "Stop any running `am`, `mcp-agent-mail`, or `am doctor ...` process using this mailbox, or wait for it to finish, then retry."
            .into(),
    })
}

/// Attempt file-level recovery when the integrity probe detects corruption.
///
/// Uses the archive-aware recovery path which tries, in order:
/// 1. Restore from a healthy `.bak` / `.backup-*` / `.recovery*` backup
/// 2. Reconstruct from the Git archive (recovers messages + agents)
/// 3. Reinitialize an empty database (last resort)
#[allow(dead_code)]
fn attempt_probe_recovery(config: &Config) -> ProbeResult {
    let Some(db_path) = resolve_server_database_url_sqlite_path(&config.database_url) else {
        return ProbeResult::Fail(ProbeFailure {
            name: "integrity",
            problem: "Cannot determine database file path for recovery".into(),
            fix: "Check DATABASE_URL format".into(),
        });
    };

    let storage_root = std::path::Path::new(&config.storage_root);

    // Capture live DB family, lock holders, and process inventory *before*
    // any repair or reconstruct mutates the mailbox state.
    let pre_snapshot =
        mcp_agent_mail_db::capture_pre_recovery_snapshot(&db_path, "startup-integrity")
            .with_environment(storage_root, &config.database_url);
    tracing::info!(
        trigger = pre_snapshot.trigger,
        db_bytes = ?pre_snapshot.db_bytes,
        wal_bytes = ?pre_snapshot.wal_bytes,
        holders = pre_snapshot.process_holders.len(),
        recovery_lock_active = pre_snapshot.recovery_lock_active,
        "startup pre-recovery snapshot captured"
    );

    let result = if storage_root.is_dir() {
        mcp_agent_mail_db::ensure_sqlite_file_healthy_with_archive(&db_path, storage_root)
    } else {
        mcp_agent_mail_db::ensure_sqlite_file_healthy(&db_path)
    };

    match result {
        Ok(()) => {
            tracing::warn!(
                path = %db_path.display(),
                "database auto-recovered from corruption; startup will continue with recovered data"
            );
            ProbeResult::Ok { name: "integrity" }
        }
        Err(e) => ProbeResult::Fail(ProbeFailure {
            name: "integrity",
            problem: format!("SQLite corruption detected and automatic recovery failed: {e}"),
            fix: format!(
                "Try these in order:\n\
                 1. `am doctor repair --yes` — automatic repair from backups/archive\n\
                 2. `am doctor reconstruct --yes` — rebuild the database from the Git archive\n\
                 3. `am clear-and-reset-everything` — archive current state and start completely fresh\n\
                 The corrupt file has been quarantined at {}.corrupt-*",
                db_path.display()
            ),
        }),
    }
}

/// Check auth configuration consistency.
fn probe_auth(config: &Config) -> ProbeResult {
    // Warn if bearer token is set but very short (likely a mistake)
    if let Some(ref token) = config.http_bearer_token
        && token.len() < 8
    {
        return ProbeResult::Fail(ProbeFailure {
            name: "auth",
            problem: "HTTP_BEARER_TOKEN is set but very short (< 8 chars)".into(),
            fix: "Use a longer token for security, or unset HTTP_BEARER_TOKEN to disable auth"
                .into(),
        });
    }

    if config.http_jwt_enabled {
        let jwks_url_present = config
            .http_jwt_jwks_url
            .as_deref()
            .is_some_and(|s| !s.is_empty());
        let secret_present = config
            .http_jwt_secret
            .as_deref()
            .is_some_and(|s| !s.is_empty());

        if !jwks_url_present && !secret_present {
            return ProbeResult::Fail(ProbeFailure {
                name: "auth",
                problem:
                    "JWT authentication is enabled but neither HTTP_JWT_JWKS_URL nor HTTP_JWT_SECRET is set"
                        .into(),
                fix: "Set HTTP_JWT_SECRET for HS256/HS384/HS512, or set HTTP_JWT_JWKS_URL for asymmetric algorithms (RS*/ES*)".into(),
            });
        }

        // If we're using a static secret without JWKS, only HS* algorithms make sense.
        if secret_present && !jwks_url_present {
            let mut algorithms: Vec<jsonwebtoken::Algorithm> = config
                .http_jwt_algorithms
                .iter()
                .filter_map(|s| s.parse::<jsonwebtoken::Algorithm>().ok())
                .collect();
            if algorithms.is_empty() {
                algorithms.push(jsonwebtoken::Algorithm::HS256);
            }
            let has_non_hs = algorithms.iter().any(|a| {
                !matches!(
                    a,
                    jsonwebtoken::Algorithm::HS256
                        | jsonwebtoken::Algorithm::HS384
                        | jsonwebtoken::Algorithm::HS512
                )
            });
            if has_non_hs {
                return ProbeResult::Fail(ProbeFailure {
                    name: "auth",
                    problem: "HTTP_JWT_SECRET is set but HTTP_JWT_ALGORITHMS includes non-HS* algorithms".into(),
                    fix: "Either restrict HTTP_JWT_ALGORITHMS to HS256/HS384/HS512 when using HTTP_JWT_SECRET, or set HTTP_JWT_JWKS_URL for asymmetric algorithms (RS*/ES*)".into(),
                });
            }
        }
    }

    ProbeResult::Ok { name: "auth" }
}

// ──────────────────────────────────────────────────────────────────────
// Main entry point
// ──────────────────────────────────────────────────────────────────────

/// Run a lightweight archive-DB consistency check on recent messages.
///
/// Samples the last `limit` messages from the DB and verifies that their
/// canonical archive files exist on disk. Reports count of missing files
/// but does NOT block startup (warnings only).
fn probe_consistency(config: &Config) -> ProbeResult {
    let pool_config = DbPoolConfig {
        database_url: config.database_url.clone(),
        storage_root: Some(config.storage_root.clone()),
        run_migrations: false,
        ..DbPoolConfig::default()
    };

    let Ok(pool) = mcp_agent_mail_db::DbPool::new(&pool_config) else {
        // If we can't open DB, skip consistency check (integrity probe
        // will catch the root cause).
        return ProbeResult::Ok {
            name: "consistency",
        };
    };

    // Sample last 100 messages for consistency check
    let limit = 100i64;
    let Ok(refs) = pool.sample_recent_message_refs(limit) else {
        // DB query failed; skip silently (other probes will catch DB issues).
        return ProbeResult::Ok {
            name: "consistency",
        };
    };

    if refs.is_empty() {
        return ProbeResult::Ok {
            name: "consistency",
        };
    }

    let report = mcp_agent_mail_storage::check_archive_consistency(&config.storage_root, &refs);

    if report.missing > 0 {
        tracing::warn!(
            sampled = report.sampled,
            found = report.found,
            missing = report.missing,
            missing_ids = ?report.missing_ids,
            "Archive-DB consistency: {} of {} sampled messages missing archive files",
            report.missing,
            report.sampled,
        );
    }

    // Consistency is advisory; never block startup
    ProbeResult::Ok {
        name: "consistency",
    }
}

/// Run the archive consistency probe as an advisory one-shot.
///
/// Intended for background execution so startup critical path stays focused on
/// hard readiness checks while still preserving consistency diagnostics.
pub fn run_consistency_probe_advisory(config: &Config) {
    let _ = probe_consistency(config);
}

/// Minimum recommended file descriptor limit for production workloads.
///
/// Under burst/multi-agent load, each connection + WAL + archive file can
/// consume FDs. Below this threshold the server may run out of FDs under
/// moderate concurrency.
const MIN_RECOMMENDED_NOFILE: u64 = 256;

/// Try to read the soft file descriptor limit from `/proc/self/limits` (Linux)
/// or `/dev/fd` directory scanning (macOS/BSD fallback).
///
/// Returns `None` if the limit cannot be determined.
fn read_fd_soft_limit() -> Option<u64> {
    // Linux: parse /proc/self/limits
    if let Ok(content) = std::fs::read_to_string("/proc/self/limits") {
        for line in content.lines() {
            if line.starts_with("Max open files") {
                // Format: "Max open files            1024                 1048576              files"
                let parts: Vec<&str> = line.split_whitespace().collect();
                // The soft limit is the 4th token (0-indexed: 3)
                if parts.len() >= 5
                    && let Ok(soft) = parts[3].parse::<u64>()
                {
                    return Some(soft);
                }
            }
        }
    }

    // macOS/BSD fallback: count entries in /dev/fd is unreliable,
    // so we skip the check on platforms without /proc.
    None
}

/// Check effective file descriptor limit and warn if too low for burst workloads.
///
/// See: <https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/18>
fn probe_fd_limit(_config: &Config) -> ProbeResult {
    if let Some(soft_limit) = read_fd_soft_limit() {
        if soft_limit < MIN_RECOMMENDED_NOFILE {
            tracing::warn!(
                soft_limit,
                recommended = MIN_RECOMMENDED_NOFILE,
                "file descriptor limit (ulimit -n) is low; may cause failures under burst load"
            );
            return ProbeResult::Ok { name: "fd_limit" };
        }
        tracing::debug!(soft_limit, "file descriptor limit check passed");
    }
    ProbeResult::Ok { name: "fd_limit" }
}

/// Check if the database file is exclusively locked by another process.
fn probe_db_lock(config: &Config) -> ProbeResult {
    match check_db_lock_status(config) {
        DbLockStatus::Available | DbLockStatus::Missing => ProbeResult::Ok { name: "db-lock" },
        DbLockStatus::Locked => {
            let Some(sqlite_path) = resolve_server_database_url_sqlite_path(&config.database_url)
            else {
                return ProbeResult::Ok { name: "db-lock" }; // Should be caught by probe_database
            };

            // Best-effort: try to identify the process holding the lock.
            let holder = identify_lock_holder(&sqlite_path);

            let problem = holder.as_ref().map_or_else(
                || {
                    format!(
                        "Database file {} is exclusively locked by another process",
                        sqlite_path.display(),
                    )
                },
                |h| {
                    format!(
                        "Database file {} is exclusively locked by PID {} ({})",
                        sqlite_path.display(),
                        h.pid,
                        h.cmdline,
                    )
                },
            );

            let fix = match &holder {
                Some(h) if h.is_python => format!(
                    "A Python process (PID {}) appears to hold the lock — this is likely a legacy Python HTTP worker. \
                     Stop it with `kill {}`, then retry.",
                    h.pid, h.pid,
                ),
                Some(h) => format!(
                    "Stop the process holding the lock (PID {}: {}) or wait for it to release the database.",
                    h.pid, h.cmdline,
                ),
                None => "Ensure no other 'am' or 'mcp-agent-mail' instances are running with the same database, \
                         or wait for background tasks to complete."
                    .into(),
            };

            ProbeResult::Fail(ProbeFailure {
                name: "db-lock",
                problem,
                fix,
            })
        }
        DbLockStatus::Error(msg) => ProbeResult::Fail(ProbeFailure {
            name: "db-lock",
            problem: format!("Database lock probe failed: {msg}"),
            fix: "Check database file permissions and path validity.".into(),
        }),
    }
}

/// Run all startup probes and return a report.
///
/// The probes are ordered from fastest to slowest, and all probes run
/// even if earlier ones fail (so the user sees all problems at once).
fn shared_runtime_startup_probes(config: &Config) -> Vec<ProbeResult> {
    vec![
        probe_database(config),
        probe_db_lock(config),
        probe_storage_root(config),
        probe_integrity(config),
        probe_fd_limit(config),
    ]
}

/// Run the full HTTP/TUI startup probe set.
#[must_use]
pub fn run_startup_probes(config: &Config) -> StartupReport {
    let mut results = run_http_startup_preflight_probes(config).results;
    results.push(probe_port(config));
    StartupReport { results }
}

/// Run the HTTP/TUI startup probes that do not depend on replacing the listener.
///
/// Call this before deciding whether it is safe to tear down an existing server.
#[must_use]
pub fn run_http_startup_preflight_probes(config: &Config) -> StartupReport {
    let existing_agent_mail_server = matches!(
        check_port_status(&config.http_host, config.http_port),
        PortStatus::AgentMailServer
    );
    let mut results = vec![
        probe_http_path(config),
        probe_auth(config),
        probe_database(config),
    ];
    if existing_agent_mail_server {
        tracing::info!(
            host = %config.http_host,
            port = config.http_port,
            "HTTP preflight detected an existing Agent Mail listener; deferring db-lock and integrity probes until after port handoff"
        );
    } else {
        results.push(probe_db_lock(config));
    }
    results.push(probe_storage_root(config));
    if !existing_agent_mail_server {
        results.push(probe_integrity(config));
    }
    results.push(probe_fd_limit(config));
    StartupReport { results }
}

/// Run the stdio startup probe set.
///
/// Stdio transport does not bind an HTTP listener, so HTTP-path/auth/port
/// checks are intentionally omitted to avoid noisy false positives.
#[must_use]
pub fn run_stdio_startup_probes(config: &Config) -> StartupReport {
    StartupReport {
        results: shared_runtime_startup_probes(config),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;

    fn default_config() -> Config {
        Config::default()
    }

    #[test]
    fn default_config_passes_http_path() {
        let config = default_config();
        let result = probe_http_path(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
    }

    #[test]
    fn empty_http_path_fails() {
        let mut config = default_config();
        config.http_path = String::new();
        let result = probe_http_path(&config);
        assert!(matches!(result, ProbeResult::Fail(_)));
    }

    #[test]
    fn no_leading_slash_fails() {
        let mut config = default_config();
        config.http_path = "mcp/".into();
        let result = probe_http_path(&config);
        assert!(matches!(result, ProbeResult::Fail(_)));
    }

    #[test]
    fn no_trailing_slash_fails() {
        let mut config = default_config();
        config.http_path = "/mcp".into();
        let result = probe_http_path(&config);
        assert!(matches!(result, ProbeResult::Fail(_)));
    }

    #[test]
    fn valid_http_path_passes() {
        let mut config = default_config();
        config.http_path = "/mcp/".into();
        let result = probe_http_path(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
    }

    #[test]
    fn default_config_passes_auth() {
        let config = default_config();
        let result = probe_auth(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
    }

    #[test]
    fn short_bearer_token_fails() {
        let mut config = default_config();
        config.http_bearer_token = Some("abc".into());
        let result = probe_auth(&config);
        assert!(matches!(result, ProbeResult::Fail(_)));
    }

    #[test]
    fn valid_bearer_token_passes() {
        let mut config = default_config();
        config.http_bearer_token = Some("a-secure-token-here".into());
        let result = probe_auth(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
    }

    #[test]
    fn empty_database_url_fails() {
        let mut config = default_config();
        config.database_url = String::new();
        let result = probe_database(&config);
        assert!(matches!(result, ProbeResult::Fail(_)));
    }

    #[test]
    fn default_database_url_passes() {
        let config = default_config();
        let result = probe_database(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
    }

    #[test]
    fn probe_database_uses_absolute_candidate_for_missing_relative_database_url() {
        let temp = tempfile::tempdir().expect("tempdir");
        let absolute_db = temp.path().join("probe-absolute.sqlite3");
        std::fs::write(&absolute_db, b"placeholder").expect("create absolute db");

        let relative_path =
            std::path::PathBuf::from(absolute_db.to_string_lossy().trim_start_matches('/'));
        assert!(
            !relative_path.exists(),
            "relative shadow path should be absent so absolute candidate fallback is exercised"
        );

        let mut config = default_config();
        config.database_url = format!("sqlite:///{}", relative_path.display());

        let result = probe_database(&config);
        assert!(
            matches!(result, ProbeResult::Ok { name: "database" }),
            "probe_database should accept the resolved absolute candidate: {result:?}"
        );
    }

    #[test]
    fn integrity_probe_missing_db_without_archive_is_ok_for_fresh_startup() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("fresh.sqlite3");
        let storage_root = temp.path().join("storage");

        let mut config = default_config();
        config.database_url = format!("sqlite:///{}", db_path.display());
        config.storage_root = storage_root;

        let result = probe_integrity(&config);
        assert!(
            matches!(result, ProbeResult::Ok { name: "integrity" }),
            "fresh startup with a missing DB should not surface IntegrityCorruption: {result:?}"
        );
    }

    #[test]
    fn sqlite_memory_url_with_query_passes() {
        let mut config = default_config();
        config.database_url = "sqlite:///:memory:?cache=shared".into();
        let result = probe_database(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
    }

    #[test]
    fn sqlite_url_with_missing_parent_and_query_fails() {
        let mut config = default_config();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        config.database_url = format!("sqlite:///am-startup-missing-{nonce}/db.sqlite3?mode=rwc");
        let result = probe_database(&config);
        assert!(matches!(result, ProbeResult::Fail(_)));
    }

    #[test]
    fn writable_storage_root_passes() {
        let tmp = std::env::temp_dir().join("am_test_startup_probe");
        let _ = std::fs::create_dir_all(&tmp);
        let mut config = default_config();
        config.storage_root = tmp.clone();
        let result = probe_storage_root(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn nonexistent_storage_root_gets_created() {
        let tmp = std::env::temp_dir().join("am_test_startup_probe_create");
        let _ = std::fs::remove_dir_all(&tmp);
        let mut config = default_config();
        config.storage_root = tmp.clone();
        let result = probe_storage_root(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
        assert!(tmp.is_dir());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn storage_probe_does_not_clobber_existing_probe_file() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("am_test_startup_probe_no_clobber_{nonce}"));
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);

        let existing = tmp.join(".am_startup_probe");
        std::fs::write(&existing, b"do-not-touch").unwrap();

        let mut config = default_config();
        config.storage_root = tmp.clone();
        let result = probe_storage_root(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
        assert_eq!(std::fs::read(&existing).unwrap(), b"do-not-touch");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn format_errors_empty_when_all_pass() {
        let report = StartupReport {
            results: vec![
                ProbeResult::Ok { name: "ok1" },
                ProbeResult::Ok { name: "ok2" },
            ],
        };
        assert!(report.is_ok());
        assert!(report.format_errors().is_empty());
    }

    #[test]
    fn format_errors_shows_failures() {
        let report = StartupReport {
            results: vec![
                ProbeResult::Ok { name: "ok" },
                ProbeResult::Fail(ProbeFailure {
                    name: "port",
                    problem: "Port 8765 is in use".into(),
                    fix: "Use a different port".into(),
                }),
            ],
        };
        assert!(!report.is_ok());
        let errors = report.format_errors();
        assert!(errors.contains("Port 8765 is in use"));
        assert!(errors.contains("Use a different port"));
    }

    #[test]
    fn format_errors_prioritizes_primary_root_cause_before_secondary_findings() {
        let report = StartupReport {
            results: vec![
                ProbeResult::Fail(ProbeFailure {
                    name: "integrity",
                    problem: "SQLite integrity check could not run because the mailbox is busy"
                        .into(),
                    fix: "Stop the running process and retry.".into(),
                }),
                ProbeResult::Fail(ProbeFailure {
                    name: "db-lock",
                    problem: "Database file is exclusively locked by PID 42".into(),
                    fix: "Stop PID 42 or wait for it to finish.".into(),
                }),
            ],
        };

        let errors = report.format_errors();
        let primary_pos = errors
            .find("Primary issue: [db-lock]")
            .expect("primary issue");
        let secondary_pos = errors
            .find("1. [integrity]")
            .expect("secondary integrity finding");
        assert!(
            primary_pos < secondary_pos,
            "expected db-lock to lead the output:\n{errors}"
        );
        assert!(
            errors.contains("Secondary findings: 1 additional failing probe(s)."),
            "expected secondary summary in output:\n{errors}"
        );
    }

    #[test]
    fn probe_failure_display() {
        let fail = ProbeFailure {
            name: "test",
            problem: "something broke".into(),
            fix: "fix it".into(),
        };
        let display = fail.to_string();
        assert!(display.contains("something broke"));
        assert!(display.contains("fix it"));
    }

    #[test]
    fn run_startup_probes_returns_results() {
        let config = default_config();
        let report = run_startup_probes(&config);
        // When an existing Agent Mail server is detected on the default port, the
        // preflight defers db-lock + integrity probes (6 results). Otherwise all 8
        // probes run. Both are valid depending on the machine state.
        assert!(
            report.results.len() == 8 || report.results.len() == 6,
            "expected 6 or 8 probes, got {}",
            report.results.len()
        );
    }

    #[test]
    fn run_stdio_startup_probes_omits_http_only_checks() {
        let config = default_config();
        let report = run_stdio_startup_probes(&config);
        assert_eq!(report.results.len(), 5);
        assert!(!report.results.iter().any(|result| matches!(
            result,
            ProbeResult::Ok {
                name: "port" | "http-path" | "auth"
            } | ProbeResult::Fail(ProbeFailure {
                name: "port" | "http-path" | "auth",
                ..
            })
        )));
        assert!(report.results.iter().any(|result| matches!(
            result,
            ProbeResult::Ok { name: "integrity" }
                | ProbeResult::Fail(ProbeFailure {
                    name: "integrity",
                    ..
                })
        )));
    }

    #[test]
    fn run_http_startup_preflight_probes_omits_port_check() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().expect("listener addr").port();
        let mut config = default_config();
        config.http_port = port;

        let report = run_http_startup_preflight_probes(&config);
        assert!(
            report.is_ok(),
            "preflight report should ignore occupied port"
        );
        assert!(!report.results.iter().any(|result| matches!(
            result,
            ProbeResult::Ok { name: "port" } | ProbeResult::Fail(ProbeFailure { name: "port", .. })
        )));
        assert!(report.results.iter().any(|result| matches!(
            result,
            ProbeResult::Ok { name: "http-path" }
                | ProbeResult::Fail(ProbeFailure {
                    name: "http-path",
                    ..
                })
        )));
    }

    #[test]
    fn jwt_without_jwks_or_secret_fails() {
        let mut config = default_config();
        config.http_jwt_enabled = true;
        config.http_jwt_jwks_url = None;
        config.http_jwt_secret = None;
        let result = probe_auth(&config);
        assert!(matches!(result, ProbeResult::Fail(_)));
    }

    #[test]
    fn jwt_with_secret_passes() {
        let mut config = default_config();
        config.http_jwt_enabled = true;
        config.http_jwt_jwks_url = None;
        config.http_jwt_secret = Some("e2e-secret".into());
        let result = probe_auth(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
    }

    #[test]
    fn jwt_with_jwks_passes() {
        let mut config = default_config();
        config.http_jwt_enabled = true;
        config.http_jwt_jwks_url = Some("http://127.0.0.1:1/jwks".into());
        config.http_jwt_secret = None;
        let result = probe_auth(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
    }

    #[test]
    fn jwt_secret_with_rs256_fails() {
        let mut config = default_config();
        config.http_jwt_enabled = true;
        config.http_jwt_secret = Some("secret".into());
        config.http_jwt_jwks_url = None;
        config.http_jwt_algorithms = vec!["RS256".into()];
        let result = probe_auth(&config);
        assert!(matches!(result, ProbeResult::Fail(_)));
    }

    #[test]
    fn run_http_startup_preflight_probes_skips_db_sensitive_checks_when_agent_mail_listener_exists()
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().expect("listener addr").port();

        let server_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept health request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            loop {
                let mut line = String::new();
                let bytes = reader.read_line(&mut line).expect("read request line");
                if bytes == 0 || line == "\r\n" {
                    break;
                }
            }

            let body = r#"{"status":"alive"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 X-Agent-Mail-Health: 1\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write health response");
            stream.flush().expect("flush health response");
        });

        let mut config = default_config();
        config.http_host = "127.0.0.1".into();
        config.http_port = port;

        let report = run_http_startup_preflight_probes(&config);
        assert!(
            report.is_ok(),
            "preflight report should defer db-sensitive checks while an Agent Mail listener is present"
        );
        assert!(!report.results.iter().any(|result| matches!(
            result,
            ProbeResult::Ok { name: "db-lock" }
                | ProbeResult::Fail(ProbeFailure {
                    name: "db-lock",
                    ..
                })
        )));
        assert!(!report.results.iter().any(|result| matches!(
            result,
            ProbeResult::Ok { name: "integrity" }
                | ProbeResult::Fail(ProbeFailure {
                    name: "integrity",
                    ..
                })
        )));

        server_thread.join().expect("join test server");
    }

    // ──────────────────────────────────────────────────────────────────────
    // Port status detection tests (br-7ri2)
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn port_status_free_is_usable() {
        let status = PortStatus::Free;
        assert!(status.is_usable());
        assert!(!status.is_agent_mail_server());
    }

    #[test]
    fn port_status_agent_mail_is_usable() {
        let status = PortStatus::AgentMailServer;
        assert!(status.is_usable());
        assert!(status.is_agent_mail_server());
    }

    #[test]
    fn port_status_other_process_not_usable() {
        let status = PortStatus::OtherProcess {
            description: "nginx".into(),
        };
        assert!(!status.is_usable());
        assert!(!status.is_agent_mail_server());
    }

    #[test]
    fn port_status_error_not_usable() {
        let status = PortStatus::Error {
            kind: std::io::ErrorKind::PermissionDenied,
            message: "access denied".into(),
        };
        assert!(!status.is_usable());
        assert!(!status.is_agent_mail_server());
    }

    #[test]
    fn check_port_status_free_on_random_port() {
        // Use port 0 to get a random available port, then check a nearby high port
        // that's almost certainly free
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind to random port");
        let port = listener.local_addr().expect("listener addr").port();
        drop(listener);

        // The port we just released should be free
        let status = check_port_status("127.0.0.1", port);
        assert!(
            matches!(status, PortStatus::Free),
            "expected Free, got {status:?}"
        );
    }

    #[test]
    fn check_port_status_in_use_by_other_process() {
        // Bind to a random port and keep it held
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind to random port");
        let port = listener.local_addr().expect("listener addr").port();

        // The port should be detected as in use
        let status = check_port_status("127.0.0.1", port);
        assert!(
            matches!(status, PortStatus::OtherProcess { .. }),
            "expected OtherProcess, got {status:?}"
        );

        // Explicitly drop to release
        drop(listener);
    }

    #[test]
    fn check_port_status_detects_agent_mail_server() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().expect("listener addr").port();

        let server_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept health request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            loop {
                let mut line = String::new();
                let bytes = reader.read_line(&mut line).expect("read request line");
                if bytes == 0 || line == "\r\n" {
                    break;
                }
            }

            let body = r#"{"status":"healthy"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 X-Agent-Mail-Health: 1\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write health response");
            stream.flush().expect("flush health response");
        });

        let status = check_port_status("127.0.0.1", port);
        assert!(
            matches!(status, PortStatus::AgentMailServer),
            "expected AgentMailServer, got {status:?}"
        );

        server_thread.join().expect("join test server");
    }

    #[test]
    fn check_port_status_detects_agent_mail_server_for_wildcard_host() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().expect("listener addr").port();

        let server_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept health request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            loop {
                let mut line = String::new();
                let bytes = reader.read_line(&mut line).expect("read request line");
                if bytes == 0 || line == "\r\n" {
                    break;
                }
            }

            let body = r#"{"status":"healthy"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 X-Agent-Mail-Health: 1\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write health response");
            stream.flush().expect("flush health response");
        });

        let status = check_port_status("0.0.0.0", port);
        assert!(
            matches!(status, PortStatus::AgentMailServer),
            "expected AgentMailServer, got {status:?}"
        );

        server_thread.join().expect("join test server");
    }

    #[test]
    fn check_port_status_accepts_raw_ipv6_loopback_host() {
        let Ok(listener) = TcpListener::bind("[::1]:0") else {
            return;
        };
        let port = listener.local_addr().expect("listener addr").port();
        drop(listener);

        let status = check_port_status("::1", port);
        assert!(
            matches!(status, PortStatus::Free),
            "expected Free, got {status:?}"
        );
    }

    #[test]
    fn health_check_tries_all_resolved_addresses() {
        let dead_listener = TcpListener::bind("127.0.0.1:0").expect("bind dead listener");
        let dead_port = dead_listener
            .local_addr()
            .expect("dead listener addr")
            .port();
        drop(dead_listener);

        let live_listener = TcpListener::bind("127.0.0.1:0").expect("bind live listener");
        let live_port = live_listener
            .local_addr()
            .expect("live listener addr")
            .port();
        let live_addr = std::net::SocketAddr::from(([127, 0, 0, 1], live_port));
        let dead_addr = std::net::SocketAddr::from(([127, 0, 0, 1], dead_port));

        let server_thread = std::thread::spawn(move || {
            let (mut stream, _) = live_listener.accept().expect("accept health request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            loop {
                let mut line = String::new();
                let bytes = reader.read_line(&mut line).expect("read request line");
                if bytes == 0 || line == "\r\n" {
                    break;
                }
            }

            let body = r#"{"status":"healthy"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 X-Agent-Mail-Health: 1\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write health response");
            stream.flush().expect("flush health response");
        });

        assert!(is_agent_mail_health_check_addrs(
            "127.0.0.1",
            live_port,
            [dead_addr, live_addr]
        ));

        server_thread.join().expect("join test server");
    }

    #[test]
    fn normalize_connect_host_for_health_check_preserves_ipv6_loopback() {
        assert_eq!(
            normalize_connect_host_for_health_check("0.0.0.0"),
            std::borrow::Cow::Borrowed("127.0.0.1")
        );
        assert_eq!(
            normalize_connect_host_for_health_check("::"),
            std::borrow::Cow::Borrowed("[::1]")
        );
        assert_eq!(
            normalize_connect_host_for_health_check("[::]"),
            std::borrow::Cow::Borrowed("[::1]")
        );
    }

    #[test]
    fn parse_content_length_ignores_case_and_whitespace() {
        let headers = "Content-Type: application/json\r\ncontent-length: 18\r\n";
        assert_eq!(parse_content_length(headers), Some(18));
    }

    #[test]
    fn agent_mail_health_signature_header_is_detected() {
        let headers = "Content-Type: application/json\r\nX-Agent-Mail-Health: 1\r\n";
        assert!(has_agent_mail_signature(headers));
    }

    #[test]
    fn server_header_alone_is_not_agent_mail_signature() {
        let headers = "Content-Type: application/json\r\nServer: mcp-agent-mail-test\r\n";
        assert!(!has_agent_mail_signature(headers));
    }

    #[test]
    fn generic_ready_json_without_signature_is_not_agent_mail() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().expect("listener addr").port();

        let server_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept health request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            loop {
                let mut line = String::new();
                let bytes = reader.read_line(&mut line).expect("read request line");
                if bytes == 0 || line == "\r\n" {
                    break;
                }
            }

            let body = r#"{"status":"ready"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write health response");
            stream.flush().expect("flush health response");
        });

        let status = check_port_status("127.0.0.1", port);
        assert!(
            matches!(status, PortStatus::OtherProcess { .. }),
            "expected OtherProcess for unsigned generic ready payload, got {status:?}"
        );

        server_thread.join().expect("join test server");
    }

    #[test]
    fn command_line_signature_rejects_unrelated_processes() {
        assert!(!command_line_has_agent_mail_signature(
            "/usr/bin/python worker.py --label=mcp-agent-mail"
        ));
        assert!(!executable_name_has_agent_mail_signature("python3"));
        assert!(!executable_name_has_agent_mail_signature("node"));
    }

    #[test]
    fn command_line_signature_accepts_am_binary() {
        assert!(command_line_has_agent_mail_signature(
            "/usr/local/bin/am serve"
        ));
        assert!(executable_name_has_agent_mail_signature("am"));
        assert!(executable_name_has_agent_mail_signature("agent-mail"));
    }

    #[test]
    fn command_line_signature_accepts_agent_mail_binary_names() {
        assert!(command_line_has_agent_mail_signature(
            "/usr/local/bin/mcp-agent-mail serve"
        ));
        assert!(command_line_has_agent_mail_signature(
            "/opt/tools/mcp_agent_mail daemon"
        ));
        assert!(command_line_has_agent_mail_signature(
            "/home/ubuntu/.cargo/bin/mcp-agent-mail-cli serve-http"
        ));
        assert!(executable_name_has_agent_mail_signature("mcp-agent-mail"));
        assert!(executable_name_has_agent_mail_signature(
            "mcp-agent-mail-cli"
        ));
        assert!(executable_name_has_agent_mail_signature(
            "mcp_agent_mail.exe"
        ));
        assert!(executable_name_has_agent_mail_signature(
            "mcp_agent_mail_cli.exe"
        ));
    }

    #[test]
    fn parse_ss_port_holder_pids_extracts_unique_pids() {
        let output = r#"LISTEN 0 4096 127.0.0.1:8765 0.0.0.0:* users:(("am",pid=1234,fd=7),("helper",pid=5678,fd=8),("am",pid=1234,fd=9))"#;
        assert_eq!(parse_ss_port_holder_pids(output), vec![1234, 5678]);
    }

    #[test]
    fn parse_lsof_port_holder_pids_extracts_unique_pids() {
        let output = "1234\n5678\n1234\n";
        assert_eq!(parse_lsof_port_holder_pids(output), vec![1234, 5678]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_ss_port_holder_pids_for_host_filters_non_matching_hosts() {
        let output = concat!(
            "LISTEN 0 4096 127.0.0.1:8765 0.0.0.0:* users:((\"am\",pid=1234,fd=7))\n",
            "LISTEN 0 4096 127.0.0.2:8765 0.0.0.0:* users:((\"am\",pid=5678,fd=8))\n",
            "LISTEN 0 4096 [::1]:8765 [::]:* users:((\"am\",pid=9999,fd=9))\n"
        );
        assert_eq!(
            parse_ss_port_holder_pids_for_host(output, "127.0.0.1"),
            vec![1234, 9999]
        );
        assert_eq!(
            parse_ss_port_holder_pids_for_host(output, "127.0.0.2"),
            vec![5678, 9999]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_ss_port_holder_pids_for_wildcard_request_matches_specific_conflicting_hosts() {
        let output = concat!(
            "LISTEN 0 4096 127.0.0.1:8765 0.0.0.0:* users:((\"am\",pid=1234,fd=7))\n",
            "LISTEN 0 4096 127.0.0.2:8765 0.0.0.0:* users:((\"am\",pid=5678,fd=8))\n",
            "LISTEN 0 4096 [::1]:8765 [::]:* users:((\"am\",pid=9999,fd=9))\n"
        );
        assert_eq!(
            parse_ss_port_holder_pids_for_host(output, "0.0.0.0"),
            vec![1234, 5678]
        );
        assert_eq!(parse_ss_port_holder_pids_for_host(output, "::"), vec![9999]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_stat_state_extracts_state_after_command_name() {
        assert_eq!(parse_proc_stat_state("123 (am) T 1 2 3 4"), Some('T'));
        assert_eq!(
            parse_proc_stat_state("124 (am worker) t 1 2 3 4"),
            Some('t')
        );
    }

    #[test]
    fn listener_pid_hint_path_sanitizes_host() {
        let path = listener_pid_hint_path("::1", 8765);
        let file_name = path.file_name().expect("file name");
        assert_eq!(file_name.to_string_lossy(), "3a3a31-8765.pid");
    }

    #[test]
    fn listener_pid_hint_path_distinguishes_hosts_that_old_sanitizer_collided() {
        let dotted = listener_pid_hint_path("127.0.0.1", 8765);
        let underscored = listener_pid_hint_path("127_0_0_1", 8765);
        assert_ne!(dotted, underscored);
    }

    #[test]
    fn parse_lsof_port_holder_pids_for_host_filters_non_matching_hosts() {
        let output = concat!(
            "p1234\n",
            "nTCP 127.0.0.1:8765 (LISTEN)\n",
            "p5678\n",
            "nTCP 127.0.0.2:8765 (LISTEN)\n",
            "p9999\n",
            "nTCP *:8765 (LISTEN)\n"
        );
        assert_eq!(
            parse_lsof_port_holder_pids_for_host(output, "127.0.0.1"),
            vec![1234, 9999]
        );
        assert_eq!(
            parse_lsof_port_holder_pids_for_host(output, "127.0.0.2"),
            vec![5678, 9999]
        );
    }

    #[test]
    fn listener_host_matches_request_handles_conflicting_listener_hosts() {
        assert!(listener_host_matches_request("*", "127.0.0.1"));
        assert!(listener_host_matches_request("0.0.0.0", "127.0.0.1"));
        assert!(listener_host_matches_request("::", "127.0.0.1"));
        assert!(listener_host_matches_request(
            "::ffff:127.0.0.1",
            "127.0.0.1"
        ));
        assert!(listener_host_matches_request("127.0.0.1", "localhost"));
        assert!(!listener_host_matches_request("127.0.0.2", "127.0.0.1"));
        assert!(listener_host_matches_request("127.0.0.1", "0.0.0.0"));
        assert!(!listener_host_matches_request("::1", "0.0.0.0"));
        assert!(listener_host_matches_request("::1", "::"));
    }

    #[test]
    fn probe_port_passes_when_free() {
        let mut config = default_config();
        config.http_host = "127.0.0.1".into();
        let mut last_failure: Option<(u16, ProbeResult)> = None;

        // Retry a handful of ephemeral ports to avoid rare race collisions where
        // another process binds the released port between probe setup and check.
        for _ in 0..16 {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind to random port");
            let port = listener.local_addr().expect("get local addr").port();
            drop(listener);

            config.http_port = port;
            let result = probe_port(&config);
            if matches!(result, ProbeResult::Ok { .. }) {
                return;
            }
            last_failure = Some((port, result));
        }

        if let Some((port, result)) = last_failure {
            panic!("expected Ok after retries, last port={port}, got {result:?}");
        }
    }

    #[test]
    fn probe_port_fails_when_other_process() {
        // Hold a port open
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind to random port");
        let port = listener.local_addr().expect("get local addr").port();

        let mut config = default_config();
        config.http_host = "127.0.0.1".into();
        config.http_port = port;

        let result = probe_port(&config);
        assert!(
            matches!(result, ProbeResult::Fail(_)),
            "expected Fail, got {result:?}"
        );

        drop(listener);
    }

    #[test]
    fn probe_port_fails_when_agent_mail_server_running() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().expect("listener addr").port();

        let server_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept health request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            loop {
                let mut line = String::new();
                let bytes = reader.read_line(&mut line).expect("read request line");
                if bytes == 0 || line == "\r\n" {
                    break;
                }
            }

            let body = r#"{"status":"healthy"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Server: mcp-agent-mail-test\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write health response");
            stream.flush().expect("flush health response");
        });

        let mut config = default_config();
        config.http_host = "127.0.0.1".into();
        config.http_port = port;

        let result = probe_port(&config);
        assert!(
            matches!(result, ProbeResult::Fail(_)),
            "expected Fail, got {result:?}"
        );

        server_thread.join().expect("join test server");
    }

    // -----------------------------------------------------------------------
    // probe_integrity tests
    // -----------------------------------------------------------------------

    #[test]
    fn probe_integrity_skipped_when_disabled() {
        let mut config = default_config();
        config.integrity_check_on_startup = false;
        let result = probe_integrity(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
    }

    #[test]
    fn probe_integrity_passes_for_memory_db() {
        let mut config = default_config();
        config.database_url = "sqlite:///:memory:".into();
        let result = probe_integrity(&config);
        assert!(matches!(result, ProbeResult::Ok { .. }));
    }

    #[test]
    fn probe_integrity_recovers_corrupt_db_with_archive() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("corrupt.db");
        let storage_root = dir.path().join("storage");

        // Write corrupt data.
        std::fs::write(&db_path, b"not-a-sqlite-db").unwrap();

        // Create archive with a project.
        let proj = storage_root.join("projects").join("test");
        let agent_dir = proj.join("agents").join("RedFox");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"RedFox","role":"Tester","model":"test","registered_ts":"2026-01-01T00:00:00"}"#,
        )
        .unwrap();

        let mut config = default_config();
        config.database_url = format!("sqlite:///{}", db_path.display());
        config.storage_root = storage_root;

        let result = probe_integrity(&config);
        assert!(
            matches!(result, ProbeResult::Ok { .. }),
            "probe_integrity should auto-recover corrupt DB; got: {result:?}"
        );
    }

    #[test]
    fn probe_integrity_recovers_corrupt_db_without_archive() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("corrupt_no_archive.db");

        // Write corrupt data.
        std::fs::write(&db_path, b"not-a-sqlite-db").unwrap();

        let mut config = default_config();
        config.database_url = format!("sqlite:///{}", db_path.display());
        // storage_root is default (nonexistent) so no archive is available.
        config.storage_root = dir.path().join("no-storage");

        let result = probe_integrity(&config);
        assert!(
            matches!(result, ProbeResult::Ok { .. }),
            "probe_integrity should reinit from scratch when no archive; got: {result:?}"
        );
    }

    #[test]
    fn probe_integrity_passes_healthy_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("healthy.db");

        // Create a valid SQLite DB.
        let conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw("CREATE TABLE t(x TEXT)").unwrap();
        drop(conn);

        let mut config = default_config();
        config.database_url = format!("sqlite:///{}", db_path.display());

        let result = probe_integrity(&config);
        assert!(
            matches!(result, ProbeResult::Ok { .. }),
            "healthy DB should pass probe_integrity; got: {result:?}"
        );
    }

    #[test]
    fn probe_integrity_reports_busy_when_mailbox_activity_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("busy.db");
        let conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw("CREATE TABLE t(x TEXT)").unwrap();
        drop(conn);

        let _shared_lock = acquire_mailbox_activity_lock_for_database_url(
            &format!("sqlite:///{}", db_path.display()),
            MailboxActivityLockMode::Shared,
        )
        .expect("acquire shared mailbox activity lock");

        let mut config = default_config();
        config.database_url = format!("sqlite:///{}", db_path.display());

        let result = probe_integrity(&config);
        assert!(
            matches!(result, ProbeResult::Fail(ProbeFailure { name: "integrity", ref problem, .. }) if problem.contains("busy")),
            "probe_integrity should report mailbox activity contention as busy: {result:?}"
        );
    }

    #[test]
    fn probe_integrity_recovers_when_archive_is_ahead_of_healthy_db() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("healthy_but_stale.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project","created_at":0}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(
            messages_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-03-22T12:00:00Z"
}
---

first body
"#,
        )
        .unwrap();

        mcp_agent_mail_db::reconstruct_from_archive(&db_path, &storage_root)
            .expect("seed initial reconstructed db");

        std::fs::write(
            messages_dir.join("2026-03-22T12-05-00Z__second__2.md"),
            r#"---json
{
  "id": 2,
  "from": "Alice",
  "to": ["Carol"],
  "subject": "Archive only",
  "importance": "urgent",
  "created_ts": "2026-03-22T12:05:00Z"
}
---

second body
"#,
        )
        .unwrap();

        let mut config = default_config();
        config.database_url = format!("sqlite:///{}", db_path.display());
        config.storage_root = storage_root;

        let result = probe_integrity(&config);
        assert!(
            matches!(result, ProbeResult::Ok { .. }),
            "archive-ahead healthy db should be auto-reconciled; got: {result:?}"
        );

        let conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.to_string_lossy().as_ref()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS count, COALESCE(MAX(id), 0) AS max_id FROM messages",
                &[],
            )
            .unwrap();
        assert_eq!(rows[0].get_named::<i64>("count").expect("message count"), 2);
        assert_eq!(rows[0].get_named::<i64>("max_id").expect("max id"), 2);
    }

    #[test]
    fn integrity_busy_probe_failure_mentions_busy_database() {
        let mut config = default_config();
        config.database_url = "sqlite:///tmp/test-busy.sqlite3".into();

        let result = integrity_busy_probe_failure(&config, "database is busy");
        assert!(
            matches!(result, ProbeResult::Fail(ProbeFailure { name: "integrity", ref problem, ref fix }) if problem.contains("busy") && fix.contains("Stop any running")),
            "busy integrity failures should point users at live lock holders; got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // probe_db_lock tests
    // -----------------------------------------------------------------------

    #[test]
    fn probe_db_lock_passes_when_available() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("available.db");
        std::fs::write(&db_path, b"data").unwrap();

        let mut config = default_config();
        config.database_url = format!("sqlite:///{}", db_path.display());

        let result = probe_db_lock(&config);
        assert!(matches!(result, ProbeResult::Ok { name: "db-lock" }));
    }

    #[test]
    fn probe_db_lock_passes_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("missing.db");

        let mut config = default_config();
        config.database_url = format!("sqlite:///{}", db_path.display());

        let result = probe_db_lock(&config);
        assert!(matches!(result, ProbeResult::Ok { name: "db-lock" }));
    }

    #[test]
    fn probe_db_lock_fails_when_locked() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("locked.db");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&db_path)
            .unwrap();
        file.lock_exclusive().unwrap();

        let mut config = default_config();
        config.database_url = format!("sqlite:///{}", db_path.display());

        let result = probe_db_lock(&config);
        assert!(matches!(result, ProbeResult::Fail(f) if f.name == "db-lock"));

        file.unlock().unwrap();
    }

    // ── Recovery elapsed formatting tests (br-rqv3i.7) ──────────────────

    #[test]
    fn format_recovery_elapsed_seconds() {
        assert_eq!(super::format_recovery_elapsed(0), "0s");
        assert_eq!(super::format_recovery_elapsed(45), "45s");
        assert_eq!(super::format_recovery_elapsed(59), "59s");
    }

    #[test]
    fn format_recovery_elapsed_minutes() {
        assert_eq!(super::format_recovery_elapsed(60), "1m");
        assert_eq!(super::format_recovery_elapsed(90), "1m 30s");
        assert_eq!(super::format_recovery_elapsed(155), "2m 35s");
    }

    #[test]
    fn format_recovery_elapsed_hours() {
        assert_eq!(super::format_recovery_elapsed(3600), "1h 0m");
        assert_eq!(super::format_recovery_elapsed(4320), "1h 12m");
        assert_eq!(super::format_recovery_elapsed(7200), "2h 0m");
    }
}
