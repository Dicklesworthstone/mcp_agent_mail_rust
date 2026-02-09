//! System Health screen for `AgentMailTUI`.
//!
//! Focus: connection diagnostics (base-path, auth, handshake, reachability) with
//! actionable remediation hints.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ftui::layout::Rect;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Event, Frame, KeyCode, KeyEventKind};
use ftui_runtime::program::Cmd;
use mcp_agent_mail_core::Config;

use crate::tui_bridge::{ConfigSnapshot, TuiSharedState};

use super::{HelpEntry, MailScreen, MailScreenMsg};

const DIAG_REFRESH_INTERVAL: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const IO_TIMEOUT: Duration = Duration::from_millis(250);
const WORKER_SLEEP: Duration = Duration::from_millis(50);
const MAX_READ_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Level {
    #[default]
    Ok,
    Warn,
    Fail,
}

impl Level {
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ProbeLine {
    level: Level,
    name: &'static str,
    detail: String,
    remediation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ProbeAuthKind {
    #[default]
    Unauth,
    Auth,
}

impl ProbeAuthKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Unauth => "unauth",
            Self::Auth => "auth",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PathProbe {
    path: String,
    kind: ProbeAuthKind,
    status: Option<u16>,
    latency_ms: Option<u64>,
    body_has_tools: Option<bool>,
    error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct DiagnosticsSnapshot {
    checked_at: Option<DateTime<Utc>>,
    endpoint: String,
    web_ui_url: String,
    auth_enabled: bool,
    localhost_unauth_allowed: bool,
    token_present: bool,
    token_len: usize,
    http_host: String,
    http_port: u16,
    configured_path: String,
    tcp_latency_ms: Option<u64>,
    tcp_error: Option<String>,
    path_probes: Vec<PathProbe>,
    lines: Vec<ProbeLine>,
}

#[derive(Debug, Clone)]
struct ParsedEndpoint {
    host: String,
    port: u16,
    path: String,
}

pub struct SystemHealthScreen {
    snapshot: Arc<Mutex<DiagnosticsSnapshot>>,
    refresh_requested: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl SystemHealthScreen {
    #[must_use]
    pub fn new(state: Arc<TuiSharedState>) -> Self {
        let snapshot = Arc::new(Mutex::new(DiagnosticsSnapshot::default()));
        let refresh_requested = Arc::new(AtomicBool::new(true)); // run once immediately
        let stop = Arc::new(AtomicBool::new(false));

        let worker_snapshot = Arc::clone(&snapshot);
        let worker_refresh = Arc::clone(&refresh_requested);
        let worker_stop = Arc::clone(&stop);

        let worker = thread::Builder::new()
            .name("tui-connection-diagnostics".into())
            .spawn(move || {
                diagnostics_worker_loop(&state, &worker_snapshot, &worker_refresh, &worker_stop);
            })
            .ok();

        Self {
            snapshot,
            refresh_requested,
            stop,
            worker,
        }
    }

    fn request_refresh(&self) {
        self.refresh_requested.store(true, Ordering::Relaxed);
    }

    fn snapshot(&self) -> DiagnosticsSnapshot {
        self.snapshot
            .lock()
            .ok()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl Drop for SystemHealthScreen {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.worker.take() {
            let _ = join.join();
        }
    }
}

impl MailScreen for SystemHealthScreen {
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('r') {
                self.request_refresh();
            }
        }
        Cmd::None
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        let snap = self.snapshot();

        let mut body = String::new();
        let _ = writeln!(body, "Endpoint: {}", snap.endpoint);
        let _ = writeln!(body, "Web UI:   {}", snap.web_ui_url);
        let _ = writeln!(
            body,
            "Auth:     {} (token_present: {}, len: {})",
            if snap.auth_enabled {
                "enabled"
            } else {
                "disabled"
            },
            snap.token_present,
            snap.token_len
        );
        if snap.auth_enabled && snap.localhost_unauth_allowed {
            body.push_str("          Note: localhost unauthenticated access is allowed\n");
        }
        let _ = writeln!(
            body,
            "Checked:  {}",
            snap.checked_at
                .map_or_else(|| "(never)".to_string(), |t| t.to_rfc3339())
        );
        body.push_str("\nConnection diagnostics:\n");

        // TCP probe
        if let Some(err) = &snap.tcp_error {
            let _ = writeln!(
                body,
                "- [{}] tcp:{}:{}  {err}",
                Level::Fail.label(),
                snap.http_host,
                snap.http_port
            );
        } else {
            let _ = writeln!(
                body,
                "- [{}] tcp:{}:{}  {}ms",
                Level::Ok.label(),
                snap.http_host,
                snap.http_port,
                snap.tcp_latency_ms.unwrap_or(0)
            );
        }

        // HTTP probes
        for p in &snap.path_probes {
            if let Some(err) = &p.error {
                let _ = writeln!(
                    body,
                    "- [{}] POST {} ({}) tools/list  {err}",
                    Level::Fail.label(),
                    p.path,
                    p.kind.label()
                );
                continue;
            }
            let status = p.status.map_or_else(|| "?".into(), |s| s.to_string());
            let latency = p.latency_ms.unwrap_or(0);
            let tools_hint = match p.body_has_tools {
                Some(true) => "tools=yes",
                Some(false) => "tools=no",
                None => "tools=?",
            };
            let level = classify_http_probe(&snap, p).label();
            let _ = writeln!(
                body,
                "- [{level}] POST {} ({}) tools/list  status:{} {}ms {tools_hint}",
                p.path,
                p.kind.label(),
                status,
                latency
            );
        }

        if !snap.lines.is_empty() {
            body.push_str("\nFindings:\n");
            for line in &snap.lines {
                let _ = writeln!(
                    body,
                    "- [{}] {}: {}",
                    line.level.label(),
                    line.name,
                    line.detail
                );
                if let Some(fix) = &line.remediation {
                    let _ = writeln!(body, "       Fix: {fix}");
                }
            }
        }

        body.push_str("\nKeys: r refresh\n");

        let block = Block::default()
            .title("System Health")
            .border_type(BorderType::Rounded);
        Paragraph::new(body).block(block).render(area, frame);
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![HelpEntry {
            key: "r",
            action: "Refresh diagnostics",
        }]
    }

    fn title(&self) -> &'static str {
        "System Health"
    }
}

fn diagnostics_worker_loop(
    state: &TuiSharedState,
    snapshot: &Mutex<DiagnosticsSnapshot>,
    refresh_requested: &AtomicBool,
    stop: &AtomicBool,
) {
    let mut next_due = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let refresh = refresh_requested.swap(false, Ordering::Relaxed);
        if refresh || now >= next_due {
            let snap = run_diagnostics(state);
            if let Ok(mut guard) = snapshot.lock() {
                *guard = snap;
            }
            next_due = Instant::now() + DIAG_REFRESH_INTERVAL;
        }
        thread::sleep(WORKER_SLEEP);
    }
}

fn run_diagnostics(state: &TuiSharedState) -> DiagnosticsSnapshot {
    let cfg = state.config_snapshot();
    let env_cfg = Config::from_env();

    let mut out = DiagnosticsSnapshot {
        checked_at: Some(Utc::now()),
        endpoint: cfg.endpoint.clone(),
        web_ui_url: cfg.web_ui_url.clone(),
        auth_enabled: cfg.auth_enabled,
        localhost_unauth_allowed: env_cfg.http_allow_localhost_unauthenticated,
        token_present: env_cfg.http_bearer_token.is_some(),
        token_len: env_cfg.http_bearer_token.as_deref().map_or(0, str::len),
        ..Default::default()
    };

    let parsed = match parse_http_endpoint(&cfg) {
        Ok(p) => p,
        Err(e) => {
            out.lines.push(ProbeLine {
                level: Level::Fail,
                name: "endpoint-parse",
                detail: e,
                remediation: Some("Expected endpoint like 'http://127.0.0.1:8766/mcp/'".into()),
            });
            return out;
        }
    };

    out.http_host.clone_from(&parsed.host);
    out.http_port = parsed.port;
    out.configured_path.clone_from(&parsed.path);

    // TCP reachability
    match tcp_probe(&parsed.host, parsed.port) {
        Ok(ms) => out.tcp_latency_ms = Some(ms),
        Err(e) => out.tcp_error = Some(e),
    }

    // Base-path checks (configured + common aliases)
    let mut paths = Vec::new();
    push_unique_path(&mut paths, &parsed.path);
    push_unique_path(&mut paths, "/mcp/");
    push_unique_path(&mut paths, "/api/");

    let token = env_cfg.http_bearer_token.as_deref();

    for path in paths {
        let probe = http_probe_tools_list(
            &parsed.host,
            parsed.port,
            &path,
            ProbeAuthKind::Unauth,
            None,
        );
        out.path_probes.push(probe);
    }

    if let Some(token) = token {
        // Auth sanity: ensure an authenticated tools/list works on the configured path.
        let probe = http_probe_tools_list(
            &parsed.host,
            parsed.port,
            &parsed.path,
            ProbeAuthKind::Auth,
            Some(token),
        );
        out.path_probes.push(probe);
    }

    // Findings / remediation hints
    if out.token_present && out.token_len < 8 {
        out.lines.push(ProbeLine {
            level: Level::Warn,
            name: "auth-token",
            detail: "HTTP_BEARER_TOKEN is set but very short (< 8 chars)".into(),
            remediation: Some(
                "Use a longer token, or unset HTTP_BEARER_TOKEN to disable auth".into(),
            ),
        });
    }

    add_base_path_findings(&mut out);
    add_auth_findings(&mut out);

    out
}

fn push_unique_path(list: &mut Vec<String>, path: &str) {
    if list.iter().any(|p| p == path) {
        return;
    }
    list.push(path.to_string());
}

fn classify_http_probe(snap: &DiagnosticsSnapshot, probe: &PathProbe) -> Level {
    let Some(status) = probe.status else {
        return Level::Fail;
    };

    if probe.kind == ProbeAuthKind::Auth {
        return match status {
            200 => {
                if probe.body_has_tools == Some(false) {
                    Level::Warn
                } else {
                    Level::Ok
                }
            }
            404 | 500..=599 => Level::Fail,
            _ => Level::Warn,
        };
    }

    // If auth is enabled, a 401/403 still indicates the endpoint/path is reachable.
    if snap.auth_enabled && matches!(status, 401 | 403) {
        return Level::Ok;
    }

    match status {
        200 => {
            if snap.auth_enabled {
                // If auth is enabled but unauthenticated requests succeed, flag it.
                Level::Warn
            } else if probe.body_has_tools == Some(false) {
                Level::Warn
            } else {
                Level::Ok
            }
        }
        404 | 500..=599 => Level::Fail,
        _ => Level::Warn,
    }
}

fn add_base_path_findings(out: &mut DiagnosticsSnapshot) {
    let configured = out.configured_path.as_str();
    let configured_ok = out
        .path_probes
        .iter()
        .find(|p| p.kind == ProbeAuthKind::Unauth && p.path == configured)
        .is_some_and(|p| classify_http_probe(out, p) != Level::Fail);

    let mcp_ok = out
        .path_probes
        .iter()
        .find(|p| p.kind == ProbeAuthKind::Unauth && p.path == "/mcp/")
        .is_some_and(|p| classify_http_probe(out, p) != Level::Fail);
    let api_ok = out
        .path_probes
        .iter()
        .find(|p| p.kind == ProbeAuthKind::Unauth && p.path == "/api/")
        .is_some_and(|p| classify_http_probe(out, p) != Level::Fail);

    if !configured_ok && (mcp_ok || api_ok) {
        let good = if mcp_ok { "/mcp/" } else { "/api/" };
        out.lines.push(ProbeLine {
            level: Level::Fail,
            name: "base-path",
            detail: format!(
                "Configured HTTP_PATH {configured} is not reachable, but {good} appears reachable"
            ),
            remediation: Some(format!(
                "Set HTTP_PATH={good} (or run with --path {})",
                good.trim_matches('/')
            )),
        });
    }

    if !mcp_ok && api_ok {
        out.lines.push(ProbeLine {
            level: Level::Warn,
            name: "base-path-alias",
            detail: "Endpoint responds on /api/ but not /mcp/".into(),
            remediation: Some(
                "Clients using /mcp/ will see 404. Use /api/ (or enable /mcp/ alias)".into(),
            ),
        });
    }

    if !api_ok && mcp_ok {
        out.lines.push(ProbeLine {
            level: Level::Warn,
            name: "base-path-alias",
            detail: "Endpoint responds on /mcp/ but not /api/".into(),
            remediation: Some(
                "Clients using /api/ will see 404. Use /mcp/ (or enable /api/ alias)".into(),
            ),
        });
    }
}

fn add_auth_findings(out: &mut DiagnosticsSnapshot) {
    if !out.auth_enabled {
        return;
    }

    // If auth is enabled, at least one path should return 401/403 for unauthenticated access
    // (or 200 if localhost-unauth is allowed). We can't reliably infer localhost allowlist here,
    // so we just flag if *all* probes returned 200.
    if out.localhost_unauth_allowed {
        return;
    }

    let all_200 = out
        .path_probes
        .iter()
        .filter(|p| p.kind == ProbeAuthKind::Unauth)
        .filter_map(|p| p.status)
        .all(|s| s == 200);
    if all_200 {
        out.lines.push(ProbeLine {
            level: Level::Warn,
            name: "auth",
            detail: "Auth appears enabled, but unauthenticated probes returned 200 everywhere".into(),
            remediation: Some("If this is unexpected, verify HTTP_BEARER_TOKEN enforcement and localhost allowlist settings".into()),
        });
    }

    // If token is present, expect the auth probe on configured path to succeed.
    if out.token_present {
        let auth_probe_ok = out
            .path_probes
            .iter()
            .find(|p| p.kind == ProbeAuthKind::Auth && p.path == out.configured_path)
            .is_some_and(|p| p.status == Some(200));
        if !auth_probe_ok {
            out.lines.push(ProbeLine {
                level: Level::Fail,
                name: "auth",
                detail: "Authenticated probe did not succeed on configured endpoint".into(),
                remediation: Some("Verify HTTP_BEARER_TOKEN matches the server config (or unset it to disable auth)".into()),
            });
        }
    }
}

fn tcp_probe(host: &str, port: u16) -> Result<u64, String> {
    let addr = resolve_socket_addr(host, port)?;
    let start = Instant::now();
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| e.to_string())?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(saturating_duration_ms_u64(start.elapsed()))
}

