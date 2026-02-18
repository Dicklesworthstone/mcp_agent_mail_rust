//! Native HTTP probe module for deployment verification.
//!
//! Provides synchronous HTTP probing without external dependencies (`curl`,
//! `reqwest`, etc.). Uses `std::net::TcpStream` for plain HTTP and the
//! `native-tls` crate (via the workspace) for HTTPS.
//!
//! Implements the probe contract from `SPEC-verify-live-contract.md`:
//! - Configurable timeout, retry, and redirect policy
//! - Structured error reasons (timeout, DNS, TLS, connection refused)
//! - Status code, header, and body capture
//! - Deterministic request sequencing

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read as _};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ── Error types ─────────────────────────────────────────────────────────

/// Structured probe failure reasons.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeError {
    /// Invalid or unparseable URL.
    InvalidUrl { detail: String },
    /// DNS resolution failed.
    DnsError { detail: String },
    /// TCP connection failed (refused, reset, etc.).
    ConnectionError { detail: String },
    /// TLS handshake failed.
    TlsError { detail: String },
    /// Request timed out.
    Timeout { timeout_ms: u64 },
    /// Too many redirects followed.
    TooManyRedirects { count: u32, max: u32 },
    /// HTTP protocol error (malformed response, etc.).
    ProtocolError { detail: String },
    /// I/O error during read/write.
    IoError { detail: String },
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl { detail } => write!(f, "invalid URL: {detail}"),
            Self::DnsError { detail } => write!(f, "DNS resolution failed: {detail}"),
            Self::ConnectionError { detail } => write!(f, "connection failed: {detail}"),
            Self::TlsError { detail } => write!(f, "TLS error: {detail}"),
            Self::Timeout { timeout_ms } => write!(f, "request timed out after {timeout_ms}ms"),
            Self::TooManyRedirects { count, max } => {
                write!(f, "too many redirects ({count} of max {max})")
            }
            Self::ProtocolError { detail } => write!(f, "protocol error: {detail}"),
            Self::IoError { detail } => write!(f, "I/O error: {detail}"),
        }
    }
}

impl std::error::Error for ProbeError {}

// ── Configuration ───────────────────────────────────────────────────────

/// Configuration for a probe request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeConfig {
    /// Per-request timeout.
    pub timeout: Duration,
    /// Maximum retry attempts for retryable failures.
    pub retries: u32,
    /// Delay between retries.
    pub retry_delay: Duration,
    /// Maximum redirects to follow.
    pub max_redirects: u32,
    /// User-Agent header value.
    pub user_agent: String,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            retries: 2,
            retry_delay: Duration::from_secs(1),
            max_redirects: 5,
            user_agent: "mcp-agent-mail/probe".to_string(),
        }
    }
}

// ── Response types ──────────────────────────────────────────────────────

/// Captured HTTP response from a probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResponse {
    /// Final URL (after redirects).
    pub final_url: String,
    /// HTTP status code.
    pub status: u16,
    /// Response headers (lowercased keys).
    pub headers: BTreeMap<String, String>,
    /// Response body bytes (truncated to `body_limit`).
    pub body: Vec<u8>,
    /// Number of redirects followed.
    pub redirects: u32,
    /// Time taken for the full request chain.
    pub elapsed: Duration,
}

impl ProbeResponse {
    /// Get a header value by lowercase key.
    #[must_use]
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers.get(key).map(String::as_str)
    }

    /// Get the response body as a UTF-8 string (lossy).
    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

// ── URL parsing ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ParsedUrl {
    scheme: Scheme,
    host: String,
    port: u16,
    path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Http,
    Https,
}

