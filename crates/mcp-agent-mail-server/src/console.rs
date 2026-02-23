//! Rich console output for MCP Agent Mail server.
//!
//! All rendering functions produce ANSI-colored strings suitable for
//! `TerminalWriter::write_log()` or `dashboard_write_log()`.

use ftui::PackedRgba;
use ftui::widgets::sparkline::Sparkline;
use serde_json::Value;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::theme;

// Re-export theme constants used frequently throughout this module.
const RESET: &str = theme::RESET;
const DIM: &str = theme::DIM;

/// Number of data points kept in the sparkline ring buffer.
pub const SPARKLINE_CAPACITY: usize = 60;

// ──────────────────────────────────────────────────────────────────────
// Sensitive value masking (br-1m6a.18)
// ──────────────────────────────────────────────────────────────────────

const MASK_REDACTED: &str = "<redacted>";

/// Sensitive key patterns (case-insensitive substring match).
const SENSITIVE_PATTERNS: &[&str] = &[
    "token",
    "secret",
    "password",
    "credential",
    "bearer",
    "jwt",
    "api_key",
    "private_key",
];

/// Returns `true` if a JSON object key should have its value masked.
#[must_use]
pub fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    // Explicit allowlist: these are identity/safety-critical values and must remain visible.
    if lower == "project_key" || lower == "storage_root" {
        return false;
    }
    // Specific header-like keys that commonly carry secrets.
    if lower == "authorization" || lower == "auth_header" {
        return true;
    }
    SENSITIVE_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Always returns the redaction placeholder (ASCII-only).
#[must_use]
pub fn mask_sensitive_value(_original: &str) -> String {
    MASK_REDACTED.to_string()
}

/// Sanitize a value for known keys where secrets can appear in *values* even if the key is not
/// obviously sensitive (e.g., `postgres://user:pass@host/db`).
#[must_use]
pub fn sanitize_known_value(key: &str, value: &str) -> Option<String> {
    let lower = key.to_ascii_lowercase();
    let is_database_url = lower == "database_url" || lower.ends_with("database_url");
    let is_redis_url =
        lower == "redis_url" || lower.ends_with("redis_url") || lower.contains("redis_url");
    if !is_database_url && !is_redis_url {
        return None;
    }
    sanitize_url_userinfo(value)
}

fn mask_json_depth(value: &Value, depth: usize) -> Value {
    if depth > 20 {
        return value.clone();
    }
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if is_sensitive_key(k) {
                    out.insert(k.clone(), Value::String(mask_sensitive_value("")));
                } else if let Value::String(s) = v
                    && let Some(sanitized) = sanitize_known_value(k, s)
                {
                    out.insert(k.clone(), Value::String(sanitized));
                } else {
                    out.insert(k.clone(), mask_json_depth(v, depth + 1));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|v| mask_json_depth(v, depth + 1)).collect())
        }
        other => other.clone(),
    }
}

/// Walk a `serde_json::Value` tree and replace values whose keys match
/// sensitive patterns with redaction placeholders. Also sanitizes select known keys.
#[must_use]
pub fn mask_json(value: &Value) -> Value {
    mask_json_depth(value, 0)
}

/// Back-compat name used by existing panel renderers/tests.
#[must_use]
pub fn mask_json_params(value: &Value) -> Value {
    mask_json(value)
}

#[must_use]
fn sanitize_url_userinfo(value: &str) -> Option<String> {
    // Conservative, allocation-light sanitizer:
    // - only modifies strings that look like: `scheme://user:pass@host/...`
    // - masks ONLY the password segment
    let scheme_end = value.find("://")?;
    let after_scheme = scheme_end + 3;
    let at_pos = value[after_scheme..].find('@')? + after_scheme;
    let userinfo = &value[after_scheme..at_pos];
    let colon_pos = userinfo.find(':')?;
    let user = &userinfo[..colon_pos];
    let pass = &userinfo[(colon_pos + 1)..];
    if pass.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(value.len() + MASK_REDACTED.len());
    out.push_str(&value[..after_scheme]);
    out.push_str(user);
    out.push(':');
    out.push_str(MASK_REDACTED);
    out.push_str(&value[at_pos..]);
    Some(out)
}

// ──────────────────────────────────────────────────────────────────────
// Duration color gradient (br-1m6a.2)
// ──────────────────────────────────────────────────────────────────────

/// Icon and ANSI color for a duration value.
pub struct DurationStyle {
    pub icon: &'static str,
    pub color: String,
    pub label: String,
}

/// Pick icon + color based on duration thresholds.
#[must_use]
pub fn duration_style(ms: u64) -> DurationStyle {
    let (icon, color) = if ms < 100 {
        ("\u{26a1}", theme::success_bold()) // ⚡ green
    } else if ms < 1000 {
        ("\u{23f1}\u{fe0f}", theme::warning_bold()) // ⏱️ yellow
    } else {
        ("\u{1f40c}", theme::error_bold()) // 🐌 red
    };
    DurationStyle {
        icon,
        color,
        label: format!("{ms}ms"),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Startup banner (br-1m6a.1)
// ──────────────────────────────────────────────────────────────────────

/// Parameters for the startup banner.
pub struct BannerParams<'a> {
    pub app_environment: &'a str,
    pub endpoint: &'a str,
    pub database_url: &'a str,
    pub storage_root: &'a str,
    pub auth_enabled: bool,
    pub tools_log_enabled: bool,
    pub tool_calls_log_enabled: bool,
    pub console_theme: &'a str,
    pub web_ui_url: &'a str,
    pub projects: u64,
    pub agents: u64,
    pub messages: u64,
    pub file_reservations: u64,
    pub contact_links: u64,
}

/// Render the full startup banner as a vector of ANSI-colored lines.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn render_startup_banner(params: &BannerParams<'_>) -> Vec<String> {
    let mut lines = Vec::with_capacity(16);
    let primary = theme::primary_bold();
    let accent = theme::accent();
    let success = theme::success_bold();
    let text = theme::text_bold();

    let auth = if params.auth_enabled {
        format!("{success}enabled{RESET}")
    } else {
        format!("{DIM}disabled{RESET}")
    };
    let tool_log = if params.tools_log_enabled {
        format!("{success}enabled{RESET}")
    } else {
        format!("{DIM}disabled{RESET}")
    };
    let tool_panels = if params.tool_calls_log_enabled {
        format!("{success}enabled{RESET}")
    } else {
        format!("{DIM}disabled{RESET}")
    };
    let database_url = sanitize_known_value("database_url", params.database_url)
        .unwrap_or_else(|| params.database_url.to_string());

    lines.push(String::new());
    lines.push(format!(
        "{primary}MCP Agent Mail{RESET} {DIM}({}){RESET}",
        params.app_environment
    ));
    lines.push(format!("  {accent}Endpoint:{RESET} {}", params.endpoint));
    lines.push(format!("  {accent}Web UI:{RESET} {}", params.web_ui_url));
    lines.push(format!(
        "  {accent}Database:{RESET} {}",
        compact_path(&database_url, 60)
    ));
    lines.push(format!(
        "  {accent}Storage:{RESET} {}",
        compact_path(params.storage_root, 60)
    ));
    lines.push(format!("  {accent}Theme:{RESET} {}", params.console_theme));
    lines.push(format!("  {accent}Auth:{RESET} {auth}"));
    lines.push(format!("  {accent}Tool logs:{RESET} {tool_log}"));
    lines.push(format!("  {accent}Tool panels:{RESET} {tool_panels}"));
    lines.push(format!(
        "  {text}Stats:{RESET} projects={} agents={} messages={} reservations={} contacts={}",
        params.projects,
        params.agents,
        params.messages,
        params.file_reservations,
        params.contact_links
    ));
    lines.push(format!(
        "  {DIM}Tip: interactive layout keys available via '?'{RESET}"
    ));
    lines.push(String::new());
    lines
}

// ──────────────────────────────────────────────────────────────────────
// Tool call panels (br-1m6a.2)
// ──────────────────────────────────────────────────────────────────────

/// Render a tool-call-start panel (DOUBLE border).
#[must_use]
pub fn render_tool_call_start(
    tool_name: &str,
    params: &Value,
    project: Option<&str>,
    agent: Option<&str>,
) -> Vec<String> {
    let mut lines = Vec::with_capacity(20);
    let timestamp = chrono::Utc::now().format("%H:%M:%S%.3f").to_string();

    let secondary_b = theme::secondary_bold();
    let primary = theme::primary_bold();
    let text = theme::text_bold();
    let warning = theme::warning_bold();

    let w = 78;
    let border = "\u{2550}".repeat(w);

    // Top border
    lines.push(format!("{secondary_b}\u{2554}{border}\u{2557}{RESET}"));

    // Title
    let title = format!(" {primary}\u{1f527} TOOL CALL{RESET} {text}{tool_name}{RESET} ");
    let title_vis = 14 + tool_name.len(); // " 🔧 TOOL CALL {tool_name} " (leading + trailing spaces)
    let pad = w.saturating_sub(title_vis);
    lines.push(format!(
        "{secondary_b}\u{2551}{title}{}{secondary_b}\u{2551}{RESET}",
        " ".repeat(pad)
    ));

    // Separator
    lines.push(format!("{secondary_b}\u{2560}{border}\u{2563}{RESET}"));

    // Info rows
    let info_rows: Vec<(&str, String)> = vec![
        ("Tool", tool_name.to_string()),
        ("Time", timestamp),
        ("Project", project.unwrap_or("-").to_string()),
        ("Agent", agent.unwrap_or("-").to_string()),
    ];

    for (label, value) in &info_rows {
        let row = format!(" {DIM}{label}:{RESET} {text}{value}{RESET}");
        let vis_len = 1 + label.len() + 2 + value.len(); // " label: value"
        let pad = w.saturating_sub(vis_len);
        lines.push(format!(
            "{secondary_b}\u{2551}{row}{}{secondary_b}\u{2551}{RESET}",
            " ".repeat(pad)
        ));
    }

    // Parameters section
    lines.push(format!("{secondary_b}\u{2560}{border}\u{2563}{RESET}"));
    {
        let hdr = format!(" {warning}Parameters:{RESET}");
        let hdr_vis = 12; // " Parameters:"
        let pad = w.saturating_sub(hdr_vis);
        lines.push(format!(
            "{secondary_b}\u{2551}{hdr}{}{secondary_b}\u{2551}{RESET}",
            " ".repeat(pad)
        ));
    }

    // Masked + pretty-printed JSON
    let masked = mask_json_params(params);
    let json_str = serde_json::to_string_pretty(&masked).unwrap_or_else(|_| masked.to_string());
    let content_max = w.saturating_sub(2); // 2-char indent
    for jline in json_str.lines() {
        let colored = colorize_json_line(jline);
        let clamped = truncate_to_vis_width(&colored, content_max);
        let vis_len = strip_ansi_len(&clamped);
        let padded = format!("  {clamped}");
        let pad = w.saturating_sub(vis_len + 2);
        lines.push(format!(
            "{secondary_b}\u{2551}{padded}{}{secondary_b}\u{2551}{RESET}",
            " ".repeat(pad)
        ));
    }

    // Bottom border
    lines.push(format!("{secondary_b}\u{255a}{border}\u{255d}{RESET}"));
    lines
}

/// Render a tool-call-end summary panel.
///
/// When `per_table` is non-empty, a "Query Stats" section shows the top 5
/// tables by query count (descending, then alphabetical tie-break).
#[must_use]
pub fn render_tool_call_end(
    tool_name: &str,
    duration_ms: u64,
    result_json: Option<&str>,
    queries: u64,
    query_time_ms: f64,
    per_table: &[(String, u64)],
    max_chars: usize,
) -> Vec<String> {
    let mut lines = Vec::with_capacity(16);
    let w = 78;
    let sep = "\u{2500}".repeat(w);

    let ds = duration_style(duration_ms);
    let color = &ds.color;
    let icon = ds.icon;
    let label = &ds.label;
    let primary = theme::primary_bold();
    let text = theme::text_bold();

    // Top
    lines.push(format!("{color}\u{256d}{sep}\u{256e}{RESET}"));

    // Title
    let title =
        format!(" {color}{icon} {text}{tool_name}{RESET} {color}completed in {label}{RESET} ",);
    let icon_w = ftui::text::display_width(icon);
    // " " + icon + " " + tool_name + " " + "completed in " + label + " "
    let title_vis = 1 + icon_w + 1 + tool_name.len() + 1 + 13 + label.len() + 1;
    let pad = w.saturating_sub(title_vis);
    lines.push(format!(
        "{color}\u{2502}{title}{}{color}\u{2502}{RESET}",
        " ".repeat(pad)
    ));

    // Separator
    lines.push(format!("{color}\u{251c}{sep}\u{2524}{RESET}"));

    // Stats
    let query_time_label = format!("{query_time_ms:.2}ms");
    let stats_line = format!(
        " {DIM}Queries:{RESET} {primary}{queries}{RESET}  {DIM}Query time:{RESET} {primary}{query_time_label}{RESET}"
    );
    let stats_vis = strip_ansi_len(&stats_line);
    let pad = w.saturating_sub(stats_vis);
    lines.push(format!(
        "{color}\u{2502}{stats_line}{}{color}\u{2502}{RESET}",
        " ".repeat(pad)
    ));

    // Per-table query breakdown (top 5)
    if !per_table.is_empty() {
        let warning = theme::warning_bold();
        lines.push(format!("{color}\u{251c}{sep}\u{2524}{RESET}"));
        // Header
        let hdr = format!(" {DIM}Table{RESET}{} {DIM}Count{RESET}", " ".repeat(w - 14));
        lines.push(format!("{color}\u{2502}{hdr}{color}\u{2502}{RESET}"));
        for (tbl, cnt) in per_table.iter().take(5) {
            let cnt_str = cnt.to_string();
            let name_max = w.saturating_sub(cnt_str.len() + 4); // 2 leading + 1 space + 1 trailing
            let name = truncate_with_suffix(tbl, name_max, "");
            let name_len = name.chars().count();
            let gap = w.saturating_sub(name_len + cnt_str.len() + 3);
            let row = format!("  {name}{}{warning}{cnt_str}{RESET}", " ".repeat(gap));
            let row_vis = name_len + cnt_str.len() + gap + 2;
            let pad = w.saturating_sub(row_vis);
            lines.push(format!(
                "{color}\u{2502}{row}{}{color}\u{2502}{RESET}",
                " ".repeat(pad)
            ));
        }
        if per_table.len() > 5 {
            let more = format!("  {DIM}... and {} more{RESET}", per_table.len() - 5);
            let more_vis = 12 + (per_table.len() - 5).to_string().len();
            let pad = w.saturating_sub(more_vis);
            lines.push(format!(
                "{color}\u{2502}{more}{}{color}\u{2502}{RESET}",
                " ".repeat(pad)
            ));
        }
        // Total row
        let total_label = format!(
            "  {DIM}Total:{RESET} {warning}{queries}{RESET} queries in {warning}{query_time_ms:.1}ms{RESET}"
        );
        let total_vis =
            10 + queries.to_string().len() + 13 + format!("{query_time_ms:.1}").len() + 2;
        let pad = w.saturating_sub(total_vis);
        lines.push(format!(
            "{color}\u{2502}{total_label}{}{color}\u{2502}{RESET}",
            " ".repeat(pad)
        ));
    }

    // Result preview (truncated)
    if let Some(result) = result_json {
        let masked_result = mask_result_preview(result);
        let truncated = truncate_with_suffix(&masked_result, max_chars, "...(truncated)");
        lines.push(format!("{color}\u{251c}{sep}\u{2524}{RESET}"));
        {
            let hdr = format!(" {DIM}Result:{RESET}");
            let hdr_vis = 8;
            let pad = w.saturating_sub(hdr_vis);
            lines.push(format!(
                "{color}\u{2502}{hdr}{}{color}\u{2502}{RESET}",
                " ".repeat(pad)
            ));
        }
        let result_content_max = w.saturating_sub(2);
        for rline in truncated.lines().take(8) {
            let clamped = truncate_to_vis_width(rline, result_content_max);
            let vis_len = strip_ansi_len(&clamped);
            let padded = format!("  {clamped}");
            let pad = w.saturating_sub(vis_len + 2);
            lines.push(format!(
                "{color}\u{2502}{padded}{}{color}\u{2502}{RESET}",
                " ".repeat(pad)
            ));
        }
    }

    // Bottom
    lines.push(format!("{color}\u{2570}{sep}\u{256f}{RESET}"));
    lines
}