fn http_probe_tools_list(
    host: &str,
    port: u16,
    path: &str,
    kind: ProbeAuthKind,
    bearer_token: Option<&str>,
) -> PathProbe {
    let mut probe = PathProbe {
        path: path.to_string(),
        kind,
        ..Default::default()
    };

    let addr = match resolve_socket_addr(host, port) {
        Ok(a) => a,
        Err(e) => {
            probe.error = Some(e);
            return probe;
        }
    };

    let body = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}";
    let mut req = String::new();
    let _ = write!(req, "POST {path} HTTP/1.1\r\n");
    let _ = write!(req, "Host: {host}:{port}\r\n");
    req.push_str("Content-Type: application/json\r\n");
    let _ = write!(req, "Content-Length: {}\r\n", body.len());
    req.push_str("Connection: close\r\n");
    if let Some(token) = bearer_token {
        // Never log token; header is only used for local self-probe.
        let _ = write!(req, "Authorization: Bearer {token}\r\n");
    }
    req.push_str("\r\n");

    let start = Instant::now();
    let mut stream = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            probe.error = Some(format!("connect failed: {e}"));
            return probe;
        }
    };
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    if let Err(e) = stream.write_all(req.as_bytes()) {
        probe.error = Some(format!("write failed: {e}"));
        return probe;
    }
    if let Err(e) = stream.write_all(body) {
        probe.error = Some(format!("write body failed: {e}"));
        return probe;
    }

    let mut buf = vec![0_u8; MAX_READ_BYTES];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(e) => {
            probe.error = Some(format!("read failed: {e}"));
            return probe;
        }
    };
    buf.truncate(n);

    probe.latency_ms = Some(saturating_duration_ms_u64(start.elapsed()));
    probe.status = parse_http_status(&buf);

    if let Ok(text) = std::str::from_utf8(&buf) {
        // Cheap handshake sanity: tools/list result payload should contain "tools".
        if probe.status == Some(200) {
            probe.body_has_tools = Some(text.contains("\"tools\""));
        }
    }
    let _ = stream.shutdown(Shutdown::Both);

    probe
}

