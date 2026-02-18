//! TOON (Token-Optimized Output Notation) output format support.
//!
//! Provides format resolution, encoder selection/validation, stats parsing,
//! and envelope construction for TOON-encoded responses. Falls back gracefully
//! to JSON on any encoder failure.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config::Config;

// ---------------------------------------------------------------------------
// Envelope types
// ---------------------------------------------------------------------------

/// Envelope wrapping a tool/resource response in the TOON protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToonEnvelope {
    pub format: String,
    pub data: serde_json::Value,
    pub meta: ToonMeta,
}

/// Metadata inside a TOON envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToonMeta {
    /// The format that was requested (e.g. "toon", "json", or null if implicit).
    pub requested: Option<String>,
    /// Where the format came from: "param", "default", or "implicit".
    pub source: String,
    /// Encoder binary path (present on success).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder: Option<String>,
    /// Error description (present on fallback).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toon_error: Option<String>,
    /// Stderr from failed encoder (present on non-zero exit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toon_stderr: Option<String>,
    /// Parsed stats from encoder stderr (present on success with stats enabled).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toon_stats: Option<ToonStats>,
    /// Raw stderr when stats parsing failed (present when stats enabled but parsing fails).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toon_stats_raw: Option<String>,
}

/// Token statistics from encoder stderr.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToonStats {
    pub json_tokens: u64,
    pub toon_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_percent: Option<f64>,
}

// ---------------------------------------------------------------------------
// Format resolution
// ---------------------------------------------------------------------------

/// Result of format resolution.
#[derive(Debug, Clone)]
pub struct FormatDecision {
    /// The resolved format: "json" or "toon".
    pub resolved: String,
    /// Where the decision came from: "param", "default", or "implicit".
    pub source: String,
    /// What was originally requested (None for implicit).
    pub requested: Option<String>,
}

/// Values treated as "no format specified" (use defaults).
const AUTO_VALUES: &[&str] = &["", "auto", "default", "none", "null"];

/// MIME type aliases mapping to canonical format names.
fn normalize_mime(value: &str) -> Option<&'static str> {
    match value {
        "application/json" | "text/json" => Some("json"),
        "application/toon" | "text/toon" => Some("toon"),
        _ => None,
    }
}

/// Resolve the output format from an explicit parameter and config defaults.
///
/// Returns `Err` with an error message if the format value is invalid.
pub fn resolve_output_format(
    format_value: Option<&str>,
    config: &Config,
) -> Result<FormatDecision, String> {
    // Check explicit parameter first
    if let Some(raw) = format_value {
        let lower = raw.trim().to_lowercase();

        // Auto-values → treat as None
        if AUTO_VALUES.contains(&lower.as_str()) {
            return resolve_from_default(config);
        }

        // Normalize MIME aliases
        let canonical = normalize_mime(&lower).unwrap_or(&lower);

        match canonical {
            "json" | "toon" => {
                return Ok(FormatDecision {
                    resolved: canonical.to_string(),
                    source: "param".to_string(),
                    requested: Some(canonical.to_string()),
                });
            }
            _ => {
                return Err(format!(
                    "Invalid format '{raw}'. Expected 'json' or 'toon'."
                ));
            }
        }
    }

    resolve_from_default(config)
}

fn resolve_from_default(config: &Config) -> Result<FormatDecision, String> {
    if let Some(ref default_fmt) = config.output_format_default {
        let lower = default_fmt.trim().to_lowercase();
        if AUTO_VALUES.contains(&lower.as_str()) {
            return Ok(implicit_json());
        }
        let canonical = normalize_mime(&lower).unwrap_or(&lower);
        match canonical {
            "json" | "toon" => Ok(FormatDecision {
                resolved: canonical.to_string(),
                source: "default".to_string(),
                requested: Some(canonical.to_string()),
            }),
            _ => Err(format!(
                "Invalid format '{default_fmt}'. Expected 'json' or 'toon'."
            )),
        }
    } else {
        Ok(implicit_json())
    }
}

fn implicit_json() -> FormatDecision {
    FormatDecision {
        resolved: "json".to_string(),
        source: "implicit".to_string(),
        requested: None,
    }
}

// ---------------------------------------------------------------------------
// Encoder selection + validation
// ---------------------------------------------------------------------------

/// Default encoder binary name.
const DEFAULT_ENCODER: &str = "tru";