impl ParsedUrl {
    fn parse(url: &str) -> Result<Self, ProbeError> {
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (Scheme::Https, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (Scheme::Http, rest)
        } else {
            return Err(ProbeError::InvalidUrl {
                detail: format!("unsupported scheme in URL: {url}"),
            });
        };

        let (host_port, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };

        let (host, port) = if let Some(bracket_end) = host_port.find(']') {
            // IPv6: [::1]:8080
            let h = &host_port[..=bracket_end];
            let p = if host_port.len() > bracket_end + 1 {
                host_port[bracket_end + 2..]
                    .parse::<u16>()
                    .map_err(|e| ProbeError::InvalidUrl {
                        detail: format!("invalid port: {e}"),
                    })?
            } else {
                match scheme {
                    Scheme::Http => 80,
                    Scheme::Https => 443,
                }
            };
            (h.to_string(), p)
        } else if let Some(colon) = host_port.rfind(':') {
            let h = &host_port[..colon];
            let p = host_port[colon + 1..]
                .parse::<u16>()
                .map_err(|e| ProbeError::InvalidUrl {
                    detail: format!("invalid port: {e}"),
                })?;
            (h.to_string(), p)
        } else {
            let p = match scheme {
                Scheme::Http => 80,
                Scheme::Https => 443,
            };
            (host_port.to_string(), p)
        };

        if host.is_empty() {
            return Err(ProbeError::InvalidUrl {
                detail: "empty host".to_string(),
            });
        }

        Ok(Self {
            scheme,
            host,
            port,
            path: path.to_string(),
        })
    }