fn parse_http_status(buf: &[u8]) -> Option<u16> {
    let line_end = buf
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or(buf.len());
    let line = std::str::from_utf8(&buf[..line_end]).ok()?;
    // Example: "HTTP/1.1 200 OK"
    let mut parts = line.split_whitespace();
    let _http = parts.next()?;
    let code = parts.next()?;
    code.parse::<u16>().ok()
}

fn saturating_duration_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn resolve_socket_addr(host: &str, port: u16) -> Result<SocketAddr, String> {
    let ip = if host == "localhost" {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        host.parse::<IpAddr>()
            .map_err(|_| format!("unsupported host {host:?} (expected an IP or localhost)"))?
    };
    Ok(SocketAddr::new(ip, port))
}

fn parse_http_endpoint(cfg: &ConfigSnapshot) -> Result<ParsedEndpoint, String> {
    let url = cfg.endpoint.trim();
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported endpoint scheme in {url:?} (expected http://)"))?;

    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };

    let (host, port) = parse_authority_host_port(authority)?;

    Ok(ParsedEndpoint {
        host,
        port,
        path: normalize_path(&path),
    })
}

fn normalize_path(path: &str) -> String {
    if path == "/" {
        return "/".to_string();
    }
    let mut out = path.to_string();
    if !out.starts_with('/') {
        out.insert(0, '/');
    }
    if !out.ends_with('/') {
        out.push('/');
    }
    out
}

