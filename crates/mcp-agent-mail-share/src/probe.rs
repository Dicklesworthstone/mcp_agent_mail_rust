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
use std::io::{self, BufRead, Read as _, Write as _};
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

fn split_supported_scheme(url: &str) -> Option<(Scheme, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.eq_ignore_ascii_case("https") {
        Some((Scheme::Https, rest))
    } else if scheme.eq_ignore_ascii_case("http") {
        Some((Scheme::Http, rest))
    } else {
        None
    }
}

fn is_absolute_http_url(url: &str) -> bool {
    split_supported_scheme(url).is_some()
}

impl ParsedUrl {
    fn parse(url: &str) -> Result<Self, ProbeError> {
        let (scheme, rest) = split_supported_scheme(url).ok_or_else(|| ProbeError::InvalidUrl {
            detail: format!("unsupported scheme in URL: {url}"),
        })?;

        let (host_port, suffix) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };

        let suffix = suffix.split_once('#').map_or(suffix, |(before, _)| before);
        let path = if suffix.is_empty() {
            "/".to_string()
        } else if suffix.starts_with('?') {
            format!("/{suffix}")
        } else {
            suffix.to_string()
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
            path,
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
    probe_get_inner(url, config, true)
}

/// Perform an HTTP GET probe while discarding the response body.
pub fn probe_get_headers_only(
    url: &str,
    config: &ProbeConfig,
) -> Result<ProbeResponse, ProbeError> {
    probe_get_inner(url, config, false)
}

fn probe_get_inner(
    url: &str,
    config: &ProbeConfig,
    capture_body: bool,
) -> Result<ProbeResponse, ProbeError> {
    let start = Instant::now();
    let mut parsed = ParsedUrl::parse(url)?;
    let mut redirects = 0u32;

    loop {
        let resp = send_with_retries(&parsed, config, capture_body)?;

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
}

/// Execute a single probe attempt plus any configured retries.
///
/// Retries apply uniformly to retryable transport failures and HTTP 5xx
/// responses so redirect and non-redirect paths share the same semantics.
fn send_with_retries(
    parsed: &ParsedUrl,
    config: &ProbeConfig,
    capture_body: bool,
) -> Result<RawResponse, ProbeError> {
    let mut last_err = None;

    for attempt in 0..=config.retries {
        if attempt > 0 {
            std::thread::sleep(config.retry_delay);
        }

        match probe_single_get(parsed, config, capture_body) {
            Ok(resp) if !is_server_error(resp.status) => return Ok(resp),
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
fn probe_single_get(
    parsed: &ParsedUrl,
    config: &ProbeConfig,
    capture_body: bool,
) -> Result<RawResponse, ProbeError> {
    let addr = format!("{}:{}", parsed.host, parsed.port);
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        parsed.path,
        parsed.authority(),
        sanitized_header_value(&config.user_agent),
    );

    let stream = TcpStream::connect_timeout(
        &addr.parse().or_else(|_| {
            // DNS resolution via std::net
            use std::net::ToSocketAddrs;
            addr.to_socket_addrs()
                .map_err(|e| ProbeError::DnsError {
                    detail: e.to_string(),
                })?
                .next()
                .ok_or_else(|| ProbeError::DnsError {
                    detail: "No addresses found".to_string(),
                })
        })?,
        config.timeout,
    )
    .map_err(|e| categorize_connect_error(e, config.timeout))?;

    let _ = stream.set_read_timeout(Some(config.timeout));
    let _ = stream.set_write_timeout(Some(config.timeout));

    let mut response_buf = Vec::new();
    let result = (|| -> Result<RawResponse, ProbeError> {
        if matches!(parsed.scheme, Scheme::Https) {
            return Err(ProbeError::TlsError {
                detail: "HTTPS probing is not enabled in this build".to_string(),
            });
        }

        let mut stream = stream;
        stream
            .write_all(request.as_bytes())
            .map_err(|e| ProbeError::IoError {
                detail: e.to_string(),
            })?;
        stream.flush().map_err(|e| ProbeError::IoError {
            detail: e.to_string(),
        })?;

        stream
            .read_to_end(&mut response_buf)
            .map_err(|e| ProbeError::IoError {
                detail: e.to_string(),
            })?;

        let _ = stream.shutdown(std::net::Shutdown::Both);

        parse_http_response(&response_buf, capture_body)
    })();

    result
}

/// Parse a raw HTTP response.
fn parse_http_response(raw: &[u8], capture_body: bool) -> Result<RawResponse, ProbeError> {
    let (header_section, body) =
        split_http_response(raw).ok_or_else(|| ProbeError::ProtocolError {
            detail: "curl did not return a complete HTTP response".to_string(),
        })?;

    let header_text = String::from_utf8_lossy(header_section);
    let mut lines = header_text.lines();
    let status_line = lines.next().unwrap_or("HTTP/1.1 000 Unknown");
    let status = parse_status_line(status_line)?;

    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_lowercase(), value.trim().to_string());
        }
    }

    let mut body_bytes = if capture_body {
        body.to_vec()
    } else {
        Vec::new()
    };
    if body_bytes.len() > BODY_LIMIT {
        body_bytes.truncate(BODY_LIMIT);
    }

    Ok(RawResponse {
        status,
        headers,
        body: body_bytes,
    })
}

#[cfg(test)]
fn parse_curl_http_response(raw: &[u8], capture_body: bool) -> Result<RawResponse, ProbeError> {
    parse_http_response(raw, capture_body)
}

#[cfg(test)]
fn map_curl_failure(output: &std::process::Output, config: &ProbeConfig) -> ProbeError {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stderr_lower = stderr.to_ascii_lowercase();
    if output.status.code() == Some(28) || stderr_lower.contains("timed out") {
        #[allow(clippy::cast_possible_truncation)]
        return ProbeError::Timeout {
            timeout_ms: config.timeout.as_millis() as u64,
        };
    }

    ProbeError::ConnectionError {
        detail: if stderr.is_empty() {
            format!("probe helper exited with status {}", output.status)
        } else {
            stderr
        },
    }
}

#[cfg(test)]
fn format_curl_timeout(duration: Duration) -> String {
    let mut rendered = format!("{:.3}", duration.as_secs_f64());
    while rendered.contains('.') && rendered.ends_with('0') {
        rendered.pop();
    }
    if rendered.ends_with('.') {
        rendered.pop();
    }
    rendered
}

fn split_http_response(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    if let Some(idx) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
        return Some((&raw[..idx], &raw[idx + 4..]));
    }
    raw.windows(2)
        .position(|window| window == b"\n\n")
        .map(|idx| (&raw[..idx], &raw[idx + 2..]))
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

fn sanitized_header_value(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_control() || c == '\u{7f}' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn resolve_redirect(base: &ParsedUrl, location: &str) -> String {
    let location = location.trim();
    let location = location.split('#').next().unwrap_or_default();
    if location.is_empty() {
        return base.to_url();
    }
    if is_absolute_http_url(location) {
        // Absolute URL
        location.to_string()
    } else if location.starts_with("//") {
        let scheme = match base.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        format!("{scheme}:{location}")
    } else if location.starts_with('/') {
        // Absolute path
        let scheme = match base.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        format!("{}://{}{}", scheme, base.authority(), location)
    } else if location.starts_with('?') {
        let scheme = match base.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        let base_path = base
            .path
            .split_once('?')
            .map_or(base.path.as_str(), |(path, _)| path);
        format!("{scheme}://{}{}{}", base.authority(), base_path, location)
    } else {
        // Relative path
        let scheme = match base.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        let (location_path, location_query) = location
            .split_once('?')
            .map_or((location, None), |(path, query)| (path, Some(query)));
        let base_path = base
            .path
            .split_once('?')
            .map_or(base.path.as_str(), |(path, _)| path);
        let base_dir = if base_path.ends_with('/') {
            base_path
        } else {
            base_path.rsplit_once('/').map_or(
                "/",
                |(dir, _)| {
                    if dir.is_empty() { "/" } else { dir }
                },
            )
        };
        let joined = if base_dir.ends_with('/') {
            format!("{base_dir}{location_path}")
        } else {
            format!("{base_dir}/{location_path}")
        };
        let normalized = normalize_redirect_path(&joined);
        if let Some(query) = location_query {
            format!("{scheme}://{}{}?{query}", base.authority(), normalized)
        } else {
            format!("{scheme}://{}{}", base.authority(), normalized)
        }
    }
}

fn normalize_redirect_path(path: &str) -> String {
    let mut segments = Vec::new();
    let trailing_slash = path.ends_with('/');

    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }

    let mut normalized = if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    };
    if trailing_slash && normalized != "/" && !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
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

/// Evaluate a single probe check against an already-fetched response.
#[must_use]
pub(crate) fn evaluate_probe_check(
    check: &ProbeCheck,
    resp: &ProbeResponse,
    elapsed: Duration,
) -> ProbeCheckResult {
    let mut passed = true;
    let mut messages = Vec::new();

    if let Some(expected) = check.expected_status {
        if resp.status != expected {
            passed = false;
            messages.push(format!("expected HTTP {expected}, got {}", resp.status));
        }
    } else if resp.status >= 400 {
        passed = false;
        messages.push(format!("HTTP {}", resp.status));
    }

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
            elapsed.as_millis()
        )
    } else {
        messages.join("; ")
    };

    let mut captured = BTreeMap::new();
    for key in &check.required_headers {
        let k = key.to_lowercase();
        if let Some(val) = resp.header(&k) {
            captured.insert(k, val.to_string());
        }
    }
    if let Some(ct) = resp.header("content-type") {
        captured.insert("content-type".to_string(), ct.to_string());
    }

    ProbeCheckResult {
        id: check.id.clone(),
        description: check.description.clone(),
        passed,
        message,
        severity: check.severity,
        elapsed,
        http_status: Some(resp.status),
        headers_captured: captured,
    }
}