/// Resolve the encoder binary path from config.
///
/// Returns the first token from the configured `TOON_BIN` (split via shell-like splitting),
/// or the default "tru" if not configured.
pub fn resolve_encoder(config: &Config) -> Vec<String> {
    let raw = config.toon_bin.as_deref().unwrap_or(DEFAULT_ENCODER);

    // Simple shell-like splitting (no quoting support, matches Python shlex.split fallback)
    let parts: Vec<String> = raw.split_whitespace().map(String::from).collect();
    if parts.is_empty() {
        vec![DEFAULT_ENCODER.to_string()]
    } else {
        parts
    }
}

/// Check if a binary looks like the `toon_rust` encoder.
///
/// Rejects binaries named exactly "toon" or "toon.exe" (Node.js CLI protection).
/// Runs `exe --help` and `exe --version` to validate.
pub fn looks_like_toon_rust_encoder(exe: &str) -> Result<bool, std::io::Error> {
    // Extract basename
    let basename = Path::new(exe)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(exe)
        .to_lowercase();

    // Reject Node.js toon CLI
    if basename == "toon" || basename == "toon.exe" {
        return Ok(false);
    }

    // Try --help: look for "reference implementation in rust"
    if let Ok(output) = Command::new(exe).arg("--help").output() {
        let text = String::from_utf8_lossy(&output.stdout);
        if text
            .to_lowercase()
            .contains("reference implementation in rust")
        {
            return Ok(true);
        }
    }

    // Try --version: look for "tru " or "toon_rust " prefix
    if let Ok(output) = Command::new(exe).arg("--version").output() {
        let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
        if text.starts_with("tru ") || text.starts_with("toon_rust ") {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Validate the encoder binary. Returns `Ok(exe_path)` or Err(error message).
pub fn validate_encoder(encoder_parts: &[String]) -> Result<String, String> {
    let exe = encoder_parts.first().ok_or("Empty encoder command")?;

    match looks_like_toon_rust_encoder(exe) {
        Ok(true) => Ok(exe.clone()),
        Ok(false) => Err(format!(
            "TOON_BIN resolved to '{exe}', which does not look like toon_rust (expected tru). \
             Refusing to run a non-toon_rust encoder."
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("TOON encoder not found: {e}"))
        }
        Err(e) => Err(format!("TOON encoder failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Stats parsing
// ---------------------------------------------------------------------------

/// Parse TOON stats from encoder stderr.
///
/// Looks for:
/// - `Token estimates: ~<N> (JSON) -> ~<N> (TOON)` (ASCII or Unicode arrow)
/// - `Saved ~<N> tokens (<percent>%)`
#[must_use]
pub fn parse_toon_stats(stderr: &str) -> Option<ToonStats> {
    // Regex-free parsing for no extra dependency: scan lines for patterns
    let mut json_tokens: Option<u64> = None;
    let mut toon_tokens: Option<u64> = None;
    let mut saved_tokens: Option<u64> = None;
    let mut saved_percent: Option<f64> = None;

    for line in stderr.lines() {
        // Token estimates line
        if line.contains("Token estimates:")
            && let Some(stats) = parse_token_line(line)
        {
            json_tokens = Some(stats.0);
            toon_tokens = Some(stats.1);
        }
        // Saved line
        if line.starts_with("Saved")
            && let Some((tokens, pct)) = parse_saved_line(line)
        {
            saved_tokens = Some(tokens);
            saved_percent = Some(pct);
        }
    }

    let jt = json_tokens?;
    let tt = toon_tokens?;

    Some(ToonStats {
        json_tokens: jt,
        toon_tokens: tt,
        saved_tokens,
        saved_percent,
    })
}

/// Parse `Token estimates: ~42 (JSON) -> ~18 (TOON)` or with `→`.
fn parse_token_line(line: &str) -> Option<(u64, u64)> {
    // Find after "Token estimates:"
    let after_prefix = line.split("Token estimates:").nth(1)?;
    let trimmed = after_prefix.trim();

    // Extract first ~N
    let json_start = trimmed.find('~')? + 1;
    let json_end = trimmed[json_start..].find(|c: char| !c.is_ascii_digit())? + json_start;
    let json_val: u64 = trimmed[json_start..json_end].parse().ok()?;

    // Find arrow (-> or →) and extract second ~N
    let after_arrow = if let Some(pos) = trimmed.find("->") {
        &trimmed[pos + 2..]
    } else if let Some(pos) = trimmed.find('\u{2192}') {
        &trimmed[pos + '\u{2192}'.len_utf8()..]
    } else {
        return None;
    };

    let toon_start = after_arrow.find('~')? + 1;
    let toon_end = after_arrow[toon_start..]
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_arrow.len() - toon_start)
        + toon_start;
    let toon_val: u64 = after_arrow[toon_start..toon_end].parse().ok()?;

    Some((json_val, toon_val))
}

/// Parse `Saved ~5 tokens (-50.0%)`.
fn parse_saved_line(line: &str) -> Option<(u64, f64)> {
    // "Saved ~<N> tokens (<pct>%)"
    let after_saved = line.strip_prefix("Saved")?;
    let trimmed = after_saved.trim();

    // ~N
    let start = trimmed.find('~')? + 1;
    // Only match non-negative digits (legacy: \d+ doesn't match minus)
    let end = trimmed[start..]
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len() - start)
        + start;
    if start == end {
        return None;
    }
    let tokens: u64 = trimmed[start..end].parse().ok()?;

    // Find percentage in parentheses: (N.N%)
    let paren_start = trimmed.find('(')? + 1;
    let paren_end = trimmed.find('%')?;
    let pct_str = &trimmed[paren_start..paren_end];
    let pct: f64 = pct_str.parse().ok()?;

    Some((tokens, pct))
}

// ---------------------------------------------------------------------------
// Encoding execution
// ---------------------------------------------------------------------------

/// Encode a JSON payload via the TOON encoder subprocess.
///
/// This is a synchronous function. For async contexts, callers should
/// dispatch to a thread pool.
///
/// Returns the TOON-encoded string on success, or an error description.
pub fn run_encoder(config: &Config, json_payload: &str) -> Result<EncoderSuccess, EncoderError> {
    let encoder_parts = resolve_encoder(config);
    let exe = validate_encoder(&encoder_parts).map_err(EncoderError::Validation)?;

    let mut cmd = Command::new(&exe);
    // Pass any extra args from config (e.g. TOON_BIN="tru --experimental")
    if encoder_parts.len() > 1 {
        cmd.args(&encoder_parts[1..]);
    }
    cmd.arg("--encode");
    if config.toon_stats_enabled {
        cmd.arg("--stats");
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EncoderError::NotFound(format!("TOON encoder not found: {e}"))
        } else {
            EncoderError::OsError(format!("TOON encoder failed: {e}"))
        }
    })?;

    // Write JSON to stdin
    if let Some(ref mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(json_payload.as_bytes());
    }

    let output = child
        .wait_with_output()
        .map_err(|e| EncoderError::OsError(format!("TOON encoder failed: {e}")))?;

    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(EncoderError::NonZeroExit {
            code,
            stderr: truncate_str(&stderr, 2000),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Parse stats if enabled
    let stats = if config.toon_stats_enabled {
        parse_toon_stats(&stderr)
    } else {
        None
    };

    let stats_raw = if config.toon_stats_enabled && stats.is_none() && !stderr.is_empty() {
        Some(truncate_str(&stderr, 2000))
    } else {
        None
    };

    Ok(EncoderSuccess {
        encoded: stdout,
        encoder: exe,
        stats,
        stats_raw,
    })
}

/// Successful encoder result.
#[derive(Debug)]
pub struct EncoderSuccess {
    pub encoded: String,
    pub encoder: String,
    pub stats: Option<ToonStats>,
    pub stats_raw: Option<String>,
}

/// Encoder error variants.
#[derive(Debug)]
pub enum EncoderError {
    /// Encoder binary validation failed (not `toon_rust`).
    Validation(String),
    /// Encoder binary not found.
    NotFound(String),
    /// OS error spawning/running encoder.
    OsError(String),
    /// Encoder exited with non-zero code.
    NonZeroExit { code: i32, stderr: String },
}

impl EncoderError {
    #[must_use]
    pub fn to_error_string(&self) -> String {
        match self {
            Self::Validation(msg) | Self::NotFound(msg) | Self::OsError(msg) => msg.clone(),
            Self::NonZeroExit { code, .. } => format!("TOON encoder exited with {code}"),
        }
    }

    #[must_use]
    pub fn stderr(&self) -> Option<&str> {
        match self {
            Self::NonZeroExit { stderr, .. } => Some(stderr),
            _ => None,
        }
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max.min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

// ---------------------------------------------------------------------------
// Envelope construction
// ---------------------------------------------------------------------------

/// Apply TOON formatting to a JSON payload.
///
/// If the format resolves to "json", returns `None` (caller should use the
/// original payload unchanged). If "toon", attempts encoding and returns
/// the envelope (success or fallback).
pub fn apply_toon_format(
    json_payload: &serde_json::Value,
    format_value: Option<&str>,
    config: &Config,
) -> Result<Option<ToonEnvelope>, String> {
    let decision = resolve_output_format(format_value, config)?;

    if decision.resolved == "json" {
        return Ok(None);
    }

    // Serialize payload to JSON string for the encoder
    let json_str = match serde_json::to_string(json_payload) {
        Ok(s) => s,
        Err(e) => {
            return Ok(Some(fallback_envelope(
                json_payload.clone(),
                &decision,
                &format!("json serialization failed: {e}"),
                None,
            )));
        }
    };

    match run_encoder(config, &json_str) {
        Ok(success) => Ok(Some(ToonEnvelope {
            format: "toon".to_string(),
            data: serde_json::Value::String(success.encoded),
            meta: ToonMeta {
                requested: decision.requested,
                source: decision.source,
                encoder: Some(success.encoder),
                toon_error: None,
                toon_stderr: None,
                toon_stats: success.stats,
                toon_stats_raw: success.stats_raw,
            },
        })),
        Err(err) => Ok(Some(fallback_envelope(
            json_payload.clone(),
            &decision,
            &err.to_error_string(),
            err.stderr().map(String::from),
        ))),
    }
}

/// Build a fallback JSON envelope when encoding fails.
fn fallback_envelope(
    data: serde_json::Value,
    decision: &FormatDecision,
    error: &str,
    stderr: Option<String>,
) -> ToonEnvelope {
    ToonEnvelope {
        format: "json".to_string(),
        data,
        meta: ToonMeta {
            requested: decision.requested.clone(),
            source: decision.source.clone(),
            encoder: None,
            toon_error: Some(error.to_string()),
            toon_stderr: stderr,
            toon_stats: None,
            toon_stats_raw: None,
        },
    }
}

/// Convenience: apply format to a JSON string (tool output).
///
/// Returns the original string if format resolves to JSON.
/// Returns an envelope JSON string if format is TOON.
pub fn apply_tool_format(
    json_result: &str,
    format_value: Option<&str>,
    config: &Config,
) -> Result<String, String> {
    let decision = resolve_output_format(format_value, config)?;

    if decision.resolved == "json" {
        return Ok(json_result.to_string());
    }

    // Parse the existing JSON result
    let payload: serde_json::Value =
        serde_json::from_str(json_result).map_err(|e| format!("json serialization failed: {e}"))?;

    apply_toon_format(&payload, format_value, config)?.map_or_else(
        || Ok(json_result.to_string()),
        |envelope| {
            serde_json::to_string(&envelope)
                .map_err(|e| format!("Failed to serialize envelope: {e}"))
        },
    )
}

/// Convenience: apply format to a resource response (already JSON string).
pub fn apply_resource_format<S: std::hash::BuildHasher>(
    json_result: &str,
    query_params: &HashMap<String, String, S>,
    config: &Config,
) -> Result<String, String> {
    let format_value = query_params.get("format").map(String::as_str);
    apply_tool_format(json_result, format_value, config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            toon_bin: None,
            toon_stats_enabled: false,
            output_format_default: None,
            ..Config::default()
        }
    }

    // -- Format resolution tests --

    #[test]
    fn resolve_explicit_toon() {
        let config = test_config();
        let d = resolve_output_format(Some("toon"), &config).unwrap();
        assert_eq!(d.resolved, "toon");
        assert_eq!(d.source, "param");
        assert_eq!(d.requested, Some("toon".to_string()));
    }

    #[test]
    fn resolve_explicit_json() {
        let config = test_config();
        let d = resolve_output_format(Some("json"), &config).unwrap();
        assert_eq!(d.resolved, "json");
        assert_eq!(d.source, "param");
        assert_eq!(d.requested, Some("json".to_string()));
    }

    #[test]
    fn resolve_mime_toon() {
        let config = test_config();
        let d = resolve_output_format(Some("application/toon"), &config).unwrap();
        assert_eq!(d.resolved, "toon");
        assert_eq!(d.source, "param");
    }

    #[test]
    fn resolve_mime_json() {
        let config = test_config();
        let d = resolve_output_format(Some("text/json"), &config).unwrap();
        assert_eq!(d.resolved, "json");
        assert_eq!(d.source, "param");
    }

    #[test]
    fn resolve_auto_uses_default() {
        let mut config = test_config();
        config.output_format_default = Some("toon".to_string());
        let d = resolve_output_format(Some("auto"), &config).unwrap();
        assert_eq!(d.resolved, "toon");
        assert_eq!(d.source, "default");
    }

    #[test]
    fn resolve_null_uses_default() {
        let mut config = test_config();
        config.output_format_default = Some("toon".to_string());
        let d = resolve_output_format(None, &config).unwrap();
        assert_eq!(d.resolved, "toon");
        assert_eq!(d.source, "default");
    }

    #[test]
    fn resolve_empty_uses_default() {
        let mut config = test_config();
        config.output_format_default = Some("json".to_string());
        let d = resolve_output_format(Some(""), &config).unwrap();
        assert_eq!(d.resolved, "json");
        assert_eq!(d.source, "default");
    }

    #[test]
    fn resolve_no_param_no_default_implicit_json() {
        let config = test_config();
        let d = resolve_output_format(None, &config).unwrap();
        assert_eq!(d.resolved, "json");
        assert_eq!(d.source, "implicit");
        assert_eq!(d.requested, None);
    }

    #[test]
    fn resolve_invalid_format_rejected() {
        let config = test_config();
        let err = resolve_output_format(Some("xml"), &config).unwrap_err();
        assert!(err.contains("Invalid format 'xml'"));
    }

    #[test]
    fn resolve_auto_aliases() {
        let config = test_config();
        for alias in &["", "auto", "default", "none", "null"] {
            let d = resolve_output_format(Some(alias), &config).unwrap();
            assert_eq!(d.resolved, "json", "alias={alias}");
            assert_eq!(d.source, "implicit", "alias={alias}");
        }
    }

    // -- Stats parsing tests --

    #[test]
    fn parse_full_stats_ascii_arrow() {
        let stderr = "Token estimates: ~42 (JSON) -> ~18 (TOON)\nSaved ~24 tokens (-57.1%)\n";
        let stats = parse_toon_stats(stderr).unwrap();
        assert_eq!(stats.json_tokens, 42);
        assert_eq!(stats.toon_tokens, 18);
        assert_eq!(stats.saved_tokens, Some(24));
        assert!((stats.saved_percent.unwrap() - (-57.1)).abs() < 0.01);
    }

    #[test]
    fn parse_full_stats_unicode_arrow() {
        let stderr =
            "Token estimates: ~100 (JSON) \u{2192} ~35 (TOON)\nSaved ~65 tokens (-65.0%)\n";
        let stats = parse_toon_stats(stderr).unwrap();
        assert_eq!(stats.json_tokens, 100);
        assert_eq!(stats.toon_tokens, 35);
        assert_eq!(stats.saved_tokens, Some(65));
        assert!((stats.saved_percent.unwrap() - (-65.0)).abs() < 0.01);
    }

    #[test]
    fn parse_tokens_only_no_saved_line() {
        let stderr = "Token estimates: ~50 (JSON) -> ~30 (TOON)\n";
        let stats = parse_toon_stats(stderr).unwrap();
        assert_eq!(stats.json_tokens, 50);
        assert_eq!(stats.toon_tokens, 30);
        assert_eq!(stats.saved_tokens, None);
        assert_eq!(stats.saved_percent, None);
    }

    #[test]
    fn parse_zero_savings() {
        let stderr = "Token estimates: ~10 (JSON) -> ~10 (TOON)\nSaved ~0 tokens (0.0%)\n";
        let stats = parse_toon_stats(stderr).unwrap();
        assert_eq!(stats.json_tokens, 10);
        assert_eq!(stats.toon_tokens, 10);
        assert_eq!(stats.saved_tokens, Some(0));
        assert!((stats.saved_percent.unwrap() - 0.0).abs() < 0.01);
    }

    #[test]
    fn parse_negative_savings() {
        // ~-3 won't match \d+ so saved_tokens/saved_percent absent
        let stderr = "Token estimates: ~5 (JSON) -> ~8 (TOON)\nSaved ~-3 tokens (60.0%)\n";
        let stats = parse_toon_stats(stderr).unwrap();
        assert_eq!(stats.json_tokens, 5);
        assert_eq!(stats.toon_tokens, 8);
        // -3 has a minus so doesn't match digit-only pattern
        assert_eq!(stats.saved_tokens, None);
        assert_eq!(stats.saved_percent, None);
    }

    #[test]
    fn parse_empty_stderr() {
        assert!(parse_toon_stats("").is_none());
    }

    #[test]
    fn parse_unrelated_stderr() {
        assert!(parse_toon_stats("warning: deprecated flag --legacy\n").is_none());
    }

    #[test]
    fn parse_stats_with_noise() {
        let stderr = "info: loading config\nToken estimates: ~200 (JSON) -> ~80 (TOON)\nSaved ~120 tokens (-60.0%)\ninfo: done\n";
        let stats = parse_toon_stats(stderr).unwrap();
        assert_eq!(stats.json_tokens, 200);
        assert_eq!(stats.toon_tokens, 80);
        assert_eq!(stats.saved_tokens, Some(120));
        assert!((stats.saved_percent.unwrap() - (-60.0)).abs() < 0.01);
    }

    // -- Encoder resolution --

    #[test]
    fn resolve_encoder_default() {
        let config = test_config();
        let parts = resolve_encoder(&config);
        assert_eq!(parts, vec!["tru".to_string()]);
    }

    #[test]
    fn resolve_encoder_custom() {
        let mut config = test_config();
        config.toon_bin = Some("/usr/local/bin/tru --experimental".to_string());
        let parts = resolve_encoder(&config);
        assert_eq!(
            parts,
            vec![
                "/usr/local/bin/tru".to_string(),
                "--experimental".to_string()
            ]
        );
    }

    // -- Fallback envelope --

    #[test]
    fn fallback_envelope_structure() {
        let data = serde_json::json!({"id": 1, "subject": "Test"});
        let decision = FormatDecision {
            resolved: "toon".to_string(),
            source: "param".to_string(),
            requested: Some("toon".to_string()),
        };
        let env = fallback_envelope(data.clone(), &decision, "encoder failed", None);
        assert_eq!(env.format, "json");
        assert_eq!(env.data, data);
        assert_eq!(env.meta.requested, Some("toon".to_string()));
        assert_eq!(env.meta.source, "param");
        assert_eq!(env.meta.toon_error.as_deref(), Some("encoder failed"));
        assert!(env.meta.encoder.is_none());
    }

    #[test]
    fn fallback_envelope_with_stderr() {
        let data = serde_json::json!({"id": 1});
        let decision = FormatDecision {
            resolved: "toon".to_string(),
            source: "param".to_string(),
            requested: Some("toon".to_string()),
        };
        let env = fallback_envelope(
            data,
            &decision,
            "TOON encoder exited with 1",
            Some("error: invalid JSON\n".to_string()),
        );
        assert_eq!(env.format, "json");
        assert_eq!(
            env.meta.toon_error.as_deref(),
            Some("TOON encoder exited with 1")
        );
        assert_eq!(
            env.meta.toon_stderr.as_deref(),
            Some("error: invalid JSON\n")
        );
    }

    // -- apply_toon_format --

    #[test]
    fn apply_toon_format_json_returns_none() {
        let config = test_config();
        let payload = serde_json::json!({"id": 1});
        let result = apply_toon_format(&payload, Some("json"), &config).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn apply_toon_format_no_param_no_default_returns_none() {
        let config = test_config();
        let payload = serde_json::json!({"id": 1});
        let result = apply_toon_format(&payload, None, &config).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn apply_toon_format_invalid_rejects() {
        let config = test_config();
        let payload = serde_json::json!({"id": 1});
        assert!(apply_toon_format(&payload, Some("xml"), &config).is_err());
    }

    #[test]
    fn apply_toon_format_encoder_missing_fallback() {
        let config = Config {
            toon_bin: Some("/nonexistent/tru_binary".to_string()),
            ..test_config()
        };
        let payload = serde_json::json!({"id": 1, "subject": "Test"});
        let envelope = apply_toon_format(&payload, Some("toon"), &config)
            .unwrap()
            .expect("should return Some");
        assert_eq!(envelope.format, "json");
        assert_eq!(envelope.data, payload);
        assert_eq!(envelope.meta.requested, Some("toon".to_string()));
        assert!(envelope.meta.toon_error.is_some());
        assert!(envelope.meta.encoder.is_none());
    }

    #[test]
    fn apply_toon_format_with_real_encoder() {
        let config = test_config();
        let payload = serde_json::json!({"id": 1, "subject": "Hello"});
        let envelope = apply_toon_format(&payload, Some("toon"), &config)
            .unwrap()
            .expect("should return Some");
        assert!(envelope.format == "toon" || envelope.format == "json");
        assert_eq!(envelope.meta.requested, Some("toon".to_string()));
        if envelope.format == "toon" {
            assert!(envelope.data.is_string());
            assert!(envelope.meta.encoder.is_some());
        }
    }

    // -- apply_tool_format --

    #[test]
    fn apply_tool_format_json_passthrough() {
        let config = test_config();
        let json = r#"{"id":1}"#;
        assert_eq!(
            apply_tool_format(json, Some("json"), &config).unwrap(),
            json
        );
    }

    #[test]
    fn apply_tool_format_no_format_passthrough() {
        let config = test_config();
        let json = r#"{"id":1}"#;
        assert_eq!(apply_tool_format(json, None, &config).unwrap(), json);
    }

    #[test]
    fn apply_tool_format_toon_wraps() {
        let config = Config {
            toon_bin: Some("/nonexistent/encoder".to_string()),
            ..test_config()
        };
        let json = r#"{"id":1,"subject":"Test"}"#;
        let result = apply_tool_format(json, Some("toon"), &config).unwrap();
        let envelope: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(envelope["format"], "json"); // fallback
        assert_eq!(envelope["data"]["id"], 1);
        assert!(envelope["meta"]["toon_error"].is_string());
    }

    // -- apply_resource_format --

    #[test]
    fn apply_resource_format_no_format_passthrough() {
        let config = test_config();
        let json = r#"{"agent":"Blue"}"#;
        let params = HashMap::new();
        assert_eq!(apply_resource_format(json, &params, &config).unwrap(), json);
    }

    #[test]
    fn apply_resource_format_toon_wraps() {
        let config = Config {
            toon_bin: Some("/nonexistent/encoder".to_string()),
            ..test_config()
        };
        let json = r#"{"agent":"Blue"}"#;
        let mut params = HashMap::new();
        params.insert("format".to_string(), "toon".to_string());
        let result = apply_resource_format(json, &params, &config).unwrap();
        let envelope: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(envelope["format"], "json");
        assert_eq!(envelope["data"]["agent"], "Blue");
    }

    // -- Encoder error types --

    #[test]
    fn encoder_error_validation_message() {
        let err = EncoderError::Validation("bad encoder".to_string());
        assert_eq!(err.to_error_string(), "bad encoder");
        assert!(err.stderr().is_none());
    }

    #[test]
    fn encoder_error_nonzero_exit() {
        let err = EncoderError::NonZeroExit {
            code: 1,
            stderr: "error: bad input".to_string(),
        };
        assert_eq!(err.to_error_string(), "TOON encoder exited with 1");
        assert_eq!(err.stderr(), Some("error: bad input"));
    }

    // -- Truncation --

    #[test]
    fn truncate_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_long() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn truncate_unicode_at_char_boundary() {
        let s = "h\u{00E9}llo"; // h + 'é' + llo
        assert_eq!(truncate_str(s, 2), "h");
        assert_eq!(truncate_str(s, 3), "h\u{00E9}");
    }

    // -- Envelope serialization --

    #[test]
    fn envelope_roundtrip() {
        let envelope = ToonEnvelope {
            format: "toon".to_string(),
            data: serde_json::Value::String("~encoded".to_string()),
            meta: ToonMeta {
                requested: Some("toon".to_string()),
                source: "param".to_string(),
                encoder: Some("tru".to_string()),
                toon_error: None,
                toon_stderr: None,
                toon_stats: Some(ToonStats {
                    json_tokens: 42,
                    toon_tokens: 18,
                    saved_tokens: Some(24),
                    saved_percent: Some(-57.1),
                }),
                toon_stats_raw: None,
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: ToonEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.format, "toon");
        assert_eq!(parsed.data.as_str().unwrap(), "~encoded");
        let stats = parsed.meta.toon_stats.unwrap();
        assert_eq!(stats.json_tokens, 42);
        assert_eq!(stats.toon_tokens, 18);
    }

    #[test]
    fn fallback_envelope_skips_none_fields() {
        let envelope = ToonEnvelope {
            format: "json".to_string(),
            data: serde_json::json!({"id": 1}),
            meta: ToonMeta {
                requested: Some("toon".to_string()),
                source: "param".to_string(),
                encoder: None,
                toon_error: Some("not found".to_string()),
                toon_stderr: None,
                toon_stats: None,
                toon_stats_raw: None,
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["meta"].get("encoder").is_none());
        assert!(parsed["meta"].get("toon_stderr").is_none());
        assert!(parsed["meta"].get("toon_stats").is_none());
        assert!(parsed["meta"].get("toon_stats_raw").is_none());
        assert!(parsed["meta"]["toon_error"].is_string());
    }

    // -- validate_encoder direct tests --

    #[test]
    fn validate_encoder_empty_command_rejected() {
        let parts: Vec<String> = vec![];
        assert!(validate_encoder(&parts).is_err());
    }

    #[test]
    fn validate_encoder_nonexistent_path() {
        let parts = vec!["/nonexistent/tru_binary".to_string()];
        let err = validate_encoder(&parts).unwrap_err();
        // Nonexistent binary: either "not found" or "does not look like toon_rust"
        // depending on how the OS reports the error to looks_like_toon_rust_encoder
        assert!(
            err.contains("not found") || err.contains("not look like"),
            "unexpected error: {err}"
        );
    }

    // -- looks_like_toon_rust_encoder basename rejection --

    #[test]
    fn basename_toon_rejected() {
        // "toon" basename should be rejected regardless of path
        let result = looks_like_toon_rust_encoder("toon");
        if matches!(result, Ok(true)) {
            panic!("should reject 'toon' basename");
        }
    }

    #[test]
    fn basename_toon_exe_rejected() {
        let result = looks_like_toon_rust_encoder("toon.exe");
        if matches!(result, Ok(true)) {
            panic!("should reject 'toon.exe' basename");
        }
    }

    #[test]
    fn basename_toon_in_path_rejected() {
        let result = looks_like_toon_rust_encoder("/usr/local/bin/toon");
        if matches!(result, Ok(true)) {
            panic!("should reject '/usr/local/bin/toon'");
        }
    }

    // -- EncoderError variants --

    #[test]
    fn encoder_error_not_found_message() {
        let err = EncoderError::NotFound("TOON encoder not found: No such file".to_string());
        assert!(err.to_error_string().contains("not found"));
        assert!(err.stderr().is_none());
    }

    #[test]
    fn encoder_error_os_error_message() {
        let err = EncoderError::OsError("TOON encoder failed: permission denied".to_string());
        assert!(err.to_error_string().contains("permission denied"));
        assert!(err.stderr().is_none());
    }

    // -- parse_toon_stats edge cases --

    #[test]
    fn parse_stats_large_numbers() {
        let stderr =
            "Token estimates: ~100000 (JSON) -> ~35000 (TOON)\nSaved ~65000 tokens (-65.0%)\n";
        let stats = parse_toon_stats(stderr).unwrap();
        assert_eq!(stats.json_tokens, 100_000);
        assert_eq!(stats.toon_tokens, 35_000);
        assert_eq!(stats.saved_tokens, Some(65_000));
    }

    #[test]
    fn parse_stats_single_digit() {
        let stderr = "Token estimates: ~1 (JSON) -> ~1 (TOON)\nSaved ~0 tokens (0.0%)\n";
        let stats = parse_toon_stats(stderr).unwrap();
        assert_eq!(stats.json_tokens, 1);
        assert_eq!(stats.toon_tokens, 1);
        assert_eq!(stats.saved_tokens, Some(0));
    }

    // -- resolve_output_format with config default --

    #[test]
    fn resolve_default_config_toon() {
        let config = Config {
            output_format_default: Some("toon".to_string()),
            ..test_config()
        };
        let d = resolve_output_format(None, &config).unwrap();
        assert_eq!(d.resolved, "toon");
        assert_eq!(d.source, "default");
        assert_eq!(d.requested, Some("toon".to_string()));
    }

    #[test]
    fn resolve_explicit_overrides_default() {
        let config = Config {
            output_format_default: Some("toon".to_string()),
            ..test_config()
        };
        // Explicit "json" should override default "toon"
        let d = resolve_output_format(Some("json"), &config).unwrap();
        assert_eq!(d.resolved, "json");
        assert_eq!(d.source, "param");
    }

    #[test]
    fn resolve_default_mime_alias() {
        let config = Config {
            output_format_default: Some("application/toon".to_string()),
            ..test_config()
        };
        let d = resolve_output_format(None, &config).unwrap();
        assert_eq!(d.resolved, "toon");
        assert_eq!(d.source, "default");
    }

    #[test]
    fn resolve_default_invalid_rejected() {
        let config = Config {
            output_format_default: Some("yaml".to_string()),
            ..test_config()
        };
        let err = resolve_output_format(None, &config).unwrap_err();
        assert!(err.contains("Invalid format"));
    }

    // -- ToonEnvelope JSON contract --

    #[test]
    fn envelope_with_all_fields_serializes() {
        let envelope = ToonEnvelope {
            format: "json".to_string(),
            data: serde_json::json!({"id": 1}),
            meta: ToonMeta {
                requested: Some("toon".to_string()),
                source: "param".to_string(),
                encoder: None,
                toon_error: Some("encoder failed".to_string()),
                toon_stderr: Some("error: bad input".to_string()),
                toon_stats: None,
                toon_stats_raw: Some("raw stderr content".to_string()),
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // All non-None fields should be present
        assert!(parsed["meta"]["toon_error"].is_string());
        assert!(parsed["meta"]["toon_stderr"].is_string());
        assert!(parsed["meta"]["toon_stats_raw"].is_string());
        // None fields should be absent
        assert!(parsed["meta"].get("encoder").is_none());
        assert!(parsed["meta"].get("toon_stats").is_none());
    }
}