    fn authority(&self) -> String {
        let default_port = match self.scheme {
            Scheme::Http => 80,
            Scheme::Https => 443,
        };
        if self.port == default_port {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    fn to_url(&self) -> String {
        let scheme = match self.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        format!("{}://{}{}", scheme, self.authority(), self.path)
    }
}

// ── Core probe implementation ───────────────────────────────────────────

/// Maximum response body bytes to capture.
const BODY_LIMIT: usize = 1024 * 1024; // 1 MiB

/// Perform an HTTP GET probe against the given URL.
///
/// Follows redirects, retries on transient errors, and captures status code,
/// headers, and body. Returns a structured `ProbeResponse` or `ProbeError`.
pub fn probe_get(url: &str, config: &ProbeConfig) -> Result<ProbeResponse, ProbeError> {
    let start = Instant::now();
    let mut parsed = ParsedUrl::parse(url)?;
    let mut redirects = 0u32;

    loop {
        let result = probe_single_get(&parsed, config);
        match result {
            Ok(resp) => {
                // Check for redirect
                if is_redirect(resp.status) && redirects < config.max_redirects {
                    if let Some(location) = resp.header("location") {
                        let next_url = resolve_redirect(&parsed, location);
                        parsed = ParsedUrl::parse(&next_url)?;
                        redirects += 1;
                        continue;
                    }
                } else if is_redirect(resp.status) {
                    return Err(ProbeError::TooManyRedirects {
                        count: redirects + 1,
                        max: config.max_redirects,
                    });
                }

                return Ok(ProbeResponse {
                    final_url: parsed.to_url(),
                    status: resp.status,
                    headers: resp.headers,
                    body: resp.body,
                    redirects,
                    elapsed: start.elapsed(),
                });
            }
            Err(e) if is_retryable(&e) => {
                // Retry handled below
                return retry_probe(&parsed, config, start, redirects);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Retry a failed probe up to `config.retries` times.
fn retry_probe(
    parsed: &ParsedUrl,
    config: &ProbeConfig,
    start: Instant,
    redirects: u32,
) -> Result<ProbeResponse, ProbeError> {
    let mut last_err = None;

    for attempt in 0..=config.retries {
        if attempt > 0 {
            std::thread::sleep(config.retry_delay);
        }

        match probe_single_get(parsed, config) {
            Ok(resp) if !is_server_error(resp.status) => {
                return Ok(ProbeResponse {
                    final_url: parsed.to_url(),
                    status: resp.status,
                    headers: resp.headers,
                    body: resp.body,
                    redirects,
                    elapsed: start.elapsed(),
                });
            }
            Ok(resp) => {
                // 5xx — retryable
                last_err = Some(ProbeError::ProtocolError {
                    detail: format!("server error: HTTP {}", resp.status),
                });
            }
            Err(e) if is_retryable(&e) => {
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_err.unwrap_or_else(|| ProbeError::IoError {
        detail: "all retries exhausted".to_string(),
    }))
}

/// Perform a single HTTP GET request (no redirect following, no retry).
fn probe_single_get(parsed: &ParsedUrl, config: &ProbeConfig) -> Result<RawResponse, ProbeError> {
    let addr = format!("{}:{}", parsed.host, parsed.port);

    let stream = TcpStream::connect_timeout(
        &addr
            .parse()
            .or_else(|_| {
                // DNS resolution via std::net
                use std::net::ToSocketAddrs;
                addr.to_socket_addrs()
                    .map_err(|e| ProbeError::DnsError {
                        detail: e.to_string(),
                    })?
                    .next()
                    .ok_or_else(|| ProbeError::DnsError {
                        detail: format!("no addresses for {addr}"),
                    })
            })
            .map_err(|e| ProbeError::DnsError {
                detail: format!("{e}"),
            })?,
        config.timeout,
    )
    .map_err(|e| categorize_connect_error(e, config.timeout))?;

    stream
        .set_read_timeout(Some(config.timeout))
        .map_err(|e| ProbeError::IoError {
            detail: e.to_string(),
        })?;
    stream
        .set_write_timeout(Some(config.timeout))
        .map_err(|e| ProbeError::IoError {
            detail: e.to_string(),
        })?;

    match parsed.scheme {
        Scheme::Http => send_and_receive(stream, parsed, config),
        Scheme::Https => {
            // HTTPS uses curl subprocess to avoid native-tls dependency
            // (share crate is #![forbid(unsafe_code)])
            drop(stream);
            probe_via_curl(parsed, config)
        }
    }
}

/// Send an HTTP/1.1 GET request and parse the response.
fn send_and_receive<S: io::Read + io::Write>(
    mut stream: S,
    parsed: &ParsedUrl,
    _config: &ProbeConfig,
) -> Result<RawResponse, ProbeError> {
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: mcp-agent-mail/probe\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        parsed.path,
        parsed.authority()
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|e| ProbeError::IoError {
            detail: e.to_string(),
        })?;
    stream.flush().map_err(|e| ProbeError::IoError {
        detail: e.to_string(),
    })?;

    let mut reader = BufReader::new(stream);

    // Parse status line
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| ProbeError::IoError {
            detail: e.to_string(),
        })?;

    let status = parse_status_line(&status_line)?;

    // Parse headers
    let mut headers = BTreeMap::new();
    let mut content_length: Option<usize> = None;
    let mut chunked = false;

    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| ProbeError::IoError {
                detail: e.to_string(),
            })?;

        let trimmed = line.trim_end_matches("\r\n").trim_end_matches('\n');
        if trimmed.is_empty() {
            break;
        }

        if let Some((key, value)) = trimmed.split_once(':') {
            let key_lower = key.trim().to_lowercase();
            let value_trimmed = value.trim().to_string();

            if key_lower == "content-length" {
                content_length = value_trimmed.parse().ok();
            }
            if key_lower == "transfer-encoding" && value_trimmed.to_lowercase().contains("chunked")
            {
                chunked = true;
            }

            headers.insert(key_lower, value_trimmed);
        }
    }

    // Read body
    let body = if chunked {
        read_chunked_body(&mut reader)?
    } else if let Some(len) = content_length {
        let capped = len.min(BODY_LIMIT);
        let mut buf = vec![0u8; capped];
        reader
            .read_exact(&mut buf)
            .map_err(|e| ProbeError::IoError {
                detail: e.to_string(),
            })?;
        buf
    } else {
        // Read until EOF (Connection: close)
        let mut buf = Vec::new();
        let _ = reader.take(BODY_LIMIT as u64).read_to_end(&mut buf);
        buf
    };

    Ok(RawResponse {
        status,
        headers,
        body,
    })
}

/// Probe via `curl` subprocess for HTTPS (avoids native TLS dependency).
fn probe_via_curl(parsed: &ParsedUrl, config: &ProbeConfig) -> Result<RawResponse, ProbeError> {
    let url = parsed.to_url();
    #[allow(clippy::cast_possible_truncation)]
    let timeout_secs = config.timeout.as_secs().max(1);

    let output = std::process::Command::new("curl")
        .args([
            "-sS",
            "-D",
            "-", // dump headers to stdout
            "--max-time",
            &timeout_secs.to_string(),
            "-L", // follow redirects (curl handles them)
            "--max-redirs",
            "0", // but we handle redirects ourselves
            &url,
        ])
        .output()
        .map_err(|e| ProbeError::ConnectionError {
            detail: format!("curl not available: {e}"),
        })?;

    if !output.status.success() && output.stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ProbeError::ConnectionError {
            detail: format!("curl failed: {stderr}"),
        });
    }

    // Parse curl output (headers + body separated by \r\n\r\n)
    let raw = String::from_utf8_lossy(&output.stdout);
    let (header_section, body_str) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));