fn mask_result_preview(result: &str) -> String {
    // Best-effort safety: if the result is JSON, apply the same masking rules
    // used for tool-call params. If parsing fails, fall back to raw text.
    let Ok(value) = serde_json::from_str::<Value>(result) else {
        return result.to_string();
    };

    let masked = mask_json_params(&value);
    serde_json::to_string_pretty(&masked).unwrap_or_else(|_| masked.to_string())
}

fn truncate_with_suffix(input: &str, max_chars: usize, suffix: &str) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let mut chars = input.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    let truncated = chars.next().is_some();

    if !truncated {
        return prefix;
    }

    let suffix_len = suffix.chars().count();
    if max_chars <= suffix_len {
        return prefix;
    }

    let keep = max_chars - suffix_len;
    let mut out: String = prefix.chars().take(keep).collect();
    out.push_str(suffix);
    out
}

// ──────────────────────────────────────────────────────────────────────
// Sparkline ring buffer (br-1m6a.3)
// ──────────────────────────────────────────────────────────────────────

/// Ring buffer of request-rate data points for the sparkline.
pub struct SparklineBuffer {
    data: Mutex<Vec<f64>>,
    counter: AtomicU64,
}

impl Default for SparklineBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl SparklineBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: Mutex::new(vec![0.0; SPARKLINE_CAPACITY]),
            counter: AtomicU64::new(0),
        }
    }

    /// Increment the request counter (called per-request).
    pub fn tick(&self) {
        self.counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Sample the current rate: reads + resets the counter, pushes into the ring.
    /// Call this at a fixed interval (e.g. every 1.2s from the dashboard worker).
    pub fn sample(&self) {
        let count = self.counter.swap(0, Ordering::Relaxed);
        let mut data = self
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if data.len() >= SPARKLINE_CAPACITY {
            data.remove(0);
        }
        let count_u32 = u32::try_from(count).unwrap_or(u32::MAX);
        data.push(f64::from(count_u32));
    }

    /// Get a snapshot of the ring buffer data.
    pub fn snapshot(&self) -> Vec<f64> {
        self.data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Render a sparkline string using frankentui.
    pub fn render_sparkline(&self) -> String {
        let data = self.snapshot();
        Sparkline::new(&data)
            .gradient(theme::sparkline_lo(), theme::sparkline_hi())
            .render_to_string()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Toast notifications (br-1m6a.4)
// ──────────────────────────────────────────────────────────────────────

/// Log level for toast display.
#[derive(Debug, Clone, Copy)]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastLevel {
    fn color(self) -> String {
        match self {
            Self::Info => theme::secondary_bold(),
            Self::Success => theme::success_bold(),
            Self::Warning => theme::warning_bold(),
            Self::Error => theme::error_bold(),
        }
    }

    const fn border_char(self) -> char {
        match self {
            Self::Info => '\u{2502}',                                  // │
            Self::Success | Self::Warning | Self::Error => '\u{2503}', // ┃
        }
    }
}

/// Render a toast notification line.
#[must_use]
pub fn render_toast(icon: &str, message: &str, level: ToastLevel) -> String {
    let color = level.color();
    let border = level.border_char();
    let text = theme::text_bold();
    format!("{color}{border} {icon}  {text}{message}{RESET}")
}

// ──────────────────────────────────────────────────────────────────────
// Structured log panels (br-1m6a.5)
// ──────────────────────────────────────────────────────────────────────

/// Render a structured log panel with level-specific border color.
#[must_use]
pub fn render_log_panel(level: ToastLevel, title: &str, body: &str) -> Vec<String> {
    let mut lines = Vec::with_capacity(8);
    let color = level.color();
    let w = 74;

    let (top_l, top_r, mid_l, mid_r, bot_l, bot_r, h) = match level {
        ToastLevel::Info | ToastLevel::Success => (
            '\u{256d}', '\u{256e}', '\u{2502}', '\u{2502}', '\u{2570}', '\u{256f}', '\u{2500}',
        ),
        ToastLevel::Warning | ToastLevel::Error => (
            '\u{250f}', '\u{2513}', '\u{2503}', '\u{2503}', '\u{2517}', '\u{251b}', '\u{2501}',
        ),
    };

    let border = std::iter::repeat_n(h, w).collect::<String>();
    lines.push(format!("{color}{top_l}{border}{top_r}{RESET}"));

    // Title
    let text = theme::text_bold();
    let title_line = format!(" {text}{title}{RESET}");
    let title_vis = title.len() + 1;
    let pad = w.saturating_sub(title_vis);
    lines.push(format!(
        "{color}{mid_l}{title_line}{}{color}{mid_r}{RESET}",
        " ".repeat(pad)
    ));

    // Body lines
    for bline in body.lines() {
        let padded = format!(" {bline}");
        let vis = bline.len() + 1;
        let pad = w.saturating_sub(vis);
        lines.push(format!(
            "{color}{mid_l}{padded}{}{color}{mid_r}{RESET}",
            " ".repeat(pad)
        ));
    }

    lines.push(format!("{color}{bot_l}{border}{bot_r}{RESET}"));
    lines
}

// ──────────────────────────────────────────────────────────────────────
// Helpers (shared with lib.rs where needed)
// ──────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────
// HTTP request panel styles (br-1m6a.13)
// ──────────────────────────────────────────────────────────────────────

/// ANSI color for an HTTP status code.
#[must_use]
pub fn status_style(code: u16) -> String {
    match code {
        200..=299 => theme::success_bold(),
        300..=399 => theme::accent(),
        400..=499 => theme::warning_bold(),
        _ => theme::error_bold(),
    }
}

/// ANSI color for an HTTP method verb.
#[must_use]
pub fn method_style(method: &str) -> String {
    match method {
        "GET" => theme::accent(),
        "POST" => theme::primary_bold(),
        "PUT" | "PATCH" => theme::secondary_bold(),
        "DELETE" => theme::error_bold(),
        _ => theme::muted(),
    }
}

/// Render a themed HTTP request panel (rounded border, muted).
///
/// Returns `None` when `width < 20` (too narrow for a useful panel).
/// When `use_ansi` is false, produces a plain-text box with no escape codes.
#[must_use]
pub fn render_http_request_panel(
    width: usize,
    method: &str,
    path: &str,
    status: u16,
    duration_ms: u64,
    client_ip: &str,
    use_ansi: bool,
) -> Option<String> {
    if width < 20 {
        return None;
    }
    let inner_width = width.saturating_sub(2);

    let status_str = status.to_string();
    let dur_str = format!("{duration_ms}ms");

    // Title: "METHOD  PATH  STATUS  DUR"
    let reserved: usize = method.len() + status_str.len() + dur_str.len() + 8;
    let max_path: usize = inner_width.saturating_sub(reserved).max(1);
    let display_path = if path.chars().count() <= max_path {
        path.to_string()
    } else if max_path <= 3 {
        path.chars().take(max_path).collect()
    } else {
        let head: String = path.chars().take(max_path - 3).collect();
        format!("{head}...")
    };

    let title_plain = format!("{method}  {display_path}  {status_str}  {dur_str}");

    let title_styled = if use_ansi {
        let m_color = method_style(method);
        let s_color = status_style(status);
        let text = theme::text_bold();
        let ds = duration_style(duration_ms);
        let d_color = &ds.color;
        format!(
            "{m_color}{method}{RESET}  {text}{display_path}{RESET}  {s_color}{status_str}{RESET}  {d_color}{dur_str}{RESET}",
        )
    } else {
        title_plain.clone()
    };

    let top_plain_len: usize = title_plain.len().saturating_add(2);
    if top_plain_len > inner_width {
        return None;
    }

    // Body: "client: <ip>"
    let mut body_plain = format!(" client: {client_ip}");
    if body_plain.len() > inner_width {
        let reserved_ip: usize = " client: ".len();
        let max_ip: usize = inner_width.saturating_sub(reserved_ip).max(1);
        let ip = if client_ip.chars().count() <= max_ip {
            client_ip.to_string()
        } else if max_ip <= 3 {
            client_ip.chars().take(max_ip).collect()
        } else {
            let head: String = client_ip.chars().take(max_ip - 3).collect();
            format!("{head}...")
        };
        body_plain = format!(" client: {ip}");
    }

    let body_plain_len: usize = body_plain.len();

    let body_styled = if use_ansi {
        let muted_c = theme::muted();
        let text = theme::text_bold();
        let prefix = " client: ";
        let ip = body_plain.strip_prefix(prefix).unwrap_or(client_ip);
        format!(" {muted_c}client: {RESET}{text}{ip}{RESET}")
    } else {
        body_plain
    };

    if body_plain_len > inner_width {
        return None;
    }
    let body_pad = " ".repeat(inner_width.saturating_sub(body_plain_len));

    if use_ansi {
        let border_c = theme::muted();
        let h = "\u{2500}".repeat(inner_width);
        let title_pad = " ".repeat(inner_width.saturating_sub(top_plain_len));
        Some(format!(
            "{border_c}\u{256d}{h}\u{256e}{RESET}\n\
             {border_c}\u{2502}{RESET} {title_styled} {title_pad}{border_c}\u{2502}{RESET}\n\
             {border_c}\u{2502}{RESET}{body_styled}{body_pad}{border_c}\u{2502}{RESET}\n\
             {border_c}\u{2570}{h}\u{256f}{RESET}"
        ))
    } else {
        let dash = "-".repeat(inner_width);
        let title_pad = " ".repeat(inner_width.saturating_sub(top_plain_len));
        Some(format!(
            "+{dash}+\n| {title_plain} {title_pad}|\n|{body_styled}{body_pad}|\n+{dash}+"
        ))
    }
}

// ──────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────

/// Compact a filesystem path to fit within `max_chars`.
fn compact_path(input: &str, max_chars: usize) -> String {
    let char_count = input.chars().count();
    if char_count <= max_chars {
        return input.to_string();
    }
    if max_chars <= 5 {
        return input.chars().take(max_chars).collect();
    }
    let keep = max_chars - 3;
    let tail: String = input.chars().skip(char_count - keep).collect();
    format!("...{tail}")
}

/// Strip ANSI escape sequences and return the visible character count.
fn strip_ansi_len(s: &str) -> usize {
    let mut count = 0usize;
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if c == '\x1b' {
            in_escape = true;
        } else {
            count += 1;
        }
    }
    count
}

/// Truncate a string (possibly containing ANSI escapes) so that at most
/// `max_vis` visible characters are retained.  Any active ANSI escape
/// sequence at the truncation point is completed so the output remains valid.
/// An ellipsis `…` is appended when truncation occurs.
fn truncate_to_vis_width(s: &str, max_vis: usize) -> String {
    let total_vis = strip_ansi_len(s);
    if total_vis <= max_vis {
        return s.to_string();
    }
    // Reserve 1 char for the ellipsis
    let keep = max_vis.saturating_sub(1);
    let mut out = String::with_capacity(s.len());
    let mut vis = 0usize;
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            out.push(c);
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if c == '\x1b' {
            in_escape = true;
            out.push(c);
        } else if vis < keep {
            out.push(c);
            vis += 1;
        } else {
            out.push('…');
            break;
        }
    }
    out
}

/// Strip ANSI escape sequences and return the cleaned string.
#[must_use]
pub fn strip_ansi_content(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if c == '\x1b' {
            in_escape = true;
        } else {
            out.push(c);
        }
    }
    out
}

// ──────────────────────────────────────────────────────────────────────
// ANSI → styled ftui Text conversion
// ──────────────────────────────────────────────────────────────────────

/// Standard 8-color ANSI palette (SGR codes 30–37 / 40–47).
const ANSI_PALETTE: [PackedRgba; 8] = [
    PackedRgba::rgb(0, 0, 0),       // black
    PackedRgba::rgb(205, 49, 49),   // red
    PackedRgba::rgb(13, 188, 121),  // green
    PackedRgba::rgb(229, 229, 16),  // yellow
    PackedRgba::rgb(36, 114, 200),  // blue
    PackedRgba::rgb(188, 63, 188),  // magenta
    PackedRgba::rgb(17, 168, 205),  // cyan
    PackedRgba::rgb(229, 229, 229), // white
];

/// Bright 8-color ANSI palette (SGR codes 90–97 / 100–107).
const ANSI_BRIGHT_PALETTE: [PackedRgba; 8] = [
    PackedRgba::rgb(102, 102, 102), // bright black
    PackedRgba::rgb(241, 76, 76),   // bright red
    PackedRgba::rgb(35, 209, 139),  // bright green
    PackedRgba::rgb(245, 245, 67),  // bright yellow
    PackedRgba::rgb(59, 142, 234),  // bright blue
    PackedRgba::rgb(214, 112, 214), // bright magenta
    PackedRgba::rgb(41, 184, 219),  // bright cyan
    PackedRgba::rgb(255, 255, 255), // bright white
];

/// Convert an ANSI 256-color index to RGB.
fn ansi_256_to_rgb(idx: u8) -> PackedRgba {
    if idx < 8 {
        ANSI_PALETTE[idx as usize]
    } else if idx < 16 {
        ANSI_BRIGHT_PALETTE[(idx - 8) as usize]
    } else if idx < 232 {
        // 6×6×6 color cube: xterm standard [0, 95, 135, 175, 215, 255]
        let n = idx - 16;
        let ri = n / 36;
        let gi = (n / 6) % 6;
        let bi = n % 6;
        let to_rgb = |v: u8| -> u8 { if v == 0 { 0 } else { 55 + 40 * v } };
        PackedRgba::rgb(to_rgb(ri), to_rgb(gi), to_rgb(bi))
    } else {
        // Grayscale ramp 232..=255
        let v = 8 + (idx - 232) * 10;
        PackedRgba::rgb(v, v, v)
    }
}

/// Apply a single SGR parameter to a mutable `Style`.
fn apply_sgr_to_style(style: &mut ftui::Style, code: u8, params: &[u8], param_idx: &mut usize) {
    match code {
        0 => *style = ftui::Style::default(),
        1 => *style = style.bold(),
        2 => *style = style.dim(),
        3 => *style = style.italic(),
        4 => *style = style.underline(),
        7 => {
            // Reverse: swap fg/bg
            std::mem::swap(&mut style.fg, &mut style.bg);
        }
        9 => {
            if let Some(ref mut a) = style.attrs {
                *a |= ftui::StyleFlags::STRIKETHROUGH;
            } else {
                style.attrs = Some(ftui::StyleFlags::STRIKETHROUGH);
            }
        }
        22 => {
            // Normal intensity (reset bold+dim)
            if let Some(ref mut a) = style.attrs {
                a.remove(ftui::StyleFlags::BOLD);
                a.remove(ftui::StyleFlags::DIM);
            }
        }
        23 => {
            if let Some(ref mut a) = style.attrs {
                a.remove(ftui::StyleFlags::ITALIC);
            }
        }
        24 => {
            if let Some(ref mut a) = style.attrs {
                a.remove(ftui::StyleFlags::UNDERLINE);
            }
        }
        // 27 (reset reverse) is a no-op; handled by the `_ => {}` wildcard below.
        29 => {
            if let Some(ref mut a) = style.attrs {
                a.remove(ftui::StyleFlags::STRIKETHROUGH);
            }
        }
        // Standard foreground colors
        c @ 30..=37 => style.fg = Some(ANSI_PALETTE[(c - 30) as usize]),
        38 => {
            // Extended foreground
            if let Some(&mode) = params.get(*param_idx) {
                *param_idx += 1;
                if mode == 5 {
                    // 256-color
                    if let Some(&idx) = params.get(*param_idx) {
                        *param_idx += 1;
                        style.fg = Some(ansi_256_to_rgb(idx));
                    }
                } else if mode == 2 {
                    // 24-bit RGB
                    if params.len() >= *param_idx + 3 {
                        let r = params[*param_idx];
                        let g = params[*param_idx + 1];
                        let b = params[*param_idx + 2];
                        *param_idx += 3;
                        style.fg = Some(PackedRgba::rgb(r, g, b));
                    }
                }
            }
        }
        39 => style.fg = None,
        // Standard background colors
        c @ 40..=47 => style.bg = Some(ANSI_PALETTE[(c - 40) as usize]),
        48 => {
            // Extended background
            if let Some(&mode) = params.get(*param_idx) {
                *param_idx += 1;
                if mode == 5 {
                    if let Some(&idx) = params.get(*param_idx) {
                        *param_idx += 1;
                        style.bg = Some(ansi_256_to_rgb(idx));
                    }
                } else if mode == 2 && params.len() >= *param_idx + 3 {
                    let r = params[*param_idx];
                    let g = params[*param_idx + 1];
                    let b = params[*param_idx + 2];
                    *param_idx += 3;
                    style.bg = Some(PackedRgba::rgb(r, g, b));
                }
            }
        }
        49 => style.bg = None,
        // Bright foreground colors
        c @ 90..=97 => style.fg = Some(ANSI_BRIGHT_PALETTE[(c - 90) as usize]),
        // Bright background colors
        c @ 100..=107 => style.bg = Some(ANSI_BRIGHT_PALETTE[(c - 100) as usize]),
        _ => {} // Ignore unsupported SGR codes
    }
}