fn parse_authority_host_port(authority: &str) -> Result<(String, u16), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6: [::1]:8766
        let Some((host, rest)) = rest.split_once(']') else {
            return Err(format!("invalid IPv6 authority {authority:?}"));
        };
        let port = if let Some(rest) = rest.strip_prefix(':') {
            rest.parse::<u16>()
                .map_err(|_| format!("invalid port in {authority:?}"))?
        } else {
            80
        };
        return Ok((host.to_string(), port));
    }

    let Some((host, port)) = authority.rsplit_once(':') else {
        return Ok((authority.to_string(), 80));
    };
    let port = port
        .parse::<u16>()
        .map_err(|_| format!("invalid port in {authority:?}"))?;
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_endpoint_ipv4() {
        let cfg = ConfigSnapshot {
            endpoint: "http://127.0.0.1:8766/api/".into(),
            http_path: "/api/".into(),
            web_ui_url: "http://127.0.0.1:8766/mail".into(),
            app_environment: "development".into(),
            auth_enabled: false,
            database_url: "sqlite:///./storage.sqlite3".into(),
            storage_root: "/tmp/am".into(),
            console_theme: "cyberpunk_aurora".into(),
            tool_filter_profile: "default".into(),
        };
        let parsed = parse_http_endpoint(&cfg).expect("parse");
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 8766);
        assert_eq!(parsed.path, "/api/");
    }

    #[test]
    fn parse_http_endpoint_ipv6_bracketed() {
        let cfg = ConfigSnapshot {
            endpoint: "http://[::1]:8766/mcp/".into(),
            http_path: "/mcp/".into(),
            web_ui_url: "http://[::1]:8766/mail".into(),
            app_environment: "development".into(),
            auth_enabled: true,
            database_url: "sqlite:///./storage.sqlite3".into(),
            storage_root: "/tmp/am".into(),
            console_theme: "cyberpunk_aurora".into(),
            tool_filter_profile: "default".into(),
        };
        let parsed = parse_http_endpoint(&cfg).expect("parse");
        assert_eq!(parsed.host, "::1");
        assert_eq!(parsed.port, 8766);
        assert_eq!(parsed.path, "/mcp/");
    }

    #[test]
    fn normalize_path_adds_slashes() {
        assert_eq!(normalize_path("api"), "/api/");
        assert_eq!(normalize_path("/api"), "/api/");
        assert_eq!(normalize_path("/api/"), "/api/");
    }

    #[test]
    fn normalize_path_root() {
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn normalize_path_nested() {
        assert_eq!(normalize_path("a/b/c"), "/a/b/c/");
        assert_eq!(normalize_path("/a/b/c"), "/a/b/c/");
        assert_eq!(normalize_path("/a/b/c/"), "/a/b/c/");
    }

    // --- parse_http_status ---

    #[test]
    fn parse_http_status_200_ok() {
        assert_eq!(parse_http_status(b"HTTP/1.1 200 OK\r\n"), Some(200));
    }

    #[test]
    fn parse_http_status_404_not_found() {
        assert_eq!(
            parse_http_status(b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\n"),
            Some(404)
        );
    }

    #[test]
    fn parse_http_status_401() {
        assert_eq!(
            parse_http_status(b"HTTP/1.1 401 Unauthorized\r\n"),
            Some(401)
        );
    }

    #[test]
    fn parse_http_status_500() {
        assert_eq!(
            parse_http_status(b"HTTP/1.1 500 Internal Server Error\r\n"),
            Some(500)
        );
    }

    #[test]
    fn parse_http_status_no_crlf() {
        // No \r\n — line_end falls to buf.len(), still parseable
        assert_eq!(parse_http_status(b"HTTP/1.1 200 OK"), Some(200));
    }

    #[test]
    fn parse_http_status_empty() {
        assert_eq!(parse_http_status(b""), None);
    }

    #[test]
    fn parse_http_status_garbage() {
        assert_eq!(parse_http_status(b"not http at all\r\n"), None);
    }

    #[test]
    fn parse_http_status_invalid_code() {
        assert_eq!(parse_http_status(b"HTTP/1.1 XYZ Oops\r\n"), None);
    }

    // --- resolve_socket_addr ---

    #[test]
    fn resolve_socket_addr_localhost() {
        let addr = resolve_socket_addr("localhost", 8766).expect("resolve");
        assert_eq!(addr, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8766));
    }

    #[test]
    fn resolve_socket_addr_ipv4() {
        let addr = resolve_socket_addr("192.168.1.1", 9000).expect("resolve");
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(addr.port(), 9000);
    }

    #[test]
    fn resolve_socket_addr_ipv6() {
        let addr = resolve_socket_addr("::1", 80).expect("resolve");
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 80);
    }

    #[test]
    fn resolve_socket_addr_invalid_host() {
        let err = resolve_socket_addr("not-an-ip", 80).unwrap_err();
        assert!(err.contains("unsupported host"));
    }

    // --- parse_authority_host_port ---

    #[test]
    fn parse_authority_ipv4_with_port() {
        let (host, port) = parse_authority_host_port("127.0.0.1:8766").expect("parse");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8766);
    }

    #[test]
    fn parse_authority_host_only_defaults_port_80() {
        let (host, port) = parse_authority_host_port("example.com").expect("parse");
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
    }

    #[test]
    fn parse_authority_ipv6_bracketed_with_port() {
        let (host, port) = parse_authority_host_port("[::1]:9090").expect("parse");
        assert_eq!(host, "::1");
        assert_eq!(port, 9090);
    }

    #[test]
    fn parse_authority_ipv6_bracketed_no_port() {
        let (host, port) = parse_authority_host_port("[::1]").expect("parse");
        assert_eq!(host, "::1");
        assert_eq!(port, 80);
    }

    #[test]
    fn parse_authority_invalid_port() {
        let err = parse_authority_host_port("127.0.0.1:notaport").unwrap_err();
        assert!(err.contains("invalid port"));
    }

    #[test]
    fn parse_authority_ipv6_unclosed_bracket() {
        let err = parse_authority_host_port("[::1").unwrap_err();
        assert!(err.contains("invalid IPv6"));
    }

    // --- push_unique_path ---

    #[test]
    fn push_unique_path_deduplicates() {
        let mut paths = Vec::new();
        push_unique_path(&mut paths, "/mcp/");
        push_unique_path(&mut paths, "/api/");
        push_unique_path(&mut paths, "/mcp/");
        assert_eq!(paths, vec!["/mcp/", "/api/"]);
    }

    #[test]
    fn push_unique_path_empty_list() {
        let mut paths = Vec::new();
        push_unique_path(&mut paths, "/");
        assert_eq!(paths.len(), 1);
    }

    // --- Level and ProbeAuthKind labels ---

    #[test]
    fn level_labels() {
        assert_eq!(Level::Ok.label(), "OK");
        assert_eq!(Level::Warn.label(), "WARN");
        assert_eq!(Level::Fail.label(), "FAIL");
    }

    #[test]
    fn probe_auth_kind_labels() {
        assert_eq!(ProbeAuthKind::Unauth.label(), "unauth");
        assert_eq!(ProbeAuthKind::Auth.label(), "auth");
    }

    // --- classify_http_probe ---

    fn make_snap(auth_enabled: bool) -> DiagnosticsSnapshot {
        DiagnosticsSnapshot {
            auth_enabled,
            ..Default::default()
        }
    }

    fn make_probe(
        kind: ProbeAuthKind,
        status: Option<u16>,
        body_has_tools: Option<bool>,
    ) -> PathProbe {
        PathProbe {
            path: "/mcp/".into(),
            kind,
            status,
            body_has_tools,
            ..Default::default()
        }
    }

    #[test]
    fn classify_no_status_is_fail() {
        let snap = make_snap(false);
        let probe = make_probe(ProbeAuthKind::Unauth, None, None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Fail);
    }

    #[test]
    fn classify_auth_200_with_tools_is_ok() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Auth, Some(200), Some(true));
        assert_eq!(classify_http_probe(&snap, &probe), Level::Ok);
    }

    #[test]
    fn classify_auth_200_no_tools_is_warn() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Auth, Some(200), Some(false));
        assert_eq!(classify_http_probe(&snap, &probe), Level::Warn);
    }

    #[test]
    fn classify_auth_404_is_fail() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Auth, Some(404), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Fail);
    }

    #[test]
    fn classify_auth_500_is_fail() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Auth, Some(500), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Fail);
    }

    #[test]
    fn classify_auth_302_is_warn() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Auth, Some(302), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Warn);
    }

    #[test]
    fn classify_unauth_401_auth_enabled_is_ok() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(401), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Ok);
    }

    #[test]
    fn classify_unauth_403_auth_enabled_is_ok() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(403), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Ok);
    }

    #[test]
    fn classify_unauth_200_auth_disabled_with_tools_is_ok() {
        let snap = make_snap(false);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(200), Some(true));
        assert_eq!(classify_http_probe(&snap, &probe), Level::Ok);
    }

    #[test]
    fn classify_unauth_200_auth_disabled_no_tools_is_warn() {
        let snap = make_snap(false);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(200), Some(false));
        assert_eq!(classify_http_probe(&snap, &probe), Level::Warn);
    }

    #[test]
    fn classify_unauth_200_auth_enabled_is_warn() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(200), Some(true));
        assert_eq!(classify_http_probe(&snap, &probe), Level::Warn);
    }

    #[test]
    fn classify_unauth_404_is_fail() {
        let snap = make_snap(false);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(404), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Fail);
    }

    #[test]
    fn classify_unauth_503_is_fail() {
        let snap = make_snap(false);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(503), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Fail);
    }

    // --- add_base_path_findings ---

    #[test]
    fn base_path_findings_configured_ok_no_finding() {
        let mut out = DiagnosticsSnapshot {
            configured_path: "/mcp/".into(),
            path_probes: vec![
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(200),
                    body_has_tools: Some(true),
                    ..Default::default()
                },
                PathProbe {
                    path: "/api/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(200),
                    body_has_tools: Some(true),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        add_base_path_findings(&mut out);
        assert!(
            out.lines.is_empty(),
            "no findings when configured path works"
        );
    }

    #[test]
    fn base_path_findings_configured_fails_mcp_works() {
        let mut out = DiagnosticsSnapshot {
            configured_path: "/custom/".into(),
            path_probes: vec![
                PathProbe {
                    path: "/custom/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(404),
                    ..Default::default()
                },
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(200),
                    body_has_tools: Some(true),
                    ..Default::default()
                },
                PathProbe {
                    path: "/api/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(404),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        add_base_path_findings(&mut out);
        assert!(
            out.lines
                .iter()
                .any(|l| l.name == "base-path" && l.level == Level::Fail)
        );
        assert!(out.lines.iter().any(|l| l.detail.contains("/mcp/")));
    }

    #[test]
    fn base_path_findings_mcp_down_api_up_warns() {
        let mut out = DiagnosticsSnapshot {
            configured_path: "/mcp/".into(),
            path_probes: vec![
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(404),
                    ..Default::default()
                },
                PathProbe {
                    path: "/api/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(200),
                    body_has_tools: Some(true),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        add_base_path_findings(&mut out);
        // Should have both a base-path FAIL and a base-path-alias WARN
        assert!(
            out.lines
                .iter()
                .any(|l| l.name == "base-path" && l.level == Level::Fail)
        );
        assert!(
            out.lines
                .iter()
                .any(|l| l.name == "base-path-alias" && l.level == Level::Warn)
        );
    }

    #[test]
    fn base_path_findings_api_down_mcp_up_warns() {
        let mut out = DiagnosticsSnapshot {
            configured_path: "/mcp/".into(),
            path_probes: vec![
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(200),
                    body_has_tools: Some(true),
                    ..Default::default()
                },
                PathProbe {
                    path: "/api/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(404),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        add_base_path_findings(&mut out);
        assert!(
            out.lines
                .iter()
                .any(|l| l.name == "base-path-alias" && l.detail.contains("/api/"))
        );
    }

    // --- add_auth_findings ---

    #[test]
    fn auth_findings_disabled_no_findings() {
        let mut out = DiagnosticsSnapshot {
            auth_enabled: false,
            ..Default::default()
        };
        add_auth_findings(&mut out);
        assert!(out.lines.is_empty());
    }

    #[test]
    fn auth_findings_localhost_unauth_allowed_no_findings() {
        let mut out = DiagnosticsSnapshot {
            auth_enabled: true,
            localhost_unauth_allowed: true,
            ..Default::default()
        };
        add_auth_findings(&mut out);
        assert!(out.lines.is_empty());
    }

    #[test]
    fn auth_findings_all_200_warns() {
        let mut out = DiagnosticsSnapshot {
            auth_enabled: true,
            localhost_unauth_allowed: false,
            path_probes: vec![PathProbe {
                path: "/mcp/".into(),
                kind: ProbeAuthKind::Unauth,
                status: Some(200),
                ..Default::default()
            }],
            ..Default::default()
        };
        add_auth_findings(&mut out);
        assert!(
            out.lines
                .iter()
                .any(|l| l.name == "auth" && l.level == Level::Warn)
        );
    }

    #[test]
    fn auth_findings_401_no_all200_warn() {
        let mut out = DiagnosticsSnapshot {
            auth_enabled: true,
            localhost_unauth_allowed: false,
            path_probes: vec![PathProbe {
                path: "/mcp/".into(),
                kind: ProbeAuthKind::Unauth,
                status: Some(401),
                ..Default::default()
            }],
            ..Default::default()
        };
        add_auth_findings(&mut out);
        // Should NOT have the "all 200" warning
        assert!(!out.lines.iter().any(|l| l.name == "auth"
            && l.level == Level::Warn
            && l.detail.contains("200 everywhere")));
    }

    #[test]
    fn auth_findings_token_present_auth_probe_fails() {
        let mut out = DiagnosticsSnapshot {
            auth_enabled: true,
            localhost_unauth_allowed: false,
            token_present: true,
            configured_path: "/mcp/".into(),
            path_probes: vec![
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(401),
                    ..Default::default()
                },
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Auth,
                    status: Some(403),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        add_auth_findings(&mut out);
        assert!(out.lines.iter().any(|l| l.name == "auth"
            && l.level == Level::Fail
            && l.detail.contains("Authenticated probe did not succeed")));
    }

    #[test]
    fn auth_findings_token_present_auth_probe_ok() {
        let mut out = DiagnosticsSnapshot {
            auth_enabled: true,
            localhost_unauth_allowed: false,
            token_present: true,
            configured_path: "/mcp/".into(),
            path_probes: vec![
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(401),
                    ..Default::default()
                },
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Auth,
                    status: Some(200),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        add_auth_findings(&mut out);
        // No auth failure finding
        assert!(
            !out.lines
                .iter()
                .any(|l| l.name == "auth" && l.level == Level::Fail)
        );
    }

    // --- parse_http_endpoint edge cases ---

    #[test]
    fn parse_http_endpoint_no_path() {
        let cfg = ConfigSnapshot {
            endpoint: "http://127.0.0.1:8766".into(),
            http_path: "/".into(),
            web_ui_url: String::new(),
            app_environment: String::new(),
            auth_enabled: false,
            database_url: String::new(),
            storage_root: String::new(),
            console_theme: String::new(),
            tool_filter_profile: String::new(),
        };
        let parsed = parse_http_endpoint(&cfg).expect("parse");
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 8766);
        assert_eq!(parsed.path, "/");
    }

    #[test]
    fn parse_http_endpoint_https_rejected() {
        let cfg = ConfigSnapshot {
            endpoint: "https://127.0.0.1:8766/mcp/".into(),
            http_path: "/mcp/".into(),
            web_ui_url: String::new(),
            app_environment: String::new(),
            auth_enabled: false,
            database_url: String::new(),
            storage_root: String::new(),
            console_theme: String::new(),
            tool_filter_profile: String::new(),
        };
        let err = parse_http_endpoint(&cfg).unwrap_err();
        assert!(err.contains("unsupported endpoint scheme"));
    }

    #[test]
    fn parse_http_endpoint_trims_whitespace() {
        let cfg = ConfigSnapshot {
            endpoint: "  http://127.0.0.1:8766/api/  ".into(),
            http_path: "/api/".into(),
            web_ui_url: String::new(),
            app_environment: String::new(),
            auth_enabled: false,
            database_url: String::new(),
            storage_root: String::new(),
            console_theme: String::new(),
            tool_filter_profile: String::new(),
        };
        let parsed = parse_http_endpoint(&cfg).expect("parse");
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 8766);
        assert_eq!(parsed.path, "/api/");
    }
}