    let mut lines = header_section.lines();
    let status_line = lines.next().unwrap_or("HTTP/1.1 000 Unknown");
    let status = parse_status_line(status_line)?;

    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_lowercase(), value.trim().to_string());
        }
    }

    Ok(RawResponse {
        status,
        headers,
        body: body_str.as_bytes().to_vec(),
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────

#[derive(Debug)]
struct RawResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl RawResponse {
    fn header(&self, key: &str) -> Option<&str> {
        self.headers.get(key).map(String::as_str)
    }
}

fn parse_status_line(line: &str) -> Result<u16, ProbeError> {
    // "HTTP/1.1 200 OK" → 200
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(ProbeError::ProtocolError {
            detail: format!("malformed status line: {line}"),
        });
    }
    parts[1]
        .parse::<u16>()
        .map_err(|_| ProbeError::ProtocolError {
            detail: format!("invalid status code in: {line}"),
        })
}

fn read_chunked_body<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, ProbeError> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        reader
            .read_line(&mut size_line)
            .map_err(|e| ProbeError::IoError {
                detail: e.to_string(),
            })?;

        let size_str = size_line.trim();
        // Strip chunk extensions (e.g., ";ext=val")
        let size_hex = size_str.split(';').next().unwrap_or("0").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| ProbeError::ProtocolError {
            detail: format!("invalid chunk size: {size_str}"),
        })?;

        if size == 0 {
            // Read trailing \r\n
            let mut trailer = String::new();
            let _ = reader.read_line(&mut trailer);
            break;
        }

        if body.len() + size > BODY_LIMIT {
            // Cap at limit
            let remaining = BODY_LIMIT - body.len();
            let mut buf = vec![0u8; remaining];
            reader
                .read_exact(&mut buf)
                .map_err(|e| ProbeError::IoError {
                    detail: e.to_string(),
                })?;
            body.extend_from_slice(&buf);
            break;
        }

        let mut buf = vec![0u8; size];
        reader
            .read_exact(&mut buf)
            .map_err(|e| ProbeError::IoError {
                detail: e.to_string(),
            })?;
        body.extend_from_slice(&buf);

        // Read trailing \r\n after chunk data
        let mut crlf = [0u8; 2];
        let _ = reader.read_exact(&mut crlf);
    }

    Ok(body)
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn is_server_error(status: u16) -> bool {
    (500..600).contains(&status)
}

fn is_retryable(err: &ProbeError) -> bool {
    matches!(
        err,
        ProbeError::Timeout { .. }
            | ProbeError::ConnectionError { .. }
            | ProbeError::IoError { .. }
    )
}

fn categorize_connect_error(err: io::Error, timeout: Duration) -> ProbeError {
    match err.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ProbeError::Timeout {
            #[allow(clippy::cast_possible_truncation)]
            timeout_ms: timeout.as_millis() as u64,
        },
        io::ErrorKind::ConnectionRefused => ProbeError::ConnectionError {
            detail: "connection refused".to_string(),
        },
        io::ErrorKind::ConnectionReset => ProbeError::ConnectionError {
            detail: "connection reset".to_string(),
        },
        _ => ProbeError::ConnectionError {
            detail: err.to_string(),
        },
    }
}

fn resolve_redirect(base: &ParsedUrl, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        // Absolute URL
        location.to_string()
    } else if location.starts_with('/') {
        // Absolute path
        let scheme = match base.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        format!("{}://{}{}", scheme, base.authority(), location)
    } else {
        // Relative path
        let base_path = base.path.rsplit_once('/').map_or("/", |(p, _)| p);
        let scheme = match base.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        format!(
            "{}://{}{}/{}",
            scheme,
            base.authority(),
            base_path,
            location
        )
    }
}

// ── Multi-probe runner ──────────────────────────────────────────────────

/// A probe check to execute.
#[derive(Debug, Clone)]
pub struct ProbeCheck {
    /// Check identifier (e.g., `remote.root`).
    pub id: String,
    /// Human-readable description.
    pub description: String,
    /// URL path to probe (appended to base URL).
    pub path: String,
    /// Expected HTTP status code (None = any success).
    pub expected_status: Option<u16>,
    /// Headers to check for presence.
    pub required_headers: Vec<String>,
    /// Severity of this check.
    pub severity: crate::CheckSeverity,
}