/// Parse an ANSI-escaped string into a styled `ftui::text::Line`.
///
/// Converts SGR (Select Graphic Rendition) sequences into properly styled
/// `Span` objects, preserving colors, bold, italic, etc. Non-SGR escape
/// sequences are silently ignored.
#[must_use]
pub fn ansi_to_line(input: &str) -> ftui::text::Line<'static> {
    use ftui::text::{Line, Span};

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current_style = ftui::Style::default();
    let mut buf = String::new();

    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == 0x1b && i + 1 < len && bytes[i + 1] == b'[' {
            // Flush accumulated text
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), current_style));
            }
            // Parse CSI sequence: ESC [ <params> <final byte>
            i += 2; // skip ESC [
            let mut params_raw = Vec::new();
            while i < len && !(bytes[i] >= 0x40 && bytes[i] <= 0x7e) {
                params_raw.push(bytes[i]);
                i += 1;
            }
            if i < len {
                let final_byte = bytes[i];
                i += 1;
                if final_byte == b'm' {
                    // SGR sequence — parse semicolon-separated params
                    let params_str = String::from_utf8_lossy(&params_raw);
                    let params: Vec<u8> = if params_str.is_empty() {
                        vec![0] // ESC[m = reset
                    } else {
                        params_str
                            .split(';')
                            .map(|s| s.parse::<u8>().unwrap_or(0))
                            .collect()
                    };
                    let mut pi = 0;
                    while pi < params.len() {
                        let code = params[pi];
                        pi += 1;
                        apply_sgr_to_style(&mut current_style, code, &params, &mut pi);
                    }
                }
                // Non-SGR CSI sequences silently ignored
            }
        } else {
            // Regular character — accumulate into buffer
            // Handle multi-byte UTF-8 properly
            let c = input[i..].chars().next().unwrap_or('?');
            buf.push(c);
            i += c.len_utf8();
        }
    }

    // Flush remaining text
    if !buf.is_empty() {
        spans.push(Span::styled(buf, current_style));
    }

    Line::from_spans(spans)
}

/// Parse an ANSI-escaped multi-line string into a styled `ftui::text::Text`.
pub fn ansi_to_text(input: &str) -> ftui::text::Text<'static> {
    let lines: Vec<ftui::text::Line> = input.split('\n').map(ansi_to_line).collect();
    ftui::text::Text::from_lines(lines)
}

/// Colorize a JSON line with key/number highlights.
fn colorize_json_line(line: &str) -> String {
    let key_color = theme::json_key();
    let str_color = theme::json_string();
    let num_color = theme::json_number();

    let mut out = String::with_capacity(line.len() + 40);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            // Read until closing quote
            let mut s = String::with_capacity(32);
            for inner in chars.by_ref() {
                if inner == '"' {
                    break;
                }
                s.push(inner);
            }
            // Check if followed by ':'  -> key, else -> string value
            let is_key = chars.peek() == Some(&':');
            out.push_str(if is_key { &key_color } else { &str_color });
            out.push('"');
            out.push_str(&s);
            out.push('"');
            out.push_str(RESET);
        } else if c.is_ascii_digit() || c == '-' {
            let mut num = String::with_capacity(16);
            num.push(c);
            while let Some(&next) = chars.peek() {
                if next.is_ascii_digit() || next == '.' {
                    num.push(chars.next().unwrap());
                } else {
                    break;
                }
            }
            out.push_str(&num_color);
            out.push_str(&num);
            out.push_str(RESET);
        } else {
            out.push(c);
        }
    }
    out
}

// ──────────────────────────────────────────────────────────────────────
// Console Capabilities (br-1m6a.23)
// ──────────────────────────────────────────────────────────────────────

/// Snapshot of terminal capabilities relevant to the MCP Agent Mail console.
///
/// Computed once at startup from `ftui::TerminalCapabilities::detect()`.
/// Provides a stable, grep-friendly one-liner for debugging and E2E tests.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct ConsoleCaps {
    pub true_color: bool,
    pub osc8_hyperlinks: bool,
    pub mouse_sgr: bool,
    pub sync_output: bool,
    pub kitty_keyboard: bool,
    pub focus_events: bool,
    pub in_mux: bool,
}

impl ConsoleCaps {
    /// Build from detected `TerminalCapabilities`.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn from_capabilities(caps: &ftui::TerminalCapabilities) -> Self {
        Self {
            true_color: caps.true_color,
            osc8_hyperlinks: caps.osc8_hyperlinks,
            mouse_sgr: caps.mouse_sgr,
            sync_output: caps.sync_output,
            kitty_keyboard: caps.kitty_keyboard,
            focus_events: caps.focus_events,
            in_mux: caps.in_any_mux(),
        }
    }

    /// Stable, grep-friendly one-liner for tests and debugging.
    ///
    /// Format: `ConsoleCaps: tc=1 osc8=1 mouse=0 sync=1 kitty=0 focus=0 mux=0`
    #[must_use]
    pub fn one_liner(&self) -> String {
        format!(
            "ConsoleCaps: tc={} osc8={} mouse={} sync={} kitty={} focus={} mux={}",
            u8::from(self.true_color),
            u8::from(self.osc8_hyperlinks),
            u8::from(self.mouse_sgr),
            u8::from(self.sync_output),
            u8::from(self.kitty_keyboard),
            u8::from(self.focus_events),
            u8::from(self.in_mux),
        )
    }

    /// Render a styled "Console Capabilities" section for the startup banner.
    ///
    /// Returns a list of lines (with ANSI color) for inclusion in the banner.
    #[must_use]
    pub fn banner_lines(&self) -> Vec<String> {
        let pri = theme::primary_bold();
        let ok = theme::success_bold();
        let warn = theme::warning_bold();
        let mut lines = Vec::with_capacity(10);
        lines.push(format!("{pri}Console Capabilities{RESET}"));

        let check = |enabled: bool| -> (&str, &str) {
            if enabled {
                ("\u{2713}", ok.as_str())
            } else {
                ("\u{2717}", warn.as_str())
            }
        };

        let items: &[(&str, bool)] = &[
            ("True color", self.true_color),
            ("OSC-8 hyperlinks", self.osc8_hyperlinks),
            ("Mouse (SGR)", self.mouse_sgr),
            ("Sync output", self.sync_output),
            ("Kitty keyboard", self.kitty_keyboard),
            ("Focus events", self.focus_events),
        ];

        for (label, enabled) in items {
            let (sym, color) = check(*enabled);
            lines.push(format!("  {color}{sym}{RESET} {label}"));
        }

        if self.in_mux {
            lines.push(format!(
                "  {warn}\u{26a0}{RESET} Running inside a multiplexer"
            ));
        }

        lines
    }

    /// Short capabilities hint for the help overlay.
    #[must_use]
    pub fn help_hint(&self) -> String {
        let caps: Vec<&str> = [
            ("tc", self.true_color),
            ("osc8", self.osc8_hyperlinks),
            ("mouse", self.mouse_sgr),
            ("sync", self.sync_output),
            ("kitty", self.kitty_keyboard),
            ("focus", self.focus_events),
        ]
        .iter()
        .filter(|(_, enabled)| *enabled)
        .map(|(name, _)| *name)
        .collect();

        if caps.is_empty() {
            "Caps: none".to_string()
        } else {
            format!("Caps: {}", caps.join(", "))
        }
    }
}

/// Format a URL as an OSC-8 terminal hyperlink when the terminal supports it,
/// otherwise return just the plain-text label + URL.
///
/// When `osc8` is `true`:  `\x1b]8;;URL\x07LABEL\x1b]8;;\x07`
/// When `osc8` is `false`: `LABEL (URL)` — always includes the raw URL so it
/// remains visible and copy-pasteable in terminals that strip sequences.
#[must_use]
pub fn format_hyperlink(url: &str, label: &str, osc8: bool) -> String {
    if osc8 {
        format!("\x1b]8;;{url}\x07{label}\x1b]8;;\x07")
    } else {
        format!("{label} ({url})")
    }
}

// ──────────────────────────────────────────────────────────────────────
// Log Pane (br-1m6a.20): AltScreen LogViewer wrapper
// ──────────────────────────────────────────────────────────────────────

use ftui::layout::{Constraint, Flex, Rect};
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::input::TextInput;
use ftui::widgets::log_viewer::{LogViewer, LogViewerState, LogWrapMode};

/// Maximum log lines retained in the ring buffer.
const LOG_PANE_MAX_LINES: usize = 5_000;

/// Input focus mode for the log pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogPaneMode {
    /// Normal scrolling/navigation mode.
    Normal,
    /// Search input mode (`/` to enter, Enter to confirm, Escape to cancel).
    Search,
    /// Help overlay visible (`?` to toggle).
    Help,
}

/// Wrapper around `ftui::LogViewer` for the right-side log pane.
pub struct LogPane {
    viewer: LogViewer,
    state: LogViewerState,
    mode: LogPaneMode,
    search_input: TextInput,
    /// Optional capabilities addendum appended to the `?` help overlay.
    caps_addendum: String,
}

impl LogPane {
    #[must_use]
    pub fn new() -> Self {
        Self {
            viewer: LogViewer::new(LOG_PANE_MAX_LINES).wrap_mode(LogWrapMode::CharWrap),
            state: LogViewerState::default(),
            mode: LogPaneMode::Normal,
            search_input: TextInput::new().with_placeholder("Search..."),
            caps_addendum: String::new(),
        }
    }

    /// Set the capabilities addendum shown in the `?` help overlay.
    pub fn set_caps_addendum(&mut self, addendum: String) {
        self.caps_addendum = addendum;
    }

    /// Current input mode.
    pub const fn mode(&self) -> LogPaneMode {
        self.mode
    }

    /// Enter search input mode.
    pub fn enter_search_mode(&mut self) {
        self.mode = LogPaneMode::Search;
        self.search_input.clear();
        self.search_input.set_focused(true);
    }

    /// Confirm search and return to normal mode.
    pub fn confirm_search(&mut self) {
        let query = self.search_input.value().to_string();
        self.mode = LogPaneMode::Normal;
        self.search_input.set_focused(false);
        if query.is_empty() {
            self.viewer.clear_search();
        } else {
            self.viewer.search(&query);
        }
    }

    /// Cancel search input and return to normal mode.
    pub fn cancel_search(&mut self) {
        self.mode = LogPaneMode::Normal;
        self.search_input.set_focused(false);
    }

    /// Toggle help overlay.
    pub fn toggle_help(&mut self) {
        self.mode = if self.mode == LogPaneMode::Help {
            LogPaneMode::Normal
        } else {
            LogPaneMode::Help
        };
    }

    /// Handle a key event in search mode. Returns true if consumed.
    pub fn handle_search_event(&mut self, event: &ftui::Event) -> bool {
        self.search_input.handle_event(event)
    }

    /// Append a log line (plain text or ANSI-stripped).
    pub fn push<'a>(&mut self, line: impl Into<ftui::text::Text<'a>>) {
        self.viewer.push(line);
    }

    /// Append multiple lines efficiently.
    pub fn push_many<'a>(
        &mut self,
        lines: impl IntoIterator<Item = impl Into<ftui::text::Text<'a>>>,
    ) {
        self.viewer.push_many(lines);
    }

    /// Total lines in buffer.
    pub fn len(&self) -> usize {
        self.viewer.len()
    }

    /// Whether buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.viewer.is_empty()
    }

    /// Scroll up by N lines.
    pub fn scroll_up(&mut self, n: usize) {
        self.viewer.scroll_up(n);
    }

    /// Scroll down by N lines.
    pub fn scroll_down(&mut self, n: usize) {
        self.viewer.scroll_down(n);
    }

    /// Jump to top.
    pub fn scroll_to_top(&mut self) {
        self.viewer.scroll_to_top();
    }

    /// Jump to bottom and re-enable follow mode.
    pub fn scroll_to_bottom(&mut self) {
        self.viewer.scroll_to_bottom();
    }

    /// Page up by viewport height.
    pub fn page_up(&mut self) {
        self.viewer.page_up(&self.state);
    }

    /// Page down by viewport height.
    pub fn page_down(&mut self) {
        self.viewer.page_down(&self.state);
    }

    /// Toggle follow (auto-scroll) mode.
    pub fn toggle_follow(&mut self) {
        self.viewer.toggle_follow();
    }

    /// Whether auto-scroll is active.
    pub fn auto_scroll_enabled(&self) -> bool {
        self.viewer.auto_scroll_enabled()
    }

    /// Start a text search, return match count.
    pub fn search(&mut self, query: &str) -> usize {
        self.viewer.search(query)
    }

    /// Jump to next search match.
    pub fn next_match(&mut self) {
        self.viewer.next_match();
    }

    /// Jump to previous search match.
    pub fn prev_match(&mut self) {
        self.viewer.prev_match();
    }

    /// Clear active search.
    pub fn clear_search(&mut self) {
        self.viewer.clear_search();
    }

    /// Current search info: (1-indexed current, total).
    pub fn search_info(&self) -> Option<(usize, usize)> {
        self.viewer.search_info()
    }

    /// Set or clear a filter pattern.
    pub fn set_filter(&mut self, pattern: Option<&str>) {
        self.viewer.set_filter(pattern);
    }

    /// Clear all lines.
    pub fn clear(&mut self) {
        self.viewer.clear();
    }

    /// Render into the given area on a frame.
    pub fn render(&mut self, area: Rect, frame: &mut ftui::Frame<'_>) {
        self.viewer.render(area, frame, &mut self.state);
    }
}