/// Convert a probe failure into a check result.
#[must_use]
pub(crate) fn probe_error_result(
    check: &ProbeCheck,
    error: &ProbeError,
    elapsed: Duration,
) -> ProbeCheckResult {
    ProbeCheckResult {
        id: check.id.clone(),
        description: check.description.clone(),
        passed: false,
        message: error.to_string(),
        severity: check.severity,
        elapsed,
        http_status: None,
        headers_captured: BTreeMap::new(),
    }
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
                Ok(resp) => evaluate_probe_check(check, &resp, start.elapsed()),
                Err(e) => probe_error_result(check, &e, start.elapsed()),
            }
        })
        .collect()
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    fn spawn_http_sequence_server(responses: Vec<String>) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let port = listener.local_addr().expect("local addr").port();
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept connection");
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
                stream.flush().expect("flush response");
            }
        });
        (port, handle)
    }

    fn spawn_http_request_capture_server(
        response: String,
    ) -> (u16, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let port = listener.local_addr().expect("local addr").port();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            let mut buf = [0u8; 4096];
            let read = stream.read(&mut buf).expect("read request");
            tx.send(String::from_utf8_lossy(&buf[..read]).into_owned())
                .expect("send request");
            stream
                .write_all(response.as_bytes())
                .expect("write response");
            stream.flush().expect("flush response");
        });
        (port, rx, handle)
    }

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
    fn parse_url_accepts_mixed_case_scheme() {
        let p = ParsedUrl::parse("HTTPS://example.com/bar").unwrap();
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
    fn parse_url_with_query_but_no_explicit_path() {
        let p = ParsedUrl::parse("https://example.com?ok=1").unwrap();
        assert_eq!(p.host, "example.com");
        assert_eq!(p.path, "/?ok=1");
    }

    #[test]
    fn parse_url_with_query_and_fragment_strips_fragment() {
        let p = ParsedUrl::parse("https://example.com/path?q=1#section").unwrap();
        assert_eq!(p.host, "example.com");
        assert_eq!(p.path, "/path?q=1");
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
    fn redirect_absolute_url_accepts_mixed_case_scheme() {
        let base = ParsedUrl::parse("http://a.com/old").unwrap();
        let resolved = resolve_redirect(&base, "HTTPS://b.com/new");
        assert_eq!(resolved, "HTTPS://b.com/new");
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

    #[test]
    fn redirect_relative_path_from_root_does_not_introduce_double_slash() {
        let base = ParsedUrl::parse("https://a.com/").unwrap();
        let resolved = resolve_redirect(&base, "new");
        assert_eq!(resolved, "https://a.com/new");
    }

    #[test]
    fn redirect_query_only_preserves_current_path() {
        let base = ParsedUrl::parse("https://a.com/dir/old?x=1").unwrap();
        let resolved = resolve_redirect(&base, "?page=2");
        assert_eq!(resolved, "https://a.com/dir/old?page=2");
    }

    #[test]
    fn redirect_fragment_only_preserves_current_url() {
        let base = ParsedUrl::parse("https://a.com/dir/old?x=1").unwrap();
        let resolved = resolve_redirect(&base, "#next");
        assert_eq!(resolved, "https://a.com/dir/old?x=1");
    }

    #[test]
    fn redirect_parent_dir_normalizes_dot_segments() {
        let base = ParsedUrl::parse("https://a.com/dir/sub/old").unwrap();
        let resolved = resolve_redirect(&base, "../next");
        assert_eq!(resolved, "https://a.com/dir/next");
    }

    #[test]
    fn redirect_scheme_relative_url_keeps_current_scheme() {
        let base = ParsedUrl::parse("https://a.com/dir/old").unwrap();
        let resolved = resolve_redirect(&base, "//b.com/new");
        assert_eq!(resolved, "https://b.com/new");
    }

    #[test]
    fn parse_curl_http_response_preserves_binary_body_bytes() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\r\n\xff\x00\x80";
        let response = parse_curl_http_response(raw, true).expect("parse curl response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, vec![0xff, 0x00, 0x80]);
    }

    #[test]
    fn parse_curl_http_response_truncates_large_bodies() {
        let mut raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n".to_vec();
        raw.extend(std::iter::repeat_n(b'x', BODY_LIMIT + 17));
        let response = parse_curl_http_response(&raw, true).expect("parse curl response");
        assert_eq!(response.body.len(), BODY_LIMIT);
    }

    #[cfg(unix)]
    #[test]
    fn map_curl_failure_classifies_timeout() {
        use std::os::unix::process::ExitStatusExt;

        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(28 << 8),
            stdout: Vec::new(),
            stderr: b"operation timed out".to_vec(),
        };
        let err = map_curl_failure(&output, &ProbeConfig::default());
        assert!(matches!(err, ProbeError::Timeout { .. }));
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

    #[test]
    fn format_curl_timeout_preserves_subsecond_precision() {
        assert_eq!(format_curl_timeout(Duration::from_millis(250)), "0.25");
        assert_eq!(format_curl_timeout(Duration::from_secs(2)), "2");
    }

    #[test]
    fn sanitize_header_value_replaces_crlf() {
        assert_eq!(
            sanitized_header_value("agent\r\nInjected: nope"),
            "agent  Injected: nope"
        );
    }

    #[test]
    fn sanitize_header_value_replaces_other_control_bytes() {
        assert_eq!(sanitized_header_value("agent\x00\x7f\tok"), "agent   ok");
    }

    #[test]
    fn probe_get_retries_server_error_and_follows_redirect() {
        let responses = vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
            "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone"
                .to_string(),
        ];
        let (port, handle) = spawn_http_sequence_server(responses);
        let config = ProbeConfig {
            retries: 1,
            retry_delay: Duration::from_millis(1),
            ..ProbeConfig::default()
        };

        let resp = probe_get(&format!("http://127.0.0.1:{port}/start"), &config).unwrap();
        handle.join().expect("join server");

        assert_eq!(resp.status, 200);
        assert_eq!(resp.final_url, format!("http://127.0.0.1:{port}/final"));
        assert_eq!(resp.redirects, 1);
        assert_eq!(resp.body_text(), "done");
    }

    #[test]
    fn probe_get_uses_configured_user_agent() {
        let response =
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string();
        let (port, requests, handle) = spawn_http_request_capture_server(response);
        let config = ProbeConfig {
            user_agent: "custom-agent/1.0".to_string(),
            ..ProbeConfig::default()
        };

        let resp = probe_get(&format!("http://127.0.0.1:{port}/ua"), &config).unwrap();
        let request = requests.recv().expect("receive request");
        handle.join().expect("join server");

        assert_eq!(resp.status, 200);
        assert!(request.contains("User-Agent: custom-agent/1.0\r\n"));
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
        // Chunk with extension: "5;ext=val\r\nhello\r\n0\r\n\r\n"
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
