//! Startup verification probes for `AgentMailTUI`.
//!
//! Each probe checks one aspect of the runtime environment and returns
//! a [`ProbeResult`] with a human-friendly error message and remediation
//! hints when something is wrong.

use mcp_agent_mail_core::{
    Config,
    disk::{is_sqlite_memory_database_url, sqlite_file_path_from_database_url},
};
use mcp_agent_mail_db::DbPoolConfig;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

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
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

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

    // Step 1: Try to bind to the port
    match TcpListener::bind(&addr) {
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
        PortStatus::AgentMailServer
    } else {
        PortStatus::OtherProcess {
            description: format!("Unknown process listening on {addr}"),
        }
    }
}

/// Attempt to connect to a port and verify it's an Agent Mail server via health check.
///
/// Sends a minimal HTTP GET request to `/health` and checks for a valid response.
fn is_agent_mail_health_check(host: &str, port: u16) -> bool {
    let addr = format!("{host}:{port}");

    // Try to connect with a short timeout
    let Ok(stream) = TcpStream::connect_timeout(
        &addr.parse().unwrap_or_else(|_| {
            // Fallback for invalid addresses
            std::net::SocketAddr::from(([127, 0, 0, 1], port))
        }),
        HEALTH_CHECK_TIMEOUT,
    ) else {
        return false;
    };

    // Set read/write timeouts
    let _ = stream.set_read_timeout(Some(HEALTH_CHECK_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HEALTH_CHECK_TIMEOUT));

    // Send HTTP GET /health request
    let request = format!(
        "GET /health HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Connection: close\r\n\
         User-Agent: mcp-agent-mail-startup-check\r\n\
         \r\n"
    );

    let mut stream = stream;
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    // Read response
    let mut reader = BufReader::new(&stream);
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

    // 2xx responses indicate a healthy server
    if !(200..300).contains(&status_code) {
        return false;
    }

    // Read headers and body to look for Agent Mail signature
    let mut headers = String::new();
    let mut header_bytes_read = 0;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if line.trim().is_empty() {
                    break; // End of headers
                }
                header_bytes_read += n;
                if header_bytes_read > 8192 {
                    break;
                }
                headers.push_str(&line);
            }
        }
    }

    // Read body (limited to first 4KB for safety)
    let mut body_bytes = Vec::new();
    // Use take(4096) to prevent OOM/hangs, and read_to_end to ensure we get
    // the full response body (until EOF or limit) even if fragmented.
    // The request sent "Connection: close", so the server should close the socket.
    if reader.take(4096).read_to_end(&mut body_bytes).is_ok() {
        // Continue
    }
    let body = String::from_utf8_lossy(&body_bytes).to_string();

    // Check for Agent Mail signatures in headers or body.
    // Currently the server does NOT set a Server HTTP header, so detection
    // relies on the JSON body from /health returning {"status":"ready"}.
    let combined = format!("{headers}{body}").to_lowercase();

    // Primary: body contains {"status":"ready"} (or legacy "healthy") / {"ok":true}.
    // Defensive: headers/body contain "mcp-agent-mail" (would match if a Server
    // header is added in the future).
    combined.contains("mcp-agent-mail")
        || combined.contains("mcp_agent_mail")
        || combined.contains("agent-mail")
        || (combined.contains("\"status\"")
            && (combined.contains("ready") || combined.contains("healthy")))
        || combined.contains("\"ok\":true")
        || combined.contains("\"ok\": true")
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

    /// Format a human-readable error block for terminal output.
    #[must_use]
    pub fn format_errors(&self) -> String {
        use fmt::Write;
        let failures = self.failures();
        if failures.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        out.push_str("\n  Startup failed — the following checks did not pass:\n\n");
        for (i, fail) in failures.iter().enumerate() {
            let _ = writeln!(out, "  {}. [{}] {}", i + 1, fail.name, fail.problem);
            let _ = writeln!(out, "     Fix: {}\n", fail.fix);
        }
        out
    }
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

    // Check writability by touching a temp file
    let probe_path = root.join(".am_startup_probe");
    match std::fs::write(&probe_path, b"ok") {
        Ok(()) => {
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
        let Some(path) = sqlite_file_path_from_database_url(url) else {
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

    let pool_config = DbPoolConfig {
        database_url: config.database_url.clone(),
        min_connections: 1,
        max_connections: 1,
        run_migrations: false,
        warmup_connections: 0,
        ..DbPoolConfig::default()
    };

    let pool = match mcp_agent_mail_db::DbPool::new(&pool_config) {
        Ok(p) => p,
        Err(e) => {
            let err_str = e.to_string();
            // If pool creation itself failed due to corruption, attempt
            // file-level recovery before giving up.
            if mcp_agent_mail_db::is_corruption_error_message(&err_str) {
                return attempt_probe_recovery(config);
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
            tracing::warn!(
                error = %err_str,
                "startup integrity check failed; attempting automatic recovery"
            );
            attempt_probe_recovery(config)
        }
    }
}

/// Attempt file-level recovery when the integrity probe detects corruption.
///
/// Uses the archive-aware recovery path which tries, in order:
/// 1. Restore from a healthy `.bak` / `.backup-*` / `.recovery*` backup
/// 2. Reconstruct from the Git archive (recovers messages + agents)
/// 3. Reinitialize an empty database (last resort)
#[allow(dead_code)]
fn attempt_probe_recovery(config: &Config) -> ProbeResult {
    let Some(db_path) = sqlite_file_path_from_database_url(&config.database_url) else {
        return ProbeResult::Fail(ProbeFailure {
            name: "integrity",
            problem: "Cannot determine database file path for recovery".into(),
            fix: "Check DATABASE_URL format".into(),
        });
    };

    let storage_root = std::path::Path::new(&config.storage_root);

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
                "Run `am doctor repair` to attempt manual recovery, or \
                 `am doctor reconstruct` to rebuild the database from the Git archive. \
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
                fix: "Set HTTP_JWT_SECRET for HS256/HS384/HS512, or set HTTP_JWT_JWKS_URL for JWKS-backed verification"
                    .into(),
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
                if parts.len() >= 5 {
                    if let Ok(soft) = parts[3].parse::<u64>() {
                        return Some(soft);
                    }
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
/// See: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/18
fn probe_fd_limit(_config: &Config) -> ProbeResult {
    if let Some(soft_limit) = read_fd_soft_limit() {
        if soft_limit < MIN_RECOMMENDED_NOFILE {
            tracing::warn!(
                soft_limit,
                recommended = MIN_RECOMMENDED_NOFILE,
                "file descriptor limit (ulimit -n) is low; may cause failures under burst load"
            );
            return ProbeResult::Fail(ProbeFailure {
                name: "fd_limit",
                problem: format!(
                    "File descriptor limit (ulimit -n) is {soft_limit}, below recommended minimum of {MIN_RECOMMENDED_NOFILE}"
                ),
                fix: format!(
                    "Increase the limit: `ulimit -n {MIN_RECOMMENDED_NOFILE}` or set in /etc/security/limits.conf"
                ),
            });
        }
        tracing::debug!(soft_limit, "file descriptor limit check passed");
    }
    ProbeResult::Ok { name: "fd_limit" }
}

/// Run all startup probes and return a report.
///
/// The probes are ordered from fastest to slowest, and all probes run
/// even if earlier ones fail (so the user sees all problems at once).
#[must_use]
pub fn run_startup_probes(config: &Config) -> StartupReport {
    let results = vec![
        probe_http_path(config),
        probe_auth(config),
        probe_database(config),
        probe_storage_root(config),
        probe_port(config),
        probe_fd_limit(config),
    ];
    StartupReport { results }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
    fn format_errors_empty_when_all_pass() {
        let report = StartupReport {
            results: vec![
                ProbeResult::Ok { name: "test1" },
                ProbeResult::Ok { name: "test2" },
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
        // Should have 6 critical probes (integrity now runs in readiness_check;
        // consistency is background/advisory).
        assert_eq!(report.results.len(), 6);
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
        let port = listener.local_addr().expect("get local addr").port();
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
        let port = listener.local_addr().expect("get local addr").port();

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

        let status = check_port_status("127.0.0.1", port);
        assert!(
            matches!(status, PortStatus::AgentMailServer),
            "expected AgentMailServer, got {status:?}"
        );

        server_thread.join().expect("join test server");
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
}