impl Default for LogPane {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the column widths for a left-split layout.
///
/// Returns `(left_width, right_width)` for the HUD and log pane respectively.
/// If the total width is too small, returns `None` (caller should fall back to inline).
#[must_use]
pub fn split_columns(total_width: u16, ratio_percent: u16) -> Option<(u16, u16)> {
    if total_width < 60 {
        return None;
    }
    let ratio = ratio_percent.clamp(10, 80);
    // The product of two u16 values divided by 100 always fits in u16.
    #[allow(clippy::cast_possible_truncation)]
    let left = (u32::from(total_width) * u32::from(ratio) / 100) as u16;
    let left = left.max(30).min(total_width.saturating_sub(30));
    let right = total_width.saturating_sub(left);
    if right < 20 {
        return None;
    }
    Some((left, right))
}

/// Help text for the log pane keybindings + discoverability hints.
const LOG_PANE_HELP: &str = "\
 /         Search
 n / N     Next / prev match
 Escape    Cancel search / close help
 f         Toggle follow mode
 Up/Down   Scroll 1 line
 PgUp/PgDn Scroll 1 page
 Home/End  Jump to top / bottom
 ?         Toggle this help
 Ctrl+P    Command palette
";

/// Extended help text appended when `ConsoleCaps` is available.
///
/// The `caps_help_lines` method on `ConsoleCaps` returns a short
/// addendum showing which terminal capabilities are active.
impl ConsoleCaps {
    /// Render a compact help-overlay addendum showing active capabilities and
    /// key-discovery hints.
    ///
    /// Returns lines suitable for appending below `LOG_PANE_HELP` in the `?`
    /// overlay.  The output is plain ASCII (no ANSI escapes) so it renders
    /// cleanly in any terminal.
    #[must_use]
    pub fn help_overlay_addendum(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(128);
        out.push_str(" -- Capabilities --\n");
        let items: &[(&str, bool)] = &[
            ("True color", self.true_color),
            ("OSC-8 links", self.osc8_hyperlinks),
            ("Mouse (SGR)", self.mouse_sgr),
            ("Sync output", self.sync_output),
            ("Kitty kbd", self.kitty_keyboard),
            ("Focus evts", self.focus_events),
        ];
        for (label, enabled) in items {
            let sym = if *enabled { '+' } else { '-' };
            let _ = writeln!(out, "  {sym} {label}");
        }
        if self.in_mux {
            let _ = writeln!(out, "  ! In multiplexer");
        }
        out
    }
}

/// Render a two-pane split frame: HUD on the left, `LogViewer` on the right.
///
/// `render_hud_fn` is a closure that renders the existing HUD into a given area.
/// This keeps the dashboard rendering logic in lib.rs while letting console.rs
/// own the split layout and log pane rendering.
pub fn render_split_frame(
    frame: &mut ftui::Frame<'_>,
    area: Rect,
    ratio_percent: u16,
    log_pane: &mut LogPane,
    render_hud_fn: impl FnOnce(&mut ftui::Frame<'_>, Rect),
) {
    let Some((left_w, _right_w)) = split_columns(area.width, ratio_percent) else {
        // Too narrow for split — fall back to full-width HUD.
        render_hud_fn(frame, area);
        return;
    };

    let cols = Flex::horizontal()
        .constraints([Constraint::Fixed(left_w), Constraint::Fill])
        .split(area);

    // Left: existing HUD dashboard.
    render_hud_fn(frame, cols[0]);

    // Right: log viewer with a border.
    let follow_indicator = if log_pane.auto_scroll_enabled() {
        " Follow "
    } else {
        " Paused "
    };

    let search_indicator = log_pane
        .search_info()
        .map(|(cur, total)| format!(" {cur}/{total} "));

    let mut title = String::from(" Logs ");
    if let Some(ref si) = search_indicator {
        title.push_str(si);
    }
    title.push_str(follow_indicator);

    let log_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(&title);
    let inner = log_block.inner(cols[1]);
    log_block.render(cols[1], frame);

    match log_pane.mode() {
        LogPaneMode::Normal => {
            log_pane.render(inner, frame);
        }
        LogPaneMode::Search => {
            // Split inner: log viewer on top, search bar at bottom (1 row).
            if inner.height > 2 {
                let rows = Flex::vertical()
                    .constraints([Constraint::Fill, Constraint::Fixed(1)])
                    .split(inner);
                log_pane.render(rows[0], frame);
                // Render search input bar.
                log_pane.search_input.render(rows[1], frame);
            } else {
                // Too short for search bar — just render the input.
                log_pane.search_input.render(inner, frame);
            }
        }
        LogPaneMode::Help => {
            // Help overlay: keybindings + capabilities addendum (br-1m6a.23).
            use ftui::widgets::paragraph::Paragraph;
            let mut full_help = String::from(LOG_PANE_HELP);
            if !log_pane.caps_addendum.is_empty() {
                full_help.push_str(&log_pane.caps_addendum);
            }
            let help = Paragraph::new(full_help.clone());
            let help_block = Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Log Pane Help ");
            let help_widget = help.block(help_block);
            // Center the help box within the inner area.
            #[allow(clippy::cast_possible_truncation)] // help text is always small
            let h = full_help.lines().count() as u16 + 2; // +2 for borders
            let w = 40u16.min(inner.width);
            let x = inner.x + inner.width.saturating_sub(w) / 2;
            let y = inner.y + inner.height.saturating_sub(h) / 2;
            let help_area = Rect::new(x, y, w, h.min(inner.height));
            // Render logs behind the overlay first.
            log_pane.render(inner, frame);
            help_widget.render(help_area, frame);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Event Timeline (br-1m6a.22): structured event stream viewer (AltScreen)
// ──────────────────────────────────────────────────────────────────────

use std::collections::VecDeque;

use chrono::SecondsFormat;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table};

/// Maximum events retained in the timeline ring buffer.
pub const TIMELINE_MAX_EVENTS: usize = 500;

/// Which view is shown in the right pane of split mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightPaneView {
    /// Log viewer (default).
    Log,
    /// Structured event timeline.
    Timeline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleEventSeverity {
    Info,
    Warn,
    Error,
}

impl ConsoleEventSeverity {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    fn fg(self) -> ftui::PackedRgba {
        use ftui_extras::theme as ftui_theme;
        match self {
            Self::Info => ftui_theme::fg::PRIMARY.resolve(),
            Self::Warn => ftui_theme::accent::WARNING.resolve(),
            Self::Error => ftui_theme::accent::ERROR.resolve(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleEventKind {
    ToolCallStart,
    ToolCallEnd,
    HttpRequest,
}

impl ConsoleEventKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ToolCallStart => "tool_start",
            Self::ToolCallEnd => "tool_end",
            Self::HttpRequest => "http",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConsoleEvent {
    pub id: u64,
    pub ts_iso: String,
    pub kind: ConsoleEventKind,
    pub severity: ConsoleEventSeverity,
    pub summary: String,
    pub fields: Vec<(String, String)>,
    pub json: Option<Value>,
}

/// Bounded ring buffer of structured console events.
pub struct ConsoleEventBuffer {
    events: VecDeque<ConsoleEvent>,
    next_id: u64,
}

impl ConsoleEventBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: VecDeque::with_capacity(TIMELINE_MAX_EVENTS),
            next_id: 1,
        }
    }

    pub fn push(
        &mut self,
        kind: ConsoleEventKind,
        severity: ConsoleEventSeverity,
        summary: impl Into<String>,
        fields: Vec<(String, String)>,
        json: Option<Value>,
    ) -> u64 {
        let id = self.next_id;
        let ts_iso = chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        self.next_id = self.next_id.saturating_add(1);
        if self.events.len() >= TIMELINE_MAX_EVENTS {
            let _ = self.events.pop_front();
        }
        self.events.push_back(ConsoleEvent {
            id,
            ts_iso,
            kind,
            severity,
            summary: summary.into(),
            fields,
            json,
        });
        id
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<ConsoleEvent> {
        self.events.iter().cloned().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl Default for ConsoleEventBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Input focus mode for the timeline pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelinePaneMode {
    Normal,
    Search,
    Help,
}

/// Timeline viewer state (selection, filters, search, follow).
pub struct TimelinePane {
    mode: TimelinePaneMode,
    follow: bool,
    show_details: bool,
    selected_id: Option<u64>,
    scroll_offset: usize,
    viewport_height: usize,
    filter_severity: Option<ConsoleEventSeverity>,
    filter_kind: Option<ConsoleEventKind>,
    query: String,
    search_input: TextInput,
}

impl TimelinePane {
    #[must_use]
    pub fn new() -> Self {
        Self {
            mode: TimelinePaneMode::Normal,
            follow: true,
            show_details: true,
            selected_id: None,
            scroll_offset: 0,
            viewport_height: 12,
            filter_severity: None,
            filter_kind: None,
            query: String::new(),
            search_input: TextInput::new().with_placeholder("Search events..."),
        }
    }

    pub const fn mode(&self) -> TimelinePaneMode {
        self.mode
    }

    pub const fn follow_enabled(&self) -> bool {
        self.follow
    }

    pub const fn filter_severity(&self) -> Option<ConsoleEventSeverity> {
        self.filter_severity
    }

    pub const fn filter_kind(&self) -> Option<ConsoleEventKind> {
        self.filter_kind
    }

    pub const fn on_event_pushed(&mut self, new_id: u64) {
        if self.follow {
            self.selected_id = Some(new_id);
        }
    }

    pub fn toggle_help(&mut self) {
        self.mode = if self.mode == TimelinePaneMode::Help {
            TimelinePaneMode::Normal
        } else {
            TimelinePaneMode::Help
        };
    }

    pub fn enter_search_mode(&mut self) {
        self.mode = TimelinePaneMode::Search;
        self.search_input.clear();
        self.search_input.set_focused(true);
    }

    pub fn confirm_search(&mut self) {
        self.query = self.search_input.value().to_string();
        self.mode = TimelinePaneMode::Normal;
        self.search_input.set_focused(false);
        self.scroll_offset = 0;
    }

    pub fn cancel_search(&mut self) {
        self.mode = TimelinePaneMode::Normal;
        self.search_input.set_focused(false);
    }

    pub const fn toggle_follow(&mut self) {
        self.follow = !self.follow;
    }

    pub const fn toggle_details(&mut self) {
        self.show_details = !self.show_details;
    }

    pub const fn cycle_severity_filter(&mut self) {
        self.filter_severity = match self.filter_severity {
            None => Some(ConsoleEventSeverity::Info),
            Some(ConsoleEventSeverity::Info) => Some(ConsoleEventSeverity::Warn),
            Some(ConsoleEventSeverity::Warn) => Some(ConsoleEventSeverity::Error),
            Some(ConsoleEventSeverity::Error) => None,
        };
        self.scroll_offset = 0;
    }

    pub const fn cycle_kind_filter(&mut self) {
        self.filter_kind = match self.filter_kind {
            None => Some(ConsoleEventKind::ToolCallStart),
            Some(ConsoleEventKind::ToolCallStart) => Some(ConsoleEventKind::ToolCallEnd),
            Some(ConsoleEventKind::ToolCallEnd) => Some(ConsoleEventKind::HttpRequest),
            Some(ConsoleEventKind::HttpRequest) => None,
        };
        self.scroll_offset = 0;
    }

    fn matches_event(&self, event: &ConsoleEvent) -> bool {
        if let Some(sev) = self.filter_severity
            && event.severity != sev
        {
            return false;
        }

        if let Some(kind) = self.filter_kind
            && event.kind != kind
        {
            return false;
        }

        if !self.query.is_empty() {
            let q = self.query.to_ascii_lowercase();
            if !event.summary.to_ascii_lowercase().contains(&q) {
                return false;
            }
        }

        true
    }

    fn visible_indices(&self, events: &[ConsoleEvent]) -> Vec<usize> {
        let mut idx = Vec::with_capacity(events.len());
        for (i, ev) in events.iter().enumerate() {
            if self.matches_event(ev) {
                idx.push(i);
            }
        }
        idx
    }

    fn resolve_selected_visible_index(
        &mut self,
        events: &[ConsoleEvent],
        visible: &[usize],
    ) -> Option<usize> {
        if visible.is_empty() {
            self.selected_id = None;
            self.scroll_offset = 0;
            return None;
        }

        let mut selected = self
            .selected_id
            .and_then(|id| visible.iter().position(|&i| events[i].id == id));

        if selected.is_none() {
            let idx = if self.follow {
                visible.len().saturating_sub(1)
            } else {
                0
            };
            selected = Some(idx);
            self.selected_id = Some(events[visible[idx]].id);
        }

        selected
    }

    const fn ensure_selection_visible(&mut self, selected: usize) {
        if self.viewport_height == 0 {
            return;
        }
        if selected < self.scroll_offset {
            self.scroll_offset = selected;
            return;
        }
        let end = self
            .scroll_offset
            .saturating_add(self.viewport_height.saturating_sub(1));
        if selected > end {
            self.scroll_offset = selected.saturating_sub(self.viewport_height.saturating_sub(1));
        }
    }

    #[allow(clippy::cast_sign_loss)]
    fn move_selection(&mut self, delta: i32, events: &[ConsoleEvent], visible: &[usize]) {
        let Some(selected) = self.resolve_selected_visible_index(events, visible) else {
            return;
        };

        let next = if delta.is_negative() {
            selected.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            selected.saturating_add(delta as usize)
        }
        .min(visible.len().saturating_sub(1));

        self.selected_id = Some(events[visible[next]].id);
        self.ensure_selection_visible(next);
    }

    /// Handle keybindings for the timeline pane. Returns true if consumed.
    pub fn handle_key(
        &mut self,
        code: ftui::KeyCode,
        event: &ftui::Event,
        events: &[ConsoleEvent],
    ) -> bool {
        use ftui::KeyCode;

        match self.mode {
            TimelinePaneMode::Search => match code {
                KeyCode::Enter => {
                    self.confirm_search();
                    true
                }
                KeyCode::Escape => {
                    self.cancel_search();
                    true
                }
                _ => {
                    self.search_input.handle_event(event);
                    true
                }
            },
            TimelinePaneMode::Help => {
                self.toggle_help();
                true
            }
            TimelinePaneMode::Normal => {
                let visible = self.visible_indices(events);
                match code {
                    KeyCode::Char('/') => {
                        self.enter_search_mode();
                        true
                    }
                    KeyCode::Char('?') => {
                        self.toggle_help();
                        true
                    }
                    KeyCode::Char('F') => {
                        self.toggle_follow();
                        true
                    }
                    KeyCode::Char('f') => {
                        self.cycle_severity_filter();
                        true
                    }
                    KeyCode::Char('k') => {
                        self.cycle_kind_filter();
                        true
                    }
                    KeyCode::Enter => {
                        self.toggle_details();
                        true
                    }
                    KeyCode::Up => {
                        self.move_selection(-1, events, &visible);
                        true
                    }
                    KeyCode::Down => {
                        self.move_selection(1, events, &visible);
                        true
                    }
                    KeyCode::PageUp => {
                        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                        let step = self.viewport_height.max(1).min(i32::MAX as usize) as i32;
                        self.move_selection(-step, events, &visible);
                        true
                    }
                    KeyCode::PageDown => {
                        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                        let step = self.viewport_height.max(1).min(i32::MAX as usize) as i32;
                        self.move_selection(step, events, &visible);
                        true
                    }
                    KeyCode::Home => {
                        if !visible.is_empty() {
                            self.selected_id = Some(events[visible[0]].id);
                            self.scroll_offset = 0;
                        }
                        true
                    }
                    KeyCode::End => {
                        if !visible.is_empty() {
                            let last = visible.len().saturating_sub(1);
                            self.selected_id = Some(events[visible[last]].id);
                            self.scroll_offset =
                                last.saturating_sub(self.viewport_height.saturating_sub(1));
                        }
                        true
                    }
                    _ => false,
                }
            }
        }
    }

    #[allow(clippy::unused_self)]
    fn render_help_overlay(&self, inner: Rect, frame: &mut ftui::Frame<'_>) {
        const HELP: &str = "\
 /        Search\n\
 f        Cycle severity filter\n\
 k        Cycle kind filter\n\
 F        Toggle follow\n\
 Up/Down  Select event\n\
 PgUp/Dn  Page\n\
 Home/End Oldest/newest\n\
 Enter    Toggle details\n\
 ?        Toggle help\n\
 Esc      Cancel search / close help";
        let help_block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" Timeline Help ");
        let help_widget = Paragraph::new(HELP).block(help_block);
        #[allow(clippy::cast_possible_truncation)]
        let h = HELP.lines().count() as u16 + 2;
        let w = 48u16.min(inner.width);
        let x = inner.x + inner.width.saturating_sub(w) / 2;
        let y = inner.y + inner.height.saturating_sub(h) / 2;
        let area = Rect::new(x, y, w, h.min(inner.height));
        help_widget.render(area, frame);
    }

    /// Render timeline contents (list + details) into the inner area.
    #[allow(clippy::too_many_lines)]
    pub fn render(&mut self, inner: Rect, frame: &mut ftui::Frame<'_>, events: &[ConsoleEvent]) {
        if inner.is_empty() {
            return;
        }

        // Track viewport height for paging behavior.
        self.viewport_height = usize::from(inner.height.saturating_sub(2)).max(1);

        let mut list_area = inner;
        let mut details_area = Rect::default();

        // In search mode, reserve 1 row for the input bar.
        let search_bar_row = if self.mode == TimelinePaneMode::Search && inner.height > 2 {
            let rows = Flex::vertical()
                .constraints([Constraint::Fill, Constraint::Fixed(1)])
                .split(inner);
            list_area = rows[0];
            rows[1]
        } else {
            Rect::default()
        };

        if self.show_details && list_area.height > 8 {
            let rows = Flex::vertical()
                .constraints([Constraint::Percentage(55.0), Constraint::Fill])
                .split(list_area);
            list_area = rows[0];
            details_area = rows[1];
        }

        let visible = self.visible_indices(events);
        let selected_visible = self.resolve_selected_visible_index(events, &visible);
        if let Some(sel) = selected_visible {
            self.ensure_selection_visible(sel);
        }

        // Render list table.
        let header_style = ftui::Style::default()
            .fg(ftui_extras::theme::fg::SECONDARY.resolve())
            .bold();
        let selected_bg = ftui::PackedRgba::rgb(35, 35, 35);

        let max_rows = usize::from(list_area.height.saturating_sub(2)).max(1);
        let start = self.scroll_offset.min(visible.len());
        let end = (start + max_rows).min(visible.len());
        let mut rows = Vec::with_capacity(end - start);
        for (pos, idx) in visible[start..end].iter().enumerate() {
            let ev = &events[*idx];
            let mut row = Row::new(vec![
                format!("#{:<4}", ev.id),
                ev.severity.label().to_string(),
                ev.kind.label().to_string(),
                compact_path(&ev.summary, 80),
            ])
            .style(ftui::Style::default().fg(ev.severity.fg()));

            if let Some(sel) = selected_visible
                && (start + pos) == sel
            {
                row = row.style(
                    ftui::Style::default()
                        .fg(ev.severity.fg())
                        .bg(selected_bg)
                        .bold(),
                );
            }
            rows.push(row);
        }

        let timeline_table = Table::new(
            rows,
            [
                Constraint::Fixed(7),
                Constraint::Fixed(5),
                Constraint::Fixed(10),
                Constraint::Fill,
            ],
        )
        .header(Row::new(vec!["ID", "SEV", "KIND", "SUMMARY"]).style(header_style))
        .column_spacing(1)
        .style(ftui::Style::default().fg(ftui_extras::theme::fg::PRIMARY.resolve()));
        <Table as Widget>::render(&timeline_table, list_area, frame);

        // Render details panel.
        if self.show_details && !details_area.is_empty() {
            let details_block = Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Details ");
            let details_inner = details_block.inner(details_area);
            details_block.render(details_area, frame);

            if let Some(sel) = selected_visible {
                let ev = &events[visible[sel]];
                let mut lines = Vec::new();
                lines.push(format!(
                    "#{}  {}  {}  {}",
                    ev.id,
                    ev.ts_iso,
                    ev.severity.label(),
                    ev.kind.label()
                ));
                lines.push(ev.summary.clone());
                if !ev.fields.is_empty() {
                    lines.push(String::new());
                    for (k, v) in &ev.fields {
                        lines.push(format!("{k}: {v}"));
                    }
                }
                if let Some(ref json) = ev.json
                    && let Ok(pretty) = serde_json::to_string_pretty(json)
                {
                    lines.push(String::new());
                    for line in pretty.lines().take(12) {
                        lines.push(compact_path(line, 120));
                    }
                }
                Paragraph::new(lines.join("\n"))
                    .wrap(ftui::text::WrapMode::Word)
                    .render(details_inner, frame);
            } else {
                Paragraph::new("No events yet.")
                    .style(ftui::Style::default().fg(ftui_extras::theme::fg::MUTED.resolve()))
                    .render(details_inner, frame);
            }
        }

        // Search bar (if active).
        if !search_bar_row.is_empty() && self.mode == TimelinePaneMode::Search {
            self.search_input.render(search_bar_row, frame);
        }

        // Help overlay (if active).
        if self.mode == TimelinePaneMode::Help {
            self.render_help_overlay(inner, frame);
        }
    }
}

impl Default for TimelinePane {
    fn default() -> Self {
        Self::new()
    }
}

/// Render a two-pane split frame: HUD left, event timeline right.
pub fn render_split_frame_timeline(
    frame: &mut ftui::Frame<'_>,
    area: Rect,
    ratio_percent: u16,
    timeline: &mut TimelinePane,
    events: &[ConsoleEvent],
    render_hud_fn: impl FnOnce(&mut ftui::Frame<'_>, Rect),
) {
    let Some((left_w, _right_w)) = split_columns(area.width, ratio_percent) else {
        render_hud_fn(frame, area);
        return;
    };

    let cols = Flex::horizontal()
        .constraints([Constraint::Fixed(left_w), Constraint::Fill])
        .split(area);

    render_hud_fn(frame, cols[0]);

    // Right: timeline with a border and small state indicators.
    let follow_indicator = if timeline.follow_enabled() {
        " Follow "
    } else {
        " Paused "
    };
    let sev = timeline
        .filter_severity()
        .map(|s| format!(" sev={} ", s.label()))
        .unwrap_or_default();

    let kind = timeline
        .filter_kind()
        .map(|k| format!(" kind={} ", k.label()))
        .unwrap_or_default();

    let title = format!(" Timeline{sev}{kind}{follow_indicator}");
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(&title);
    let inner = block.inner(cols[1]);
    block.render(cols[1], frame);

    timeline.render(inner, frame, events);
}

// ──────────────────────────────────────────────────────────────────────
// Command palette (br-1m6a.21)
// ──────────────────────────────────────────────────────────────────────

use ftui::widgets::command_palette::{ActionItem, CommandPalette, PaletteAction};

/// Action IDs for command palette entries.
pub mod action_ids {
    // Layout actions
    pub const MODE_INLINE: &str = "layout:mode_inline";
    pub const MODE_LEFT_SPLIT: &str = "layout:mode_left_split";
    pub const SPLIT_RATIO_20: &str = "layout:split_ratio_20";
    pub const SPLIT_RATIO_30: &str = "layout:split_ratio_30";
    pub const SPLIT_RATIO_40: &str = "layout:split_ratio_40";
    pub const SPLIT_RATIO_50: &str = "layout:split_ratio_50";
    pub const HUD_HEIGHT_INC: &str = "layout:hud_height_inc";
    pub const HUD_HEIGHT_DEC: &str = "layout:hud_height_dec";
    pub const ANCHOR_TOP: &str = "layout:anchor_top";
    pub const ANCHOR_BOTTOM: &str = "layout:anchor_bottom";
    pub const TOGGLE_AUTO_SIZE: &str = "layout:toggle_auto_size";
    pub const PERSIST_NOW: &str = "layout:persist_now";
    // Theme actions
    pub const THEME_CYCLE: &str = "theme:cycle";
    pub const THEME_CYBERPUNK: &str = "theme:cyberpunk_aurora";
    pub const THEME_DARCULA: &str = "theme:darcula";
    pub const THEME_LUMEN: &str = "theme:lumen_light";
    pub const THEME_NORDIC: &str = "theme:nordic_frost";
    pub const THEME_HIGH_CONTRAST: &str = "theme:high_contrast";
    // Log actions
    pub const LOG_TOGGLE_FOLLOW: &str = "logs:toggle_follow";
    pub const LOG_SEARCH: &str = "logs:search";
    pub const LOG_CLEAR: &str = "logs:clear";
    // Right pane view
    pub const RIGHT_PANE_TOGGLE: &str = "layout:right_pane_toggle";
    // Tool panel toggles
    pub const TOGGLE_TOOL_CALLS_LOG: &str = "tools:toggle_tool_calls_log";
    pub const TOGGLE_TOOLS_LOG: &str = "tools:toggle_tools_log";
    // Help
    pub const SHOW_KEYBINDINGS: &str = "help:keybindings";
    pub const SHOW_CONFIG: &str = "help:config_summary";
}

/// Build the ordered list of command palette actions.
#[must_use]
fn build_palette_actions() -> Vec<ActionItem> {
    use action_ids as id;
    vec![
        // Layout
        ActionItem::new(id::MODE_INLINE, "Switch to Inline Mode")
            .with_description("Use inline HUD with terminal scrollback")
            .with_tags(&["layout", "inline"])
            .with_category("Layout"),
        ActionItem::new(id::MODE_LEFT_SPLIT, "Switch to Left Split Mode")
            .with_description("AltScreen: HUD left, log viewer right")
            .with_tags(&["layout", "split", "altscreen"])
            .with_category("Layout"),
        ActionItem::new(id::SPLIT_RATIO_20, "Split Ratio 20%")
            .with_description("Set HUD width to 20%")
            .with_tags(&["layout", "ratio"])
            .with_category("Layout"),
        ActionItem::new(id::SPLIT_RATIO_30, "Split Ratio 30%")
            .with_description("Set HUD width to 30%")
            .with_tags(&["layout", "ratio"])
            .with_category("Layout"),
        ActionItem::new(id::SPLIT_RATIO_40, "Split Ratio 40%")
            .with_description("Set HUD width to 40%")
            .with_tags(&["layout", "ratio"])
            .with_category("Layout"),
        ActionItem::new(id::SPLIT_RATIO_50, "Split Ratio 50%")
            .with_description("Set HUD width to 50%")
            .with_tags(&["layout", "ratio"])
            .with_category("Layout"),
        ActionItem::new(id::HUD_HEIGHT_INC, "Increase HUD Height (+5%)")
            .with_description("Increase inline HUD height by 5%")
            .with_tags(&["layout", "height"])
            .with_category("Layout"),
        ActionItem::new(id::HUD_HEIGHT_DEC, "Decrease HUD Height (-5%)")
            .with_description("Decrease inline HUD height by 5%")
            .with_tags(&["layout", "height"])
            .with_category("Layout"),
        ActionItem::new(id::ANCHOR_TOP, "Anchor HUD to Top")
            .with_tags(&["layout", "anchor"])
            .with_category("Layout"),
        ActionItem::new(id::ANCHOR_BOTTOM, "Anchor HUD to Bottom")
            .with_tags(&["layout", "anchor"])
            .with_category("Layout"),
        ActionItem::new(id::TOGGLE_AUTO_SIZE, "Toggle Auto-Size")
            .with_description("Toggle inline auto-sizing (min/max rows)")
            .with_tags(&["layout", "auto"])
            .with_category("Layout"),
        ActionItem::new(id::PERSIST_NOW, "Save Console Settings")
            .with_description("Persist current CONSOLE_* settings to envfile")
            .with_tags(&["save", "persist"])
            .with_category("Layout"),
        ActionItem::new(id::RIGHT_PANE_TOGGLE, "Toggle Right Pane: Log/Timeline")
            .with_description("Switch right pane between log viewer and event timeline")
            .with_tags(&["layout", "timeline", "events", "log"])
            .with_category("Layout"),
        // Theme
        ActionItem::new(id::THEME_CYCLE, "Cycle Theme")
            .with_description("Switch to the next available theme")
            .with_tags(&["theme", "color"])
            .with_category("Theme"),
        ActionItem::new(id::THEME_CYBERPUNK, "Theme: Cyberpunk Aurora")
            .with_tags(&["theme"])
            .with_category("Theme"),
        ActionItem::new(id::THEME_DARCULA, "Theme: Darcula")
            .with_tags(&["theme"])
            .with_category("Theme"),
        ActionItem::new(id::THEME_LUMEN, "Theme: Lumen Light")
            .with_tags(&["theme"])
            .with_category("Theme"),
        ActionItem::new(id::THEME_NORDIC, "Theme: Nordic Frost")
            .with_tags(&["theme"])
            .with_category("Theme"),
        ActionItem::new(id::THEME_HIGH_CONTRAST, "Theme: High Contrast")
            .with_tags(&["theme"])
            .with_category("Theme"),
        // Logs
        ActionItem::new(id::LOG_TOGGLE_FOLLOW, "Toggle Follow Mode")
            .with_description("Toggle log auto-scroll (follow tail)")
            .with_tags(&["log", "follow", "tail"])
            .with_category("Logs"),
        ActionItem::new(id::LOG_SEARCH, "Search Logs")
            .with_description("Open log search (split mode only)")
            .with_tags(&["log", "search", "find"])
            .with_category("Logs"),
        ActionItem::new(id::LOG_CLEAR, "Clear Log Buffer")
            .with_description("Clear all log lines from the viewer")
            .with_tags(&["log", "clear"])
            .with_category("Logs"),
        // Tool panels
        ActionItem::new(id::TOGGLE_TOOL_CALLS_LOG, "Toggle Tool Calls Logging")
            .with_description("Toggle LOG_TOOL_CALLS_ENABLED at runtime")
            .with_tags(&["tools", "logging"])
            .with_category("Tools"),
        ActionItem::new(id::TOGGLE_TOOLS_LOG, "Toggle Tools Detail Logging")
            .with_description("Toggle TOOLS_LOG_ENABLED at runtime")
            .with_tags(&["tools", "logging"])
            .with_category("Tools"),
        // Help
        ActionItem::new(id::SHOW_KEYBINDINGS, "Show Keybindings")
            .with_description("Display keyboard shortcut reference")
            .with_tags(&["help", "keys"])
            .with_category("Help"),
        ActionItem::new(id::SHOW_CONFIG, "Show Current Config")
            .with_description("Display sanitized console configuration")
            .with_tags(&["help", "config", "status"])
            .with_category("Help"),
    ]
}

/// Wrapper around ftui `CommandPalette` with pre-registered console actions.
pub struct ConsoleCommandPalette {
    palette: CommandPalette,
}

impl ConsoleCommandPalette {
    /// Create a new command palette pre-populated with console actions.
    #[must_use]
    pub fn new() -> Self {
        let mut palette = CommandPalette::new().with_max_visible(10);
        for action in build_palette_actions() {
            palette.register_action(action);
        }
        Self { palette }
    }

    /// Open the palette (clears previous query).
    pub fn open(&mut self) {
        self.palette.open();
    }

    /// Close the palette.
    pub fn close(&mut self) {
        self.palette.close();
    }

    /// Toggle visibility.
    pub fn toggle(&mut self) {
        self.palette.toggle();
    }

    /// Whether the palette is currently visible.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.palette.is_visible()
    }

    /// Forward a key event to the palette. Returns a `PaletteAction` if the
    /// user executed or dismissed.
    pub fn handle_event(&mut self, event: &ftui::Event) -> Option<PaletteAction> {
        self.palette.handle_event(event)
    }

    /// Render the palette overlay onto the frame.
    pub fn render(&self, area: Rect, frame: &mut ftui::Frame<'_>) {
        self.palette.render(area, frame);
    }

    /// Number of registered actions.
    #[must_use]
    pub fn action_count(&self) -> usize {
        self.palette.action_count()
    }
}

impl Default for ConsoleCommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Masking tests ──

    #[test]
    fn test_is_sensitive_key_positive() {
        for key in &[
            "api_token",
            "SECRET_KEY",
            "password",
            "auth_header",
            "credential_file",
            "bearer_token",
            "HTTP_BEARER_TOKEN",
            "MyPassword",
            "secret",
            "api_key",
            "Authorization",
            "jwt_secret",
            "private_key",
        ] {
            assert!(is_sensitive_key(key), "expected sensitive: {key}");
        }
    }

    #[test]
    fn test_is_sensitive_key_negative() {
        for key in &[
            "project_key",
            "storage_root",
            "agent_name",
            "auth_enabled",
            "body_md",
            "subject",
            "to",
            "importance",
            "format",
            "limit",
            "endpoint",
            "app_environment",
        ] {
            assert!(!is_sensitive_key(key), "expected non-sensitive: {key}");
        }
    }

    #[test]
    fn test_mask_sensitive_value() {
        assert_eq!(mask_sensitive_value("abc123"), MASK_REDACTED);
    }

    #[test]
    fn test_mask_json_params() {
        let input = serde_json::json!({
            "project_key": "/data/backend",
            "storage_root": "/tmp/storage",
            "bearer_token": "secret-value",
            "nested": {
                "password": "hunter2",
                "name": "test",
                "api_key": 123
            },
            "list": [
                {"jwt_secret": true},
                {"not_sensitive": "ok"},
                {"auth_header": "Bearer xyz"}
            ],
            "database_url": "postgres://user:pass@localhost/db"
        });
        let masked = mask_json_params(&input);
        assert_eq!(masked["bearer_token"], MASK_REDACTED);
        assert_eq!(masked["nested"]["password"], MASK_REDACTED);
        assert_eq!(masked["nested"]["name"], "test");
        assert_eq!(masked["nested"]["api_key"], MASK_REDACTED);
        assert_eq!(masked["list"][0]["jwt_secret"], MASK_REDACTED);
        assert_eq!(masked["list"][1]["not_sensitive"], "ok");
        assert_eq!(masked["list"][2]["auth_header"], MASK_REDACTED);
        assert_eq!(
            masked["database_url"],
            "postgres://user:<redacted>@localhost/db"
        );
        assert_eq!(masked["project_key"], "/data/backend");
        assert_eq!(masked["storage_root"], "/tmp/storage");
    }

    #[test]
    fn test_mask_preserves_non_sensitive() {
        let input = serde_json::json!({
            "agent_name": "BlueLake",
            "subject": "Hello",
            "body_md": "content"
        });
        let masked = mask_json_params(&input);
        assert_eq!(masked["agent_name"], "BlueLake");
        assert_eq!(masked["subject"], "Hello");
        assert_eq!(masked["body_md"], "content");
    }

    #[test]
    fn test_sanitize_known_value_postgres_userinfo() {
        let url = "postgres://user:pass@localhost/db";
        let sanitized = sanitize_known_value("database_url", url).unwrap();
        assert_eq!(sanitized, "postgres://user:<redacted>@localhost/db");
    }

    #[test]
    fn test_sanitize_known_value_sqlite_no_change() {
        assert!(sanitize_known_value("database_url", "/tmp/test.db").is_none());
    }

    #[test]
    fn test_mask_json_non_string_sensitive_values_are_redacted() {
        let input = serde_json::json!({
            "api_key": 123,
            "jwt_secret": true,
            "token": null,
            "ok": false
        });
        let masked = mask_json_params(&input);
        assert_eq!(masked["api_key"], MASK_REDACTED);
        assert_eq!(masked["jwt_secret"], MASK_REDACTED);
        assert_eq!(masked["token"], MASK_REDACTED);
        assert_eq!(masked["ok"], false);
    }

    #[test]
    fn test_mask_json_empty_objects_and_arrays() {
        let input = serde_json::json!({
            "empty_obj": {},
            "empty_arr": []
        });
        let masked = mask_json_params(&input);
        assert_eq!(masked["empty_obj"], serde_json::json!({}));
        assert_eq!(masked["empty_arr"], serde_json::json!([]));
    }

    // ── Duration gradient tests ──

    #[test]
    fn test_duration_style_fast() {
        let ds = duration_style(10);
        assert!(
            ds.color.contains("38;2;"),
            "expected 24-bit color for fast: {}",
            ds.color
        );
        assert_eq!(ds.label, "10ms");
    }

    #[test]
    fn test_duration_style_medium() {
        let ds = duration_style(200);
        assert!(
            ds.color.contains("38;2;"),
            "expected 24-bit color for medium: {}",
            ds.color
        );
    }

    #[test]
    fn test_duration_style_slow() {
        let ds = duration_style(2000);
        assert!(
            ds.color.contains("38;2;"),
            "expected 24-bit color for slow: {}",
            ds.color
        );
        assert_eq!(ds.label, "2000ms");
    }

    // ── Banner tests ──

    #[test]
    fn test_banner_contains_logo() {
        let params = BannerParams {
            app_environment: "development",
            endpoint: "http://localhost:8765/mcp",
            database_url: "/tmp/test.db",
            storage_root: "/tmp/storage",
            auth_enabled: false,
            tools_log_enabled: true,
            tool_calls_log_enabled: true,
            console_theme: "Cyberpunk Aurora",
            web_ui_url: "http://localhost:8765/mail",
            projects: 3,
            agents: 5,
            messages: 42,
            file_reservations: 2,
            contact_links: 1,
        };
        let lines = render_startup_banner(&params);
        let joined = lines.join("\n");
        assert!(
            joined.contains("MCP Agent Mail"),
            "banner should contain title"
        );
        assert!(
            joined.contains("Endpoint:"),
            "banner should contain endpoint"
        );
        assert!(joined.contains("Web UI:"), "banner should contain web ui");
        assert!(
            joined.contains("Tool logs:"),
            "banner should contain tool logging state"
        );
        assert!(
            joined.contains("Tool panels:"),
            "banner should contain tool panel state"
        );
        assert!(
            joined.contains("Stats:"),
            "banner should contain stats summary"
        );
        assert!(
            joined.contains("Cyberpunk Aurora"),
            "banner should display active theme name"
        );
        assert!(joined.contains("Theme:"), "banner should contain theme row");
    }

    #[test]
    fn test_banner_no_rich_logging_when_disabled() {
        let params = BannerParams {
            app_environment: "production",
            endpoint: "http://localhost:8765/mcp",
            database_url: "/tmp/test.db",
            storage_root: "/tmp/storage",
            auth_enabled: true,
            tools_log_enabled: false,
            tool_calls_log_enabled: false,
            console_theme: "Darcula",
            web_ui_url: "http://localhost:8765/mail",
            projects: 0,
            agents: 0,
            messages: 0,
            file_reservations: 0,
            contact_links: 0,
        };
        let lines = render_startup_banner(&params);
        let joined = lines.join("\n");
        assert!(joined.contains("Tool logs:"));
        assert!(joined.contains("Tool panels:"));
        assert!(!joined.contains("Rich Logging ENABLED"));
    }

    // ── Tool call panel tests ──

    #[test]
    fn test_tool_call_start_contains_fields() {
        let params = serde_json::json!({"project_key": "/data/backend", "agent_name": "BlueLake"});
        let lines =
            render_tool_call_start("send_message", &params, Some("backend"), Some("BlueLake"));
        let joined = lines.join("\n");
        assert!(joined.contains("TOOL CALL"), "should have TOOL CALL header");
        assert!(joined.contains("send_message"), "should contain tool name");
        assert!(joined.contains("BlueLake"), "should contain agent name");
        assert!(
            joined.contains("Parameters:"),
            "should have parameters section"
        );
    }

    #[test]
    fn test_tool_call_start_masks_sensitive() {
        let params = serde_json::json!({"bearer_token": "secret123", "agent_name": "BlueLake"});
        let lines = render_tool_call_start("health_check", &params, None, None);
        let joined = lines.join("\n");
        assert!(
            !joined.contains("secret123"),
            "should not contain raw secret"
        );
        assert!(
            joined.contains(MASK_REDACTED),
            "should contain redaction placeholder"
        );
        assert!(
            joined.contains("BlueLake"),
            "non-sensitive value should appear"
        );
    }

    #[test]
    fn test_tool_call_end_fields() {
        let lines =
            render_tool_call_end("send_message", 42, Some("{\"id\": 1}"), 5, 12.0, &[], 2000);
        let joined = lines.join("\n");
        assert!(joined.contains("send_message"));
        assert!(joined.contains("42ms"));
        assert!(joined.contains("completed in"));
        assert!(joined.contains("Queries:"));
        assert!(joined.contains("Result:"));
    }

    #[test]
    fn test_tool_call_end_truncates_long_result() {
        let long_result = "x".repeat(600);
        let lines = render_tool_call_end("test_tool", 100, Some(&long_result), 1, 5.0, &[], 500);
        let joined = lines.join("\n");
        // Line-level clamping uses `…` (unicode ellipsis); char-budget uses `...(truncated)`
        assert!(
            joined.contains('…') || joined.contains("..."),
            "long result should be truncated"
        );
        assert!(
            !joined.contains(&"x".repeat(600)),
            "full result should not appear"
        );
    }

    #[test]
    fn test_tool_call_end_masks_sensitive_result_json() {
        let result = r#"{"bearer_token":"secret123","ok":true}"#;
        let lines = render_tool_call_end("test_tool", 10, Some(result), 0, 0.0, &[], 2000);
        let joined = lines.join("\n");
        assert!(!joined.contains("secret123"));
        assert!(joined.contains(MASK_REDACTED));
        assert!(joined.contains("Result:"));
    }

    #[test]
    fn test_tool_call_end_per_table_stats() {
        let per_table = vec![
            ("messages".to_string(), 15u64),
            ("projects".to_string(), 8),
            ("agents".to_string(), 5),
            ("file_reservations".to_string(), 3),
            ("contacts".to_string(), 2),
            ("acks".to_string(), 1),
        ];
        let lines = render_tool_call_end("send_message", 42, None, 34, 12.5, &per_table, 2000);
        let joined = lines.join("\n");
        // Top 5 tables shown
        assert!(joined.contains("messages"), "top table shown");
        assert!(joined.contains("projects"), "2nd table shown");
        assert!(joined.contains("agents"), "3rd table shown");
        assert!(joined.contains("file_reservations"), "4th table shown");
        assert!(joined.contains("contacts"), "5th table shown");
        // 6th table hidden behind "... and 1 more"
        assert!(!joined.contains("acks"), "6th table hidden");
        assert!(joined.contains("1 more"), "overflow indicator");
        // Total row
        assert!(joined.contains("34"), "total count shown");
        assert!(joined.contains("12.5ms"), "total time shown");
    }

    #[test]
    fn test_tool_call_end_empty_per_table() {
        let lines = render_tool_call_end("test_tool", 10, None, 0, 0.0, &[], 2000);
        let joined = lines.join("\n");
        // No table/count headers when per_table is empty
        assert!(!joined.contains("Table"), "no table header when empty");
        assert!(!joined.contains("Count"), "no count header when empty");
    }

    #[test]
    fn test_tool_call_lines_never_exceed_box_width() {
        // Regression: long JSON values caused terminal line wrapping and garbled borders
        let long_body = serde_json::json!({
            "body_md": "x".repeat(500),
            "subject": "y".repeat(200),
        });
        let start_lines = render_tool_call_start("send_message", &long_body, None, None);
        for (i, line) in start_lines.iter().enumerate() {
            let vis = strip_ansi_len(line);
            assert!(
                vis <= 80, // w=78 + 2 for border chars
                "start line {i} has visible width {vis}: {line}"
            );
        }

        let long_result = serde_json::to_string_pretty(&long_body).unwrap();
        let end_lines =
            render_tool_call_end("send_message", 42, Some(&long_result), 5, 12.0, &[], 2000);
        for (i, line) in end_lines.iter().enumerate() {
            let vis = strip_ansi_len(line);
            assert!(vis <= 80, "end line {i} has visible width {vis}: {line}");
        }
    }

    // ── Sparkline tests ──

    #[test]
    fn test_sparkline_buffer_sample() {
        let buf = SparklineBuffer::new();
        buf.tick();
        buf.tick();
        buf.tick();
        buf.sample();
        let data = buf.snapshot();
        let last = data.last().copied().unwrap();
        assert!(
            (last - 3.0).abs() < 0.0001,
            "expected last sample to be ~3, got {last}"
        );
    }

    #[test]
    fn test_sparkline_render_nonempty() {
        let buf = SparklineBuffer::new();
        for _ in 0..5 {
            buf.tick();
            buf.sample();
        }
        let rendered = buf.render_sparkline();
        assert!(!rendered.is_empty());
    }

    // ── Toast tests ──

    #[test]
    fn test_toast_formatting() {
        let toast = render_toast("\u{1f4e8}", "New message from BlueLake", ToastLevel::Info);
        assert!(toast.contains("New message from BlueLake"));
        assert!(toast.contains("\u{1f4e8}"));
    }

    #[test]
    fn test_toast_error_level() {
        let toast = render_toast("\u{274c}", "Conflict detected", ToastLevel::Error);
        assert!(
            toast.contains("38;2;"),
            "error toast should use 24-bit theme color: {toast}"
        );
    }

    // ── Log panel tests ──

    #[test]
    fn test_log_panel_info() {
        let lines = render_log_panel(
            ToastLevel::Info,
            "System Info",
            "Server started successfully",
        );
        let joined = lines.join("\n");
        assert!(joined.contains("System Info"));
        assert!(joined.contains("Server started successfully"));
        // Info uses rounded borders
        assert!(joined.contains('\u{256d}'));
    }

    #[test]
    fn test_log_panel_error() {
        let lines = render_log_panel(ToastLevel::Error, "Fatal Error", "Connection refused");
        let joined = lines.join("\n");
        assert!(joined.contains("Fatal Error"));
        assert!(joined.contains("Connection refused"));
        // Error uses heavy borders
        assert!(joined.contains('\u{250f}'));
    }

    // ── Helper tests ──

    #[test]
    fn test_strip_ansi_len() {
        assert_eq!(strip_ansi_len("hello"), 5);
        assert_eq!(strip_ansi_len("\x1b[1;32mhello\x1b[0m"), 5);
        assert_eq!(strip_ansi_len(""), 0);
    }

    #[test]
    fn test_compact_path() {
        assert_eq!(compact_path("short", 10), "short");
        assert_eq!(
            compact_path("/very/long/path/to/something", 15),
            "...to/something"
        );
    }

    #[test]
    fn test_colorize_json_line() {
        let line = r#"  "name": "test","#;
        let colored = colorize_json_line(line);
        // Should contain ANSI codes
        assert!(colored.contains('\x1b'));
        // The key "name" and value "test" should still be present
        let stripped = strip_ansi_content(&colored);
        assert!(stripped.contains("name"));
        assert!(stripped.contains("test"));
    }

    /// Strip ANSI codes from a string (for test assertions).
    fn strip_ansi_content(s: &str) -> String {
        let mut out = String::new();
        let mut in_escape = false;
        for c in s.chars() {
            if in_escape {
                if c.is_ascii_alphabetic() {
                    in_escape = false;
                }
            } else if c == '\x1b' {
                in_escape = true;
            } else {
                out.push(c);
            }
        }
        out
    }

    // ── Status/method style tests (br-1m6a.13) ──

    #[test]
    fn test_status_style_2xx() {
        let s = status_style(200);
        assert!(s.contains("38;2;"), "2xx should use 24-bit color: {s}");
    }

    #[test]
    fn test_status_style_differentiation() {
        let ok = status_style(200);
        let warn = status_style(404);
        let err = status_style(500);
        assert_ne!(ok, err, "success and error should differ");
        assert_ne!(warn, err, "warning and error should differ");
    }

    #[test]
    fn test_method_style_differentiation() {
        let get = method_style("GET");
        let post = method_style("POST");
        let del = method_style("DELETE");
        assert_ne!(get, del, "GET and DELETE should differ");
        assert_ne!(post, del, "POST and DELETE should differ");
    }

    #[test]
    fn test_request_panel_non_ansi() {
        let panel = render_http_request_panel(100, "GET", "/api", 200, 42, "127.0.0.1", false)
            .expect("panel should render");
        assert!(!panel.contains("\x1b["), "non-ANSI panel: no escapes");
        assert!(panel.contains('+'), "non-ANSI: + corners");
        assert!(panel.contains("GET"), "panel: method");
        assert!(panel.contains("42ms"), "panel: duration");
        assert!(panel.contains("client: 127.0.0.1"), "panel: client IP");
    }

    #[test]
    fn test_request_panel_ansi_uses_theme() {
        let panel = render_http_request_panel(100, "POST", "/mcp", 201, 5, "10.0.0.1", true)
            .expect("panel should render");
        assert!(panel.contains("38;2;"), "ANSI: 24-bit color");
        assert!(panel.contains('\u{256d}'), "ANSI: rounded corner");
    }

    #[test]
    fn test_request_panel_tiny_width() {
        assert!(render_http_request_panel(0, "GET", "/", 200, 1, "x", false).is_none());
        assert!(render_http_request_panel(19, "GET", "/", 200, 1, "x", true).is_none());
    }

    #[test]
    fn test_request_panel_long_path_truncated() {
        let long = "/".to_string() + &"a".repeat(200);
        let panel = render_http_request_panel(100, "GET", &long, 200, 1, "x", false)
            .expect("panel should render");
        assert!(panel.contains("..."), "long path should be truncated");
    }

    #[test]
    fn test_request_panel_all_status_ranges() {
        for (status, label) in [(200, "2xx"), (301, "3xx"), (404, "4xx"), (500, "5xx")] {
            let panel = render_http_request_panel(100, "GET", "/x", status, 1, "x", true)
                .unwrap_or_else(|| panic!("{label} panel should render"));
            assert!(panel.contains("38;2;"), "{label}: 24-bit color");
        }
    }

    // ── LogPane tests (br-1m6a.20) ──

    #[test]
    fn log_pane_push_and_len() {
        let mut pane = LogPane::new();
        assert!(pane.is_empty());
        assert_eq!(pane.len(), 0);
        pane.push("hello");
        assert_eq!(pane.len(), 1);
        pane.push("world");
        assert_eq!(pane.len(), 2);
        assert!(!pane.is_empty());
    }

    #[test]
    fn log_pane_push_many() {
        let mut pane = LogPane::new();
        pane.push_many(vec!["a", "b", "c"]);
        assert_eq!(pane.len(), 3);
    }

    #[test]
    fn log_pane_clear() {
        let mut pane = LogPane::new();
        pane.push_many(vec!["a", "b"]);
        assert_eq!(pane.len(), 2);
        pane.clear();
        assert!(pane.is_empty());
    }

    #[test]
    fn log_pane_search() {
        let mut pane = LogPane::new();
        pane.push("INFO: starting");
        pane.push("ERROR: something failed");
        pane.push("INFO: done");
        let count = pane.search("ERROR");
        assert_eq!(count, 1);
        assert_eq!(pane.search_info(), Some((1, 1)));
        pane.clear_search();
        assert_eq!(pane.search_info(), None);
    }

    #[test]
    fn log_pane_filter() {
        let mut pane = LogPane::new();
        pane.push("INFO: a");
        pane.push("ERROR: b");
        pane.push("INFO: c");
        pane.set_filter(Some("ERROR"));
        // Filter is applied; push another line to verify incremental matching.
        pane.push("ERROR: d");
        pane.set_filter(None);
        assert_eq!(pane.len(), 4);
    }

    #[test]
    fn log_pane_regex_filter_matches_formatted_entries() {
        let mut pane = LogPane::new();
        pane.push("[00:00:00.000] INFO  MessageSent    GoldFox -> SilverWolf");
        pane.push("[00:00:00.001] DEBUG ToolCallEnd   send_message (12ms)");
        pane.push("[00:00:00.002] WARN  ResReleased   GoldFox src/**");

        // LogViewer.set_filter uses literal substring matching (not regex).
        pane.set_filter(Some("WARN"));
        assert_eq!(pane.search("ResReleased"), 1);
        assert_eq!(pane.search("MessageSent"), 0);

        pane.set_filter(None);
        assert_eq!(pane.search("MessageSent"), 1);
    }

    #[test]
    fn log_pane_follow_toggle() {
        let mut pane = LogPane::new();
        assert!(pane.auto_scroll_enabled());
        pane.toggle_follow();
        assert!(!pane.auto_scroll_enabled());
        pane.toggle_follow();
        assert!(pane.auto_scroll_enabled());
    }

    #[test]
    fn log_pane_scroll_operations() {
        let mut pane = LogPane::new();
        for i in 0..100 {
            pane.push(format!("line {i}"));
        }
        // These should not panic.
        pane.scroll_up(5);
        pane.scroll_down(3);
        pane.scroll_to_top();
        pane.scroll_to_bottom();
        pane.page_up();
        pane.page_down();
    }

    #[test]
    fn log_pane_default() {
        let pane = LogPane::default();
        assert!(pane.is_empty());
    }

    // ── split_columns tests (br-1m6a.20) ──

    #[test]
    fn split_columns_too_narrow_returns_none() {
        assert!(split_columns(59, 30).is_none());
        assert!(split_columns(0, 30).is_none());
    }

    #[test]
    fn split_columns_normal_width() {
        let (left, right) = split_columns(100, 30).expect("100 wide should split");
        assert_eq!(left, 30);
        assert_eq!(right, 70);
        assert_eq!(left + right, 100);
    }

    #[test]
    fn split_columns_clamps_ratio() {
        // Ratio below 10% should be clamped to 10%.
        let (left, _right) = split_columns(100, 5).expect("should split");
        assert!(left >= 10, "left={left} should be at least 10");

        // Ratio above 80% should be clamped to 80%.
        let (left, right) = split_columns(100, 95).expect("should split");
        assert!(right >= 20, "right={right} should be at least 20");
        assert!(left <= 80, "left={left} should be at most 80");
    }

    #[test]
    fn split_columns_60_wide_minimum() {
        let result = split_columns(60, 30);
        assert!(result.is_some());
        let (left, right) = result.unwrap();
        assert!(left >= 30);
        assert!(right >= 20);
        assert_eq!(left + right, 60);
    }

    #[test]
    fn split_columns_preserves_total_width() {
        for w in [60, 80, 100, 120, 160, 200] {
            for ratio in [10, 20, 30, 50, 70, 80] {
                if let Some((l, r)) = split_columns(w, ratio) {
                    assert_eq!(l + r, w, "w={w} ratio={ratio}: {l}+{r} != {w}");
                }
            }
        }
    }

    // ── Command palette tests ──

    #[test]
    fn command_palette_has_expected_action_count() {
        let palette = ConsoleCommandPalette::new();
        assert_eq!(palette.action_count(), 26);
    }

    #[test]
    fn command_palette_action_ids_are_unique() {
        let actions = build_palette_actions();
        let mut seen = std::collections::HashSet::new();
        for action in &actions {
            assert!(
                seen.insert(&action.id),
                "duplicate action id: {}",
                action.id
            );
        }
    }

    #[test]
    fn command_palette_all_actions_have_category() {
        let actions = build_palette_actions();
        for action in &actions {
            assert!(
                action.category.is_some(),
                "action {} missing category",
                action.id
            );
        }
    }

    #[test]
    fn command_palette_categories_are_expected() {
        let actions = build_palette_actions();
        let expected = ["Layout", "Theme", "Logs", "Tools", "Help"];
        for action in &actions {
            let cat = action.category.as_deref().unwrap();
            assert!(
                expected.contains(&cat),
                "unexpected category '{}' on action {}",
                cat,
                action.id
            );
        }
    }

    #[test]
    fn command_palette_default_not_visible() {
        let palette = ConsoleCommandPalette::new();
        assert!(!palette.is_visible());
    }

    #[test]
    fn command_palette_toggle_visibility() {
        let mut palette = ConsoleCommandPalette::new();
        assert!(!palette.is_visible());
        palette.open();
        assert!(palette.is_visible());
        palette.close();
        assert!(!palette.is_visible());
        palette.toggle();
        assert!(palette.is_visible());
        palette.toggle();
        assert!(!palette.is_visible());
    }

    #[test]
    fn command_palette_stable_action_order() {
        let a1 = build_palette_actions();
        let a2 = build_palette_actions();
        let ids1: Vec<&str> = a1.iter().map(|a| a.id.as_str()).collect();
        let ids2: Vec<&str> = a2.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids1, ids2, "action order must be deterministic");
    }

    #[test]
    fn command_palette_render_no_panic() {
        let palette = ConsoleCommandPalette::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 20, &mut pool);
        let area = Rect::new(0, 0, 120, 20);
        palette.render(area, &mut frame);
    }

    #[test]
    fn command_palette_render_open_no_panic() {
        let mut palette = ConsoleCommandPalette::new();
        palette.open();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 20, &mut pool);
        let area = Rect::new(0, 0, 120, 20);
        palette.render(area, &mut frame);
    }

    // ── LogPane mode transition tests (br-1m6a.20) ──

    #[test]
    fn log_pane_mode_defaults_to_normal() {
        let pane = LogPane::new();
        assert_eq!(pane.mode(), LogPaneMode::Normal);
    }

    #[test]
    fn log_pane_enter_search_mode() {
        let mut pane = LogPane::new();
        pane.enter_search_mode();
        assert_eq!(pane.mode(), LogPaneMode::Search);
    }

    #[test]
    fn log_pane_confirm_search_returns_to_normal() {
        let mut pane = LogPane::new();
        pane.push("hello world");
        pane.push("goodbye world");
        pane.enter_search_mode();
        pane.search_input.set_value("hello");
        pane.confirm_search();
        assert_eq!(pane.mode(), LogPaneMode::Normal);
        assert!(pane.search_info().is_some());
        let (cur, total) = pane.search_info().unwrap();
        assert_eq!(total, 1);
        assert_eq!(cur, 1);
    }

    #[test]
    fn log_pane_confirm_empty_search_clears() {
        let mut pane = LogPane::new();
        pane.push("hello world");
        pane.search("hello");
        assert!(pane.search_info().is_some());
        pane.enter_search_mode();
        pane.confirm_search();
        assert_eq!(pane.mode(), LogPaneMode::Normal);
        assert!(pane.search_info().is_none());
    }

    #[test]
    fn log_pane_cancel_search_returns_to_normal() {
        let mut pane = LogPane::new();
        pane.enter_search_mode();
        pane.cancel_search();
        assert_eq!(pane.mode(), LogPaneMode::Normal);
    }

    #[test]
    fn log_pane_toggle_help() {
        let mut pane = LogPane::new();
        assert_eq!(pane.mode(), LogPaneMode::Normal);
        pane.toggle_help();
        assert_eq!(pane.mode(), LogPaneMode::Help);
        pane.toggle_help();
        assert_eq!(pane.mode(), LogPaneMode::Normal);
    }

    #[test]
    fn log_pane_help_from_search_goes_to_help() {
        let mut pane = LogPane::new();
        pane.enter_search_mode();
        pane.toggle_help();
        assert_eq!(pane.mode(), LogPaneMode::Help);
    }

    #[test]
    fn log_pane_ring_buffer_overflow() {
        let mut pane = LogPane::new();
        for i in 0..LOG_PANE_MAX_LINES + 100 {
            pane.push(format!("line {i}"));
        }
        assert_eq!(pane.len(), LOG_PANE_MAX_LINES);
    }

    #[test]
    fn render_split_frame_no_panic() {
        let mut pane = LogPane::new();
        pane.push("line 1");
        pane.push("line 2");
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        let area = Rect::new(0, 0, 120, 40);
        render_split_frame(&mut frame, area, 30, &mut pane, |f, a| {
            let block = Block::bordered().title(" HUD ");
            block.render(a, f);
        });
    }

    #[test]
    fn render_split_frame_search_mode_no_panic() {
        let mut pane = LogPane::new();
        pane.push("line 1");
        pane.enter_search_mode();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        let area = Rect::new(0, 0, 120, 40);
        render_split_frame(&mut frame, area, 30, &mut pane, |f, a| {
            let block = Block::bordered().title(" HUD ");
            block.render(a, f);
        });
    }

    #[test]
    fn render_split_frame_help_mode_no_panic() {
        let mut pane = LogPane::new();
        pane.push("line 1");
        pane.toggle_help();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        let area = Rect::new(0, 0, 120, 40);
        render_split_frame(&mut frame, area, 30, &mut pane, |f, a| {
            let block = Block::bordered().title(" HUD ");
            block.render(a, f);
        });
    }

    #[test]
    fn render_split_frame_narrow_falls_back() {
        let mut pane = LogPane::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(50, 20, &mut pool);
        let area = Rect::new(0, 0, 50, 20);
        // When too narrow (<60), should fall back to full-width HUD rendering.
        render_split_frame(&mut frame, area, 30, &mut pane, |_f, _a| {
            // HUD renderer called as fallback.
        });
    }

    // ── ConsoleCaps tests ──

    #[test]
    fn console_caps_oneliner_format_is_stable() {
        let caps = ConsoleCaps {
            true_color: true,
            osc8_hyperlinks: false,
            mouse_sgr: true,
            sync_output: true,
            kitty_keyboard: false,
            focus_events: false,
            in_mux: false,
        };
        let line = caps.one_liner();
        assert!(line.starts_with("ConsoleCaps:"));
        assert!(line.contains("tc=1"));
        assert!(line.contains("osc8=0"));
        assert!(line.contains("mouse=1"));
        assert!(line.contains("sync=1"));
        assert!(line.contains("kitty=0"));
        assert!(line.contains("focus=0"));
        assert!(line.contains("mux=0"));
    }

    #[test]
    fn console_caps_oneliner_all_keys_present() {
        let caps = ConsoleCaps {
            true_color: false,
            osc8_hyperlinks: false,
            mouse_sgr: false,
            sync_output: false,
            kitty_keyboard: false,
            focus_events: false,
            in_mux: false,
        };
        let line = caps.one_liner();
        for key in [
            "tc=", "osc8=", "mouse=", "sync=", "kitty=", "focus=", "mux=",
        ] {
            assert!(line.contains(key), "missing key '{key}' in: {line}");
        }
    }

    #[test]
    fn console_caps_oneliner_is_ascii_only() {
        let caps = ConsoleCaps {
            true_color: true,
            osc8_hyperlinks: true,
            mouse_sgr: true,
            sync_output: true,
            kitty_keyboard: true,
            focus_events: true,
            in_mux: true,
        };
        let line = caps.one_liner();
        assert!(line.is_ascii(), "one-liner must be ASCII: {line}");
    }

    #[test]
    fn console_caps_banner_lines_not_empty() {
        let caps = ConsoleCaps {
            true_color: true,
            osc8_hyperlinks: false,
            mouse_sgr: false,
            sync_output: true,
            kitty_keyboard: false,
            focus_events: false,
            in_mux: false,
        };
        let lines = caps.banner_lines();
        assert!(!lines.is_empty());
        let joined = lines.join("\n");
        let stripped = strip_ansi_content(&joined);
        assert!(
            stripped.contains("Console Capabilities"),
            "expected 'Console Capabilities' in banner: {stripped}"
        );
    }

    #[test]
    fn console_caps_banner_mux_warning() {
        let caps = ConsoleCaps {
            true_color: false,
            osc8_hyperlinks: false,
            mouse_sgr: false,
            sync_output: false,
            kitty_keyboard: false,
            focus_events: false,
            in_mux: true,
        };
        let lines = caps.banner_lines();
        let joined = lines.join("\n");
        let stripped = strip_ansi_content(&joined);
        assert!(
            stripped.contains("multiplexer"),
            "mux warning expected: {stripped}"
        );
    }

    #[test]
    fn console_caps_help_hint_contains_enabled_caps() {
        let caps = ConsoleCaps {
            true_color: true,
            osc8_hyperlinks: true,
            mouse_sgr: false,
            sync_output: false,
            kitty_keyboard: false,
            focus_events: false,
            in_mux: false,
        };
        let hint = caps.help_hint();
        assert!(hint.contains("tc"), "expected 'tc' in hint: {hint}");
        assert!(hint.contains("osc8"), "expected 'osc8' in hint: {hint}");
        assert!(
            !hint.contains("mouse"),
            "mouse disabled, should not appear: {hint}"
        );
    }

    #[test]
    fn console_caps_help_hint_none_when_empty() {
        let caps = ConsoleCaps {
            true_color: false,
            osc8_hyperlinks: false,
            mouse_sgr: false,
            sync_output: false,
            kitty_keyboard: false,
            focus_events: false,
            in_mux: false,
        };
        let hint = caps.help_hint();
        assert_eq!(hint, "Caps: none");
    }

    #[test]
    fn console_caps_from_capabilities_maps_fields() {
        let mut ftui_caps = ftui::TerminalCapabilities::basic();
        ftui_caps.true_color = true;
        ftui_caps.osc8_hyperlinks = true;
        ftui_caps.mouse_sgr = false;
        ftui_caps.sync_output = true;
        ftui_caps.kitty_keyboard = false;
        ftui_caps.focus_events = true;
        let caps = ConsoleCaps::from_capabilities(&ftui_caps);
        assert!(caps.true_color);
        assert!(caps.osc8_hyperlinks);
        assert!(!caps.mouse_sgr);
        assert!(caps.sync_output);
        assert!(!caps.kitty_keyboard);
        assert!(caps.focus_events);
    }

    // ── help_overlay_addendum tests (br-1m6a.23) ──

    #[test]
    fn help_overlay_addendum_contains_capabilities_header() {
        let caps = ConsoleCaps {
            true_color: true,
            osc8_hyperlinks: false,
            mouse_sgr: false,
            sync_output: true,
            kitty_keyboard: false,
            focus_events: false,
            in_mux: false,
        };
        let addendum = caps.help_overlay_addendum();
        assert!(
            addendum.contains("Capabilities"),
            "addendum should contain capabilities header: {addendum}"
        );
    }

    #[test]
    fn help_overlay_addendum_shows_enabled_and_disabled() {
        let caps = ConsoleCaps {
            true_color: true,
            osc8_hyperlinks: false,
            mouse_sgr: true,
            sync_output: false,
            kitty_keyboard: false,
            focus_events: true,
            in_mux: false,
        };
        let addendum = caps.help_overlay_addendum();
        assert!(
            addendum.contains("+ True color"),
            "true_color should show '+': {addendum}"
        );
        assert!(
            addendum.contains("- OSC-8 links"),
            "osc8 disabled should show '-': {addendum}"
        );
        assert!(
            addendum.contains("+ Mouse (SGR)"),
            "mouse_sgr should show '+': {addendum}"
        );
        assert!(
            addendum.contains("+ Focus evts"),
            "focus_events should show '+': {addendum}"
        );
    }

    #[test]
    fn help_overlay_addendum_mux_warning() {
        let caps = ConsoleCaps {
            true_color: false,
            osc8_hyperlinks: false,
            mouse_sgr: false,
            sync_output: false,
            kitty_keyboard: false,
            focus_events: false,
            in_mux: true,
        };
        let addendum = caps.help_overlay_addendum();
        assert!(
            addendum.contains("multiplexer"),
            "mux flag should show warning: {addendum}"
        );
    }

    #[test]
    fn help_overlay_addendum_is_plain_ascii_except_markers() {
        let caps = ConsoleCaps {
            true_color: true,
            osc8_hyperlinks: true,
            mouse_sgr: true,
            sync_output: true,
            kitty_keyboard: true,
            focus_events: true,
            in_mux: true,
        };
        let addendum = caps.help_overlay_addendum();
        // Should be entirely printable ASCII + newlines (no ANSI escapes).
        for ch in addendum.chars() {
            assert!(
                ch.is_ascii_graphic() || ch == ' ' || ch == '\n',
                "unexpected character {ch:?} in addendum"
            );
        }
    }

    // ── format_hyperlink tests (br-1m6a.23) ──

    #[test]
    fn format_hyperlink_osc8_enabled() {
        let link = format_hyperlink("https://example.com", "Example", true);
        assert!(link.contains("\x1b]8;;https://example.com\x07"));
        assert!(link.contains("Example"));
        assert!(link.ends_with("\x1b]8;;\x07"));
    }

    #[test]
    fn format_hyperlink_osc8_disabled() {
        let link = format_hyperlink("https://example.com", "Example", false);
        assert_eq!(link, "Example (https://example.com)");
        // No escape sequences.
        assert!(!link.contains('\x1b'));
    }

    // ── LogPane caps_addendum wiring test ──

    #[test]
    fn log_pane_caps_addendum_initially_empty() {
        let pane = LogPane::new();
        assert!(pane.caps_addendum.is_empty());
    }

    #[test]
    fn log_pane_set_caps_addendum() {
        let mut pane = LogPane::new();
        pane.set_caps_addendum("test addendum".to_string());
        assert_eq!(pane.caps_addendum, "test addendum");
    }

    // ── LOG_PANE_HELP includes Ctrl+P hint (br-1m6a.23) ──

    #[test]
    fn log_pane_help_includes_palette_hint() {
        assert!(
            LOG_PANE_HELP.contains("Ctrl+P"),
            "help text should include Ctrl+P palette hint"
        );
        assert!(
            LOG_PANE_HELP.contains("Command palette"),
            "help text should mention command palette"
        );
    }

    // ── ConsoleEventBuffer tests (br-1m6a.22) ──

    #[test]
    fn event_buffer_push_and_len() {
        let mut buf = ConsoleEventBuffer::new();
        assert!(buf.is_empty());
        buf.push(
            ConsoleEventKind::HttpRequest,
            ConsoleEventSeverity::Info,
            "GET /health",
            vec![],
            None,
        );
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn event_buffer_assigns_sequential_ids() {
        let mut buf = ConsoleEventBuffer::new();
        let id1 = buf.push(
            ConsoleEventKind::ToolCallStart,
            ConsoleEventSeverity::Info,
            "start",
            vec![],
            None,
        );
        let id2 = buf.push(
            ConsoleEventKind::ToolCallEnd,
            ConsoleEventSeverity::Info,
            "end",
            vec![],
            None,
        );
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn event_buffer_overflow_evicts_oldest() {
        let mut buf = ConsoleEventBuffer::new();
        for i in 0..TIMELINE_MAX_EVENTS + 50 {
            buf.push(
                ConsoleEventKind::HttpRequest,
                ConsoleEventSeverity::Info,
                format!("req {i}"),
                vec![],
                None,
            );
        }
        assert_eq!(buf.len(), TIMELINE_MAX_EVENTS);
        let snap = buf.snapshot();
        assert_eq!(snap[0].id, 51);
        #[allow(clippy::cast_possible_truncation)]
        let expected_last = (TIMELINE_MAX_EVENTS + 50) as u64;
        assert_eq!(snap.last().unwrap().id, expected_last);
    }

    #[test]
    fn event_buffer_snapshot_is_ordered() {
        let mut buf = ConsoleEventBuffer::new();
        for _ in 0..10 {
            buf.push(
                ConsoleEventKind::HttpRequest,
                ConsoleEventSeverity::Info,
                "x",
                vec![],
                None,
            );
        }
        let snap = buf.snapshot();
        for window in snap.windows(2) {
            assert!(window[0].id < window[1].id);
        }
    }

    #[test]
    fn event_buffer_default() {
        let buf = ConsoleEventBuffer::default();
        assert!(buf.is_empty());
    }

    // ── TimelinePane tests (br-1m6a.22) ──

    #[test]
    fn timeline_pane_default_state() {
        let pane = TimelinePane::new();
        assert_eq!(pane.mode(), TimelinePaneMode::Normal);
        assert!(pane.follow_enabled());
        assert!(pane.filter_severity().is_none());
        assert!(pane.filter_kind().is_none());
    }

    #[test]
    fn timeline_pane_severity_filter_cycle() {
        let mut pane = TimelinePane::new();
        pane.cycle_severity_filter();
        assert_eq!(pane.filter_severity(), Some(ConsoleEventSeverity::Info));
        pane.cycle_severity_filter();
        assert_eq!(pane.filter_severity(), Some(ConsoleEventSeverity::Warn));
        pane.cycle_severity_filter();
        assert_eq!(pane.filter_severity(), Some(ConsoleEventSeverity::Error));
        pane.cycle_severity_filter();
        assert!(pane.filter_severity().is_none());
    }

    #[test]
    fn timeline_pane_kind_filter_cycle() {
        let mut pane = TimelinePane::new();
        pane.cycle_kind_filter();
        assert_eq!(pane.filter_kind(), Some(ConsoleEventKind::ToolCallStart));
        pane.cycle_kind_filter();
        assert_eq!(pane.filter_kind(), Some(ConsoleEventKind::ToolCallEnd));
        pane.cycle_kind_filter();
        assert_eq!(pane.filter_kind(), Some(ConsoleEventKind::HttpRequest));
        pane.cycle_kind_filter();
        assert!(pane.filter_kind().is_none());
    }

    #[test]
    fn timeline_pane_filter_matches() {
        let mut pane = TimelinePane::new();
        let events = vec![
            ConsoleEvent {
                id: 1,
                ts_iso: "2026-02-07T00:00:00Z".into(),
                kind: ConsoleEventKind::HttpRequest,
                severity: ConsoleEventSeverity::Info,
                summary: "GET /health".into(),
                fields: vec![],
                json: None,
            },
            ConsoleEvent {
                id: 2,
                ts_iso: "2026-02-07T00:00:01Z".into(),
                kind: ConsoleEventKind::ToolCallEnd,
                severity: ConsoleEventSeverity::Error,
                summary: "send_message failed".into(),
                fields: vec![],
                json: None,
            },
            ConsoleEvent {
                id: 3,
                ts_iso: "2026-02-07T00:00:02Z".into(),
                kind: ConsoleEventKind::HttpRequest,
                severity: ConsoleEventSeverity::Warn,
                summary: "POST /mcp 404".into(),
                fields: vec![],
                json: None,
            },
        ];
        assert_eq!(pane.visible_indices(&events).len(), 3);
        pane.filter_severity = Some(ConsoleEventSeverity::Error);
        let vis = pane.visible_indices(&events);
        assert_eq!(vis.len(), 1);
        assert_eq!(events[vis[0]].id, 2);
        pane.filter_severity = None;
        pane.query = "POST".to_string();
        let vis = pane.visible_indices(&events);
        assert_eq!(vis.len(), 1);
        assert_eq!(events[vis[0]].id, 3);
    }

    #[test]
    fn timeline_pane_follow_tracks_new_events() {
        let mut pane = TimelinePane::new();
        pane.on_event_pushed(42);
        assert_eq!(pane.selected_id, Some(42));
    }

    #[test]
    fn timeline_pane_toggle_follow() {
        let mut pane = TimelinePane::new();
        pane.toggle_follow();
        assert!(!pane.follow_enabled());
        pane.toggle_follow();
        assert!(pane.follow_enabled());
    }

    #[test]
    fn timeline_pane_toggle_help() {
        let mut pane = TimelinePane::new();
        pane.toggle_help();
        assert_eq!(pane.mode(), TimelinePaneMode::Help);
        pane.toggle_help();
        assert_eq!(pane.mode(), TimelinePaneMode::Normal);
    }

    #[test]
    fn timeline_pane_search_flow() {
        let mut pane = TimelinePane::new();
        pane.enter_search_mode();
        assert_eq!(pane.mode(), TimelinePaneMode::Search);
        pane.search_input.set_value("test");
        pane.confirm_search();
        assert_eq!(pane.mode(), TimelinePaneMode::Normal);
        assert_eq!(pane.query, "test");
        pane.enter_search_mode();
        pane.cancel_search();
        assert_eq!(pane.mode(), TimelinePaneMode::Normal);
    }

    #[test]
    fn timeline_pane_toggle_details() {
        let mut pane = TimelinePane::new();
        assert!(pane.show_details);
        pane.toggle_details();
        assert!(!pane.show_details);
    }

    #[test]
    fn timeline_pane_render_empty_no_panic() {
        let mut pane = TimelinePane::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        pane.render(Rect::new(0, 0, 80, 30), &mut frame, &[]);
    }

    #[test]
    fn timeline_pane_render_with_events_no_panic() {
        let mut pane = TimelinePane::new();
        let events = vec![
            ConsoleEvent {
                id: 1,
                ts_iso: "2026-02-07T00:00:00Z".into(),
                kind: ConsoleEventKind::HttpRequest,
                severity: ConsoleEventSeverity::Info,
                summary: "GET /health 200 5ms".into(),
                fields: vec![("client".into(), "127.0.0.1".into())],
                json: Some(serde_json::json!({"status": 200})),
            },
            ConsoleEvent {
                id: 2,
                ts_iso: "2026-02-07T00:00:01Z".into(),
                kind: ConsoleEventKind::ToolCallStart,
                severity: ConsoleEventSeverity::Info,
                summary: "send_message".into(),
                fields: vec![("project".into(), "/data/test".into())],
                json: None,
            },
        ];
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        pane.render(Rect::new(0, 0, 80, 30), &mut frame, &events);
    }

    #[test]
    fn render_split_frame_timeline_no_panic() {
        let mut pane = TimelinePane::new();
        let events = vec![ConsoleEvent {
            id: 1,
            ts_iso: "2026-02-07T00:00:00Z".into(),
            kind: ConsoleEventKind::HttpRequest,
            severity: ConsoleEventSeverity::Info,
            summary: "GET /health".into(),
            fields: vec![],
            json: None,
        }];
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        render_split_frame_timeline(
            &mut frame,
            Rect::new(0, 0, 120, 40),
            30,
            &mut pane,
            &events,
            |f, a| {
                Block::bordered().title(" HUD ").render(a, f);
            },
        );
    }

    #[test]
    fn render_split_frame_timeline_narrow_falls_back() {
        let mut pane = TimelinePane::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(50, 20, &mut pool);
        render_split_frame_timeline(
            &mut frame,
            Rect::new(0, 0, 50, 20),
            30,
            &mut pane,
            &[],
            |_f, _a| {},
        );
    }

    #[test]
    fn right_pane_view_equality() {
        assert_eq!(RightPaneView::Log, RightPaneView::Log);
        assert_ne!(RightPaneView::Log, RightPaneView::Timeline);
    }

    #[test]
    fn severity_labels() {
        assert_eq!(ConsoleEventSeverity::Info.label(), "INFO");
        assert_eq!(ConsoleEventSeverity::Warn.label(), "WARN");
        assert_eq!(ConsoleEventSeverity::Error.label(), "ERROR");
    }

    #[test]
    fn event_kind_labels() {
        assert_eq!(ConsoleEventKind::ToolCallStart.label(), "tool_start");
        assert_eq!(ConsoleEventKind::ToolCallEnd.label(), "tool_end");
        assert_eq!(ConsoleEventKind::HttpRequest.label(), "http");
    }

    // ── compact_path UTF-8 safety ────────────────────────────────────

    #[test]
    fn compact_path_short_unchanged() {
        assert_eq!(compact_path("/usr/bin", 20), "/usr/bin");
    }

    #[test]
    fn compact_path_3byte_chars() {
        let s = "/home/→/→/→/→/file";
        let r = compact_path(s, 10);
        assert!(r.chars().count() <= 10, "got {} chars", r.chars().count());
        assert!(r.starts_with("..."));
    }

    #[test]
    fn compact_path_cjk() {
        let s = "/home/日本語/テスト/ファイル";
        let r = compact_path(s, 12);
        assert!(r.chars().count() <= 12);
        assert!(r.starts_with("..."));
    }

    #[test]
    fn compact_path_emoji() {
        let s = "/🔥/🚀/💡/🎯/🏆/file.txt";
        let r = compact_path(s, 10);
        assert!(r.chars().count() <= 10);
    }

    #[test]
    fn compact_path_tiny_max() {
        let s = "/very/long/path";
        let r = compact_path(s, 3);
        assert!(r.chars().count() <= 3);
    }

    #[test]
    fn compact_path_multibyte_sweep() {
        let s = "/a→b🔥c/dé/f";
        for max in 1..=s.chars().count() + 2 {
            let r = compact_path(s, max);
            assert!(
                r.chars().count() <= max.max(3),
                "max={max} got {} chars: {r:?}",
                r.chars().count()
            );
        }
    }
}