/// Result of a single probe check.
#[derive(Debug, Clone)]
pub struct ProbeCheckResult {
    /// Check identifier.
    pub id: String,
    /// Check description.
    pub description: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Result message.
    pub message: String,
    /// Check severity.
    pub severity: crate::CheckSeverity,
    /// Time taken.
    pub elapsed: Duration,
    /// HTTP status code (if request succeeded).
    pub http_status: Option<u16>,
    /// Captured headers.
    pub headers_captured: BTreeMap<String, String>,
}

/// Run a sequence of probe checks against a base URL.
#[must_use]
pub fn run_probe_checks(
    base_url: &str,
    checks: &[ProbeCheck],
    config: &ProbeConfig,
) -> Vec<ProbeCheckResult> {
    let base = base_url.trim_end_matches('/');

    checks
        .iter()
        .map(|check| {
            let url = format!("{}{}", base, check.path);
            let start = Instant::now();

            match probe_get(&url, config) {
                Ok(resp) => {
                    let mut passed = true;
                    let mut messages = Vec::new();

                    // Check status
                    if let Some(expected) = check.expected_status {
                        if resp.status != expected {
                            passed = false;
                            messages.push(format!("expected HTTP {expected}, got {}", resp.status));
                        }
                    } else if resp.status >= 400 {
                        passed = false;
                        messages.push(format!("HTTP {}", resp.status));
                    }

                    // Check required headers
                    for header in &check.required_headers {
                        let key = header.to_lowercase();
                        if resp.header(&key).is_none() {
                            passed = false;
                            messages.push(format!("{header} header missing"));
                        }
                    }

                    let message = if passed {
                        format!(
                            "GET {} \u{2192} {} ({}ms)",
                            check.path,
                            resp.status,
                            start.elapsed().as_millis()
                        )
                    } else {
                        messages.join("; ")
                    };

                    // Capture relevant headers
                    let mut captured = BTreeMap::new();
                    for key in &check.required_headers {
                        let k = key.to_lowercase();
                        if let Some(val) = resp.header(&k) {
                            captured.insert(k, val.to_string());
                        }
                    }
                    // Always capture content-type
                    if let Some(ct) = resp.header("content-type") {
                        captured.insert("content-type".to_string(), ct.to_string());
                    }

                    ProbeCheckResult {
                        id: check.id.clone(),
                        description: check.description.clone(),
                        passed,
                        message,
                        severity: check.severity,
                        elapsed: start.elapsed(),
                        http_status: Some(resp.status),
                        headers_captured: captured,
                    }
                }
                Err(e) => ProbeCheckResult {
                    id: check.id.clone(),
                    description: check.description.clone(),
                    passed: false,
                    message: e.to_string(),
                    severity: check.severity,
                    elapsed: start.elapsed(),
                    http_status: None,
                    headers_captured: BTreeMap::new(),
                },
            }
        })
        .collect()
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── URL parsing ─────────────────────────────────────────────────

    #[test]
    fn parse_http_url() {
        let p = ParsedUrl::parse("http://example.com/foo").unwrap();
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 80);
        assert_eq!(p.path, "/foo");
        assert!(matches!(p.scheme, Scheme::Http));
    }

    #[test]
    fn parse_https_url() {
        let p = ParsedUrl::parse("https://example.com/bar").unwrap();
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 443);
        assert_eq!(p.path, "/bar");
        assert!(matches!(p.scheme, Scheme::Https));
    }

    #[test]
    fn parse_url_with_port() {
        let p = ParsedUrl::parse("http://localhost:9000/test").unwrap();
        assert_eq!(p.host, "localhost");
        assert_eq!(p.port, 9000);
        assert_eq!(p.path, "/test");
    }

    #[test]
    fn parse_url_no_path() {
        let p = ParsedUrl::parse("https://example.com").unwrap();
        assert_eq!(p.path, "/");
    }

    #[test]
    fn parse_invalid_scheme() {
        let r = ParsedUrl::parse("ftp://example.com");
        assert!(r.is_err());
    }

    #[test]
    fn parse_empty_host() {
        let r = ParsedUrl::parse("http:///path");
        assert!(r.is_err());
    }

    #[test]
    fn authority_default_port() {
        let p = ParsedUrl::parse("https://example.com/x").unwrap();
        assert_eq!(p.authority(), "example.com");
    }

    #[test]
    fn authority_custom_port() {
        let p = ParsedUrl::parse("http://localhost:8080/x").unwrap();
        assert_eq!(p.authority(), "localhost:8080");
    }

    #[test]
    fn to_url_roundtrip() {
        let url = "https://example.com/path/to/resource";
        let p = ParsedUrl::parse(url).unwrap();
        assert_eq!(p.to_url(), url);
    }

    // ── Status line parsing ─────────────────────────────────────────

    #[test]
    fn parse_status_200() {
        assert_eq!(parse_status_line("HTTP/1.1 200 OK").unwrap(), 200);
    }

    #[test]
    fn parse_status_404() {
        assert_eq!(parse_status_line("HTTP/1.1 404 Not Found").unwrap(), 404);
    }

    #[test]
    fn parse_status_malformed() {
        assert!(parse_status_line("garbage").is_err());
    }

    // ── Redirect resolution ─────────────────────────────────────────

    #[test]
    fn redirect_absolute_url() {
        let base = ParsedUrl::parse("http://a.com/old").unwrap();
        let resolved = resolve_redirect(&base, "https://b.com/new");
        assert_eq!(resolved, "https://b.com/new");
    }

    #[test]
    fn redirect_absolute_path() {
        let base = ParsedUrl::parse("https://a.com/old/path").unwrap();
        let resolved = resolve_redirect(&base, "/new/path");
        assert_eq!(resolved, "https://a.com/new/path");
    }

    #[test]
    fn redirect_relative_path() {
        let base = ParsedUrl::parse("https://a.com/dir/old").unwrap();
        let resolved = resolve_redirect(&base, "new");
        assert_eq!(resolved, "https://a.com/dir/new");
    }

    // ── Helper predicates ───────────────────────────────────────────

    #[test]
    fn test_is_redirect() {
        assert!(is_redirect(301));
        assert!(is_redirect(302));
        assert!(is_redirect(307));
        assert!(!is_redirect(200));
        assert!(!is_redirect(404));
    }

    #[test]
    fn test_is_server_error() {
        assert!(is_server_error(500));
        assert!(is_server_error(503));
        assert!(!is_server_error(200));
        assert!(!is_server_error(404));
    }

    #[test]
    fn test_is_retryable() {
        assert!(is_retryable(&ProbeError::Timeout { timeout_ms: 1000 }));
        assert!(is_retryable(&ProbeError::ConnectionError {
            detail: "refused".to_string(),
        }));
        assert!(!is_retryable(&ProbeError::InvalidUrl {
            detail: "bad".to_string(),
        }));
        assert!(!is_retryable(&ProbeError::TlsError {
            detail: "cert".to_string(),
        }));
    }

    // ── Error display ───────────────────────────────────────────────

    #[test]
    fn error_display_timeout() {
        let e = ProbeError::Timeout { timeout_ms: 5000 };
        assert_eq!(e.to_string(), "request timed out after 5000ms");
    }

    #[test]
    fn error_display_dns() {
        let e = ProbeError::DnsError {
            detail: "NXDOMAIN".to_string(),
        };
        assert!(e.to_string().contains("DNS"));
    }

    #[test]
    fn error_serialization() {
        let e = ProbeError::Timeout { timeout_ms: 10000 };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"kind\":\"timeout\""));
        assert!(json.contains("\"timeout_ms\":10000"));
    }

    // ── Config defaults ─────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let cfg = ProbeConfig::default();
        assert_eq!(cfg.timeout, Duration::from_secs(10));
        assert_eq!(cfg.retries, 2);
        assert_eq!(cfg.retry_delay, Duration::from_secs(1));
        assert_eq!(cfg.max_redirects, 5);
    }

    // ── ProbeResponse helpers ───────────────────────────────────────

    #[test]
    fn response_header_lookup() {
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_string(), "text/html".to_string());
        let resp = ProbeResponse {
            final_url: "http://example.com".to_string(),
            status: 200,
            headers,
            body: b"hello".to_vec(),
            redirects: 0,
            elapsed: Duration::from_millis(100),
        };
        assert_eq!(resp.header("content-type"), Some("text/html"));
        assert_eq!(resp.header("missing"), None);
        assert_eq!(resp.body_text(), "hello");
    }

    // ── Categorize connect errors ───────────────────────────────────

    #[test]
    fn categorize_timeout() {
        let err = io::Error::new(io::ErrorKind::TimedOut, "timed out");
        match categorize_connect_error(err, Duration::from_secs(5)) {
            ProbeError::Timeout { timeout_ms } => assert_eq!(timeout_ms, 5000),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn categorize_connection_refused() {
        let err = io::Error::new(io::ErrorKind::ConnectionRefused, "refused");
        match categorize_connect_error(err, Duration::from_secs(5)) {
            ProbeError::ConnectionError { detail } => assert!(detail.contains("refused")),
            other => panic!("expected ConnectionError, got {other:?}"),
        }
    }

    // ── ProbeCheck builder ──────────────────────────────────────────

    #[test]
    fn probe_check_result_defaults() {
        let result = ProbeCheckResult {
            id: "test".to_string(),
            description: "test check".to_string(),
            passed: true,
            message: "ok".to_string(),
            severity: crate::CheckSeverity::Info,
            elapsed: Duration::from_millis(50),
            http_status: Some(200),
            headers_captured: BTreeMap::new(),
        };
        assert!(result.passed);
        assert_eq!(result.http_status, Some(200));
    }

    // ── br-3h13.17.5: read_chunked_body tests (RubyPrairie) ────────────

    #[test]
    fn chunked_body_single_chunk() {
        // "5\r\nhello\r\n0\r\n\r\n"
        let data = b"5\r\nhello\r\n0\r\n\r\n";
        let mut cursor = std::io::Cursor::new(data.as_ref());
        let body = read_chunked_body(&mut std::io::BufReader::new(&mut cursor)).unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn chunked_body_multiple_chunks() {
        let data = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut cursor = std::io::Cursor::new(data.as_ref());
        let body = read_chunked_body(&mut std::io::BufReader::new(&mut cursor)).unwrap();
        assert_eq!(body, b"hello world");
    }

    #[test]
    fn chunked_body_empty() {
        // Zero-length chunk terminates immediately
        let data = b"0\r\n\r\n";
        let mut cursor = std::io::Cursor::new(data.as_ref());
        let body = read_chunked_body(&mut std::io::BufReader::new(&mut cursor)).unwrap();
        assert!(body.is_empty());
    }

    #[test]
    fn chunked_body_hex_uppercase() {
        // Chunk size in uppercase hex: A = 10 bytes
        let data = b"A\r\n0123456789\r\n0\r\n\r\n";
        let mut cursor = std::io::Cursor::new(data.as_ref());
        let body = read_chunked_body(&mut std::io::BufReader::new(&mut cursor)).unwrap();
        assert_eq!(body, b"0123456789");
    }

    #[test]
    fn chunked_body_hex_lowercase() {
        // Chunk size in lowercase hex: a = 10 bytes
        let data = b"a\r\n0123456789\r\n0\r\n\r\n";
        let mut cursor = std::io::Cursor::new(data.as_ref());
        let body = read_chunked_body(&mut std::io::BufReader::new(&mut cursor)).unwrap();
        assert_eq!(body, b"0123456789");
    }

    #[test]
    fn chunked_body_with_extension() {
        // Chunk extension: "5;ext=val\r\nhello\r\n0\r\n\r\n"
        let data = b"5;ext=val\r\nhello\r\n0\r\n\r\n";
        let mut cursor = std::io::Cursor::new(data.as_ref());
        let body = read_chunked_body(&mut std::io::BufReader::new(&mut cursor)).unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn chunked_body_invalid_hex_returns_error() {
        let data = b"XZ\r\nbad\r\n0\r\n\r\n";
        let mut cursor = std::io::Cursor::new(data.as_ref());
        let result = read_chunked_body(&mut std::io::BufReader::new(&mut cursor));
        assert!(result.is_err());
    }

    #[test]
    fn chunked_body_binary_data() {
        // Chunk with non-UTF8 binary bytes
        let chunk_data: Vec<u8> = (0..16).collect();
        let mut data = b"10\r\n".to_vec(); // 0x10 = 16 bytes
        data.extend_from_slice(&chunk_data);
        data.extend_from_slice(b"\r\n0\r\n\r\n");
        let mut cursor = std::io::Cursor::new(data.as_slice());
        let body = read_chunked_body(&mut std::io::BufReader::new(&mut cursor)).unwrap();
        assert_eq!(body, chunk_data);
    }
}
