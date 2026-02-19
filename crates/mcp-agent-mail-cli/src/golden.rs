//! Golden-output capture and normalization helpers.
//!
//! These utilities are intentionally small and deterministic so they can be
//! reused by native `am golden` workflows and golden snapshot tests.

#![forbid(unsafe_code)]

use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Captured stdout/stderr plus exit code for a command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedCommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Which stream from a command should be persisted as golden text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoldenStream {
    Stdout,
    Stderr,
    Combined,
}

/// Declarative command specification used by `am golden`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GoldenCommandSpec {
    pub filename: String,
    pub command: Vec<String>,
    pub expected_exit_code: i32,
    pub stream: GoldenStream,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
}

impl GoldenCommandSpec {
    #[must_use]
    pub fn new(filename: impl Into<String>, command: Vec<String>) -> Self {
        Self {
            filename: filename.into(),
            command,
            expected_exit_code: 0,
            stream: GoldenStream::Stdout,
            stdin: None,
            env: Vec::new(),
        }
    }

    #[must_use]
    pub fn expected_exit_code(mut self, code: i32) -> Self {
        self.expected_exit_code = code;
        self
    }

    #[must_use]
    pub fn stream(mut self, stream: GoldenStream) -> Self {
        self.stream = stream;
        self
    }

    #[must_use]
    pub fn stdin(mut self, stdin: impl Into<String>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

/// Normalized output produced by executing a golden command spec.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GoldenCommandRun {
    pub filename: String,
    pub expected_exit_code: i32,
    pub exit_code: i32,
    pub normalized_stdout: String,
    pub normalized_stderr: String,
    pub normalized_output: String,
}

/// Errors returned when capturing command output for golden artifacts.
#[derive(Debug, thiserror::Error)]
pub enum GoldenCaptureError {
    #[error("command must not be empty")]
    EmptyCommand,
    #[error("failed to run command: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors for reading/writing `checksums.sha256`.
#[derive(Debug, thiserror::Error)]
pub enum GoldenChecksumError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid checksum line {line}: {reason}")]
    Parse { line: usize, reason: String },
}

/// Result of comparing expected-vs-actual golden text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldenComparison {
    pub expected_sha256: String,
    pub actual_sha256: String,
    pub matches: bool,
    pub inline_diff: Option<String>,
}

#[derive(Debug)]
struct NormalizationRule {
    regex: Regex,
    replacement: &'static str,
}

fn default_normalization_rules() -> &'static [NormalizationRule] {
    static RULES: OnceLock<Vec<NormalizationRule>> = OnceLock::new();
    RULES
        .get_or_init(|| {
            [
                // Strip ANSI SGR escape codes.
                (r"\x1b\[[0-9;]*m", ""),
                // Normalize ISO-8601 timestamps.
                (r"\d{4}-\d{2}-\d{2}T[\d:.Z+\-]+", "TIMESTAMP"),
                // Normalize pid=12345 fragments.
                (r"pid=\d+", "pid=PID"),
            ]
            .into_iter()
            .map(|(pattern, replacement)| NormalizationRule {
                regex: Regex::new(pattern).unwrap_or_else(|e| {
                    panic!("invalid built-in golden normalization regex '{pattern}': {e}")
                }),
                replacement,
            })
            .collect()
        })
        .as_slice()
}

/// Normalize unstable output fragments to deterministic placeholders.
///
/// Rules intentionally match the legacy `bench_golden.sh` behavior:
/// 1) strip ANSI escapes, 2) normalize timestamps, 3) normalize `pid=...`.
#[must_use]
pub fn normalize_output(raw: &str) -> String {
    let mut out = raw.to_string();
    for rule in default_normalization_rules() {
        out = rule.regex.replace_all(&out, rule.replacement).into_owned();
    }
    out
}

/// Normalize both stdout and stderr of an existing capture.
#[must_use]
pub fn normalize_captured_output(captured: &CapturedCommandOutput) -> CapturedCommandOutput {
    CapturedCommandOutput {
        stdout: normalize_output(&captured.stdout),
        stderr: normalize_output(&captured.stderr),
        exit_code: captured.exit_code,
    }
}

/// Capture command stdout/stderr and exit code.
pub fn capture_command(
    command: &[String],
    env: &[(String, String)],
    working_dir: Option<&Path>,
) -> Result<CapturedCommandOutput, GoldenCaptureError> {
    capture_command_with_stdin(command, env, working_dir, None)
}

/// Capture command stdout/stderr and exit code with optional stdin payload.
pub fn capture_command_with_stdin(
    command: &[String],
    env: &[(String, String)],
    working_dir: Option<&Path>,
    stdin: Option<&str>,
) -> Result<CapturedCommandOutput, GoldenCaptureError> {
    let (program, args) = command
        .split_first()
        .ok_or(GoldenCaptureError::EmptyCommand)?;
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }

    let output = if let Some(stdin_payload) = stdin {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut child_stdin) = child.stdin.take() {
            child_stdin.write_all(stdin_payload.as_bytes())?;
        }
        child.wait_with_output()?
    } else {
        cmd.output()?
    };

    Ok(CapturedCommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

/// Convenience wrapper around [`capture_command`] + [`normalize_captured_output`].
pub fn capture_and_normalize_command(
    command: &[String],
    env: &[(String, String)],
    working_dir: Option<&Path>,
) -> Result<CapturedCommandOutput, GoldenCaptureError> {
    let captured = capture_command_with_stdin(command, env, working_dir, None)?;
    Ok(normalize_captured_output(&captured))
}

fn select_stream_output(stream: GoldenStream, captured: &CapturedCommandOutput) -> String {
    match stream {
        GoldenStream::Stdout => captured.stdout.clone(),
        GoldenStream::Stderr => captured.stderr.clone(),
        GoldenStream::Combined => format!("{}{}", captured.stdout, captured.stderr),
    }
}

/// Execute a golden command, normalize output, and select target stream text.
pub fn run_golden_command(
    spec: &GoldenCommandSpec,
    env: &[(String, String)],
    working_dir: Option<&Path>,
) -> Result<GoldenCommandRun, GoldenCaptureError> {
    let mut merged_env = env.to_vec();
    merged_env.extend(spec.env.iter().cloned());
    let captured = capture_command_with_stdin(
        &spec.command,
        &merged_env,
        working_dir,
        spec.stdin.as_deref(),
    )?;
    let normalized = normalize_captured_output(&captured);
    let selected = select_stream_output(spec.stream, &normalized);
    Ok(GoldenCommandRun {
        filename: spec.filename.clone(),
        expected_exit_code: spec.expected_exit_code,
        exit_code: normalized.exit_code,
        normalized_stdout: normalized.stdout,
        normalized_stderr: normalized.stderr,
        normalized_output: selected,
    })
}

/// Compute SHA-256 checksum of text as lowercase hex.
#[must_use]
pub fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

/// Parse `sha256sum`-style file: `<hex><space><space><filename>`.
pub fn read_checksums_file(path: &Path) -> Result<BTreeMap<String, String>, GoldenChecksumError> {
    let content = std::fs::read_to_string(path)?;
    let mut out = BTreeMap::new();
    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((hash_raw, filename_raw)) = line.split_once(char::is_whitespace) else {
            return Err(GoldenChecksumError::Parse {
                line: line_no,
                reason: "expected '<sha256>  <filename>'".to_string(),
            });
        };
        let hash = hash_raw.trim();
        let filename = filename_raw.trim();
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(GoldenChecksumError::Parse {
                line: line_no,
                reason: format!("invalid sha256 hash '{hash}'"),
            });
        }
        if filename.is_empty() {
            return Err(GoldenChecksumError::Parse {
                line: line_no,
                reason: "missing filename".to_string(),
            });
        }
        out.insert(filename.to_string(), hash.to_ascii_lowercase());
    }
    Ok(out)
}

/// Write `sha256sum`-style checksums in deterministic filename order.
pub fn write_checksums_file(
    path: &Path,
    checksums: &BTreeMap<String, String>,
) -> Result<(), GoldenChecksumError> {
    let mut output = String::new();
    for (filename, hash) in checksums {
        output.push_str(hash);
        output.push_str("  ");
        output.push_str(filename);
        output.push('\n');
    }
    std::fs::write(path, output)?;
    Ok(())
}

fn build_inline_diff(expected: &str, actual: &str, context_lines: usize) -> String {
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    let shared_len = expected_lines.len().min(actual_lines.len());
    let mismatch_idx = (0..shared_len)
        .find(|&idx| expected_lines[idx] != actual_lines[idx])
        .unwrap_or(shared_len);

    let max_len = expected_lines.len().max(actual_lines.len());
    let start = mismatch_idx.saturating_sub(context_lines);
    let end = max_len.min(mismatch_idx + context_lines + 1);

    let mut out = String::new();
    out.push_str(&format!(
        "@@ mismatch around line {} @@\n",
        mismatch_idx + 1
    ));
    for idx in start..end {
        match (expected_lines.get(idx), actual_lines.get(idx)) {
            (Some(exp), Some(act)) if exp == act => {
                out.push_str(&format!(" {:>5} | {exp}\n", idx + 1));
            }
            (Some(exp), Some(act)) => {
                out.push_str(&format!("-{:>5} | {exp}\n", idx + 1));
                out.push_str(&format!("+{:>5} | {act}\n", idx + 1));
            }
            (Some(exp), None) => {
                out.push_str(&format!("-{:>5} | {exp}\n", idx + 1));
            }
            (None, Some(act)) => {
                out.push_str(&format!("+{:>5} | {act}\n", idx + 1));
            }
            (None, None) => {}
        }
    }
    out
}

/// Compare expected-vs-actual text with SHA-256 and inline diff context.
#[must_use]
pub fn compare_text(expected: &str, actual: &str) -> GoldenComparison {
    let expected_sha256 = sha256_hex(expected);
    let actual_sha256 = sha256_hex(actual);
    let matches = expected == actual;
    let inline_diff = (!matches).then(|| build_inline_diff(expected, actual, 3));
    GoldenComparison {
        expected_sha256,
        actual_sha256,
        matches,
        inline_diff,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_output_applies_ansi_timestamp_and_pid_rules() {
        let raw = "\x1b[31mERROR\x1b[0m at 2026-02-12T07:30:59.123Z pid=48152";
        assert_eq!(normalize_output(raw), "ERROR at TIMESTAMP pid=PID");
    }

    #[test]
    fn normalize_output_is_idempotent() {
        let raw = "ok pid=99 at 2026-02-12T07:30:59Z";
        let once = normalize_output(raw);
        let twice = normalize_output(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn capture_command_rejects_empty_command() {
        let err = capture_command(&[], &[], None).expect_err("empty command must fail");
        assert!(matches!(err, GoldenCaptureError::EmptyCommand));
    }

    #[test]
    fn capture_command_collects_stdout_stderr_and_exit_code() {
        let command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf 'out\\n'; printf 'err\\n' 1>&2; exit 7".to_string(),
        ];
        let output = capture_command(&command, &[], None).expect("capture");
        assert_eq!(output.stdout, "out\n");
        assert_eq!(output.stderr, "err\n");
        assert_eq!(output.exit_code, 7);
    }

    #[test]
    fn capture_and_normalize_command_applies_rules_to_both_streams() {
        // Use \033 (octal) instead of \x1b (hex) because /bin/sh printf doesn't support \xNN escapes
        let command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf '\\033[32mok\\033[0m pid=42\\n'; \
             printf 'ts=2026-02-12T07:30:59Z pid=100\\n' 1>&2"
                .to_string(),
        ];
        let output = capture_and_normalize_command(&command, &[], None).expect("capture");
        assert_eq!(output.stdout, "ok pid=PID\n");
        assert_eq!(output.stderr, "ts=TIMESTAMP pid=PID\n");
    }

    #[test]
    fn capture_command_with_stdin_passes_input_to_child() {
        let command = vec!["/bin/sh".to_string(), "-c".to_string(), "cat -".to_string()];
        let output = capture_command_with_stdin(&command, &[], None, Some("{\"id\":1}\n"))
            .expect("capture stdin");
        assert_eq!(output.stdout, "{\"id\":1}\n");
        assert_eq!(output.exit_code, 0);
    }

    #[test]
    fn run_golden_command_uses_expected_stream_and_env() {
        let spec = GoldenCommandSpec::new(
            "demo.txt",
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf 'out:%s\\n' \"$X\"; printf 'err:%s\\n' \"$X\" 1>&2".to_string(),
            ],
        )
        .stream(GoldenStream::Stderr)
        .env("X", "ok");
        let run = run_golden_command(&spec, &[], None).expect("run");
        assert_eq!(run.normalized_output, "err:ok\n");
        assert_eq!(run.exit_code, 0);
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn compare_text_returns_match_without_diff_when_equal() {
        let cmp = compare_text("same\ntext", "same\ntext");
        assert!(cmp.matches);
        assert!(cmp.inline_diff.is_none());
        assert_eq!(cmp.expected_sha256, cmp.actual_sha256);
    }

    #[test]
    fn compare_text_reports_hashes_and_inline_diff_when_mismatch() {
        let cmp = compare_text("alpha\nbeta\ngamma", "alpha\nBETTER\ngamma");
        assert!(!cmp.matches);
        assert_ne!(cmp.expected_sha256, cmp.actual_sha256);
        let diff = cmp.inline_diff.unwrap_or_default();
        assert!(diff.contains("@@ mismatch around line 2 @@"));
        assert!(diff.contains("-    2 | beta"));
        assert!(diff.contains("+    2 | BETTER"));
    }

    #[test]
    fn checksums_round_trip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("checksums.sha256");
        let mut checksums = BTreeMap::new();
        checksums.insert("a.txt".to_string(), sha256_hex("a"));
        checksums.insert("b.txt".to_string(), sha256_hex("b"));
        write_checksums_file(&path, &checksums).expect("write checksums");
        let loaded = read_checksums_file(&path).expect("read checksums");
        assert_eq!(loaded, checksums);
    }

    #[test]
    fn read_checksums_file_rejects_invalid_hash() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("checksums.sha256");
        std::fs::write(&path, "not-a-hash  am_help.txt\n").expect("write");
        let err = read_checksums_file(&path).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("invalid sha256 hash"));
    }

    // ── br-3h13: Additional golden.rs test coverage ──────────────────

    // ── GoldenCommandSpec builder ────────────────────────────────────

    #[test]
    fn golden_command_spec_new_defaults() {
        let spec = GoldenCommandSpec::new("test.txt", vec!["echo".to_string()]);
        assert_eq!(spec.filename, "test.txt");
        assert_eq!(spec.command, vec!["echo"]);
        assert_eq!(spec.expected_exit_code, 0);
        assert_eq!(spec.stream, GoldenStream::Stdout);
        assert!(spec.stdin.is_none());
        assert!(spec.env.is_empty());
    }

    #[test]
    fn golden_command_spec_builder_chain() {
        let spec = GoldenCommandSpec::new("out.txt", vec!["cmd".to_string()])
            .expected_exit_code(1)
            .stream(GoldenStream::Combined)
            .stdin("input data")
            .env("KEY", "VALUE")
            .env("KEY2", "VALUE2");

        assert_eq!(spec.expected_exit_code, 1);
        assert_eq!(spec.stream, GoldenStream::Combined);
        assert_eq!(spec.stdin, Some("input data".to_string()));
        assert_eq!(spec.env.len(), 2);
        assert_eq!(spec.env[0], ("KEY".to_string(), "VALUE".to_string()));
        assert_eq!(spec.env[1], ("KEY2".to_string(), "VALUE2".to_string()));
    }

    // ── GoldenStream serde ───────────────────────────────────────────

    #[test]
    fn golden_stream_serde_roundtrip_stdout() {
        let json = serde_json::to_string(&GoldenStream::Stdout).unwrap();
        assert_eq!(json, "\"stdout\"");
        let back: GoldenStream = serde_json::from_str(&json).unwrap();
        assert_eq!(back, GoldenStream::Stdout);
    }

    #[test]
    fn golden_stream_serde_roundtrip_stderr() {
        let json = serde_json::to_string(&GoldenStream::Stderr).unwrap();
        assert_eq!(json, "\"stderr\"");
        let back: GoldenStream = serde_json::from_str(&json).unwrap();
        assert_eq!(back, GoldenStream::Stderr);
    }

    #[test]
    fn golden_stream_serde_roundtrip_combined() {
        let json = serde_json::to_string(&GoldenStream::Combined).unwrap();
        assert_eq!(json, "\"combined\"");
        let back: GoldenStream = serde_json::from_str(&json).unwrap();
        assert_eq!(back, GoldenStream::Combined);
    }

    // ── GoldenCommandSpec serde ──────────────────────────────────────

    #[test]
    fn golden_command_spec_serde_roundtrip() {
        let spec = GoldenCommandSpec::new("test.golden", vec!["echo".to_string(), "hello".to_string()])
            .expected_exit_code(0)
            .stream(GoldenStream::Stdout);

        let json = serde_json::to_string(&spec).unwrap();
        let back: GoldenCommandSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn golden_command_spec_serde_omits_none_stdin() {
        let spec = GoldenCommandSpec::new("test.golden", vec!["ls".to_string()]);
        let json = serde_json::to_string(&spec).unwrap();
        assert!(!json.contains("stdin"));
    }

    #[test]
    fn golden_command_spec_serde_omits_empty_env() {
        let spec = GoldenCommandSpec::new("test.golden", vec!["ls".to_string()]);
        let json = serde_json::to_string(&spec).unwrap();
        assert!(!json.contains("env"));
    }

    // ── select_stream_output ─────────────────────────────────────────

    #[test]
    fn select_stream_output_stdout() {
        let captured = CapturedCommandOutput {
            stdout: "out\n".to_string(),
            stderr: "err\n".to_string(),
            exit_code: 0,
        };
        assert_eq!(select_stream_output(GoldenStream::Stdout, &captured), "out\n");
    }

    #[test]
    fn select_stream_output_stderr() {
        let captured = CapturedCommandOutput {
            stdout: "out\n".to_string(),
            stderr: "err\n".to_string(),
            exit_code: 0,
        };
        assert_eq!(select_stream_output(GoldenStream::Stderr, &captured), "err\n");
    }

    #[test]
    fn select_stream_output_combined() {
        let captured = CapturedCommandOutput {
            stdout: "out\n".to_string(),
            stderr: "err\n".to_string(),
            exit_code: 0,
        };
        assert_eq!(
            select_stream_output(GoldenStream::Combined, &captured),
            "out\nerr\n"
        );
    }

    // ── sha256_hex edge cases ────────────────────────────────────────

    #[test]
    fn sha256_hex_empty_string() {
        // Well-known SHA-256 of empty string
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_hex_unicode() {
        let hash = sha256_hex("日本語");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_hex_deterministic() {
        let a = sha256_hex("determinism test");
        let b = sha256_hex("determinism test");
        assert_eq!(a, b);
    }

    // ── normalize_output edge cases ──────────────────────────────────

    #[test]
    fn normalize_output_no_matches_returns_unchanged() {
        let input = "plain text with no special content";
        assert_eq!(normalize_output(input), input);
    }

    #[test]
    fn normalize_output_multiple_timestamps() {
        let input = "start=2026-01-01T00:00:00Z end=2026-12-31T23:59:59.999Z";
        let result = normalize_output(input);
        assert_eq!(result, "start=TIMESTAMP end=TIMESTAMP");
    }

    #[test]
    fn normalize_output_multiple_pids() {
        let input = "parent pid=100 child pid=200";
        let result = normalize_output(input);
        assert_eq!(result, "parent pid=PID child pid=PID");
    }

    #[test]
    fn normalize_output_empty_string() {
        assert_eq!(normalize_output(""), "");
    }

    // ── normalize_captured_output ────────────────────────────────────

    #[test]
    fn normalize_captured_output_preserves_exit_code() {
        let captured = CapturedCommandOutput {
            stdout: "pid=42\n".to_string(),
            stderr: "pid=99\n".to_string(),
            exit_code: 42,
        };
        let normalized = normalize_captured_output(&captured);
        assert_eq!(normalized.exit_code, 42);
        assert_eq!(normalized.stdout, "pid=PID\n");
        assert_eq!(normalized.stderr, "pid=PID\n");
    }

    // ── read_checksums_file edge cases ───────────────────────────────

    #[test]
    fn read_checksums_file_empty_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("checksums.sha256");
        std::fs::write(&path, "").expect("write");
        let checksums = read_checksums_file(&path).expect("read");
        assert!(checksums.is_empty());
    }

    #[test]
    fn read_checksums_file_blank_lines_skipped() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("checksums.sha256");
        let hash = sha256_hex("content");
        std::fs::write(&path, format!("\n{hash}  file.txt\n\n")).expect("write");
        let checksums = read_checksums_file(&path).expect("read");
        assert_eq!(checksums.len(), 1);
        assert_eq!(checksums["file.txt"], hash);
    }

    #[test]
    fn read_checksums_file_hash_only_no_filename() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("checksums.sha256");
        let hash = sha256_hex("x");
        // Hash with trailing whitespace: trim() removes it, leaving no separator
        std::fs::write(&path, format!("{hash}  \n")).expect("write");
        let err = read_checksums_file(&path).expect_err("must fail");
        // After trim(), the line is just the hash with no internal whitespace
        assert!(err.to_string().contains("expected"));
    }

    #[test]
    fn read_checksums_file_nonexistent_file() {
        let path = Path::new("/nonexistent/checksums.sha256");
        let err = read_checksums_file(path).expect_err("must fail");
        assert!(matches!(err, GoldenChecksumError::Io(_)));
    }

    #[test]
    fn read_checksums_file_no_separator() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("checksums.sha256");
        std::fs::write(&path, "noseparator\n").expect("write");
        let err = read_checksums_file(&path).expect_err("must fail");
        assert!(err.to_string().contains("expected"));
    }

    // ── build_inline_diff edge cases ─────────────────────────────────

    #[test]
    fn build_inline_diff_added_lines() {
        let diff = build_inline_diff("line1\n", "line1\nline2\n", 1);
        assert!(diff.contains("+"));
    }

    #[test]
    fn build_inline_diff_removed_lines() {
        let diff = build_inline_diff("line1\nline2\n", "line1\n", 1);
        assert!(diff.contains("-"));
    }

    #[test]
    fn build_inline_diff_identical_returns_context() {
        let diff = build_inline_diff("same", "same", 3);
        // Mismatch index should be at shared_len (1), diff should show context
        assert!(diff.contains("@@ mismatch around line"));
    }

    #[test]
    fn build_inline_diff_completely_different() {
        let diff = build_inline_diff("alpha\nbeta\n", "gamma\ndelta\n", 0);
        assert!(diff.contains("@@ mismatch around line 1 @@"));
        assert!(diff.contains("-    1 | alpha"));
        assert!(diff.contains("+    1 | gamma"));
    }

    // ── compare_text additional cases ────────────────────────────────

    #[test]
    fn compare_text_empty_strings() {
        let cmp = compare_text("", "");
        assert!(cmp.matches);
        assert!(cmp.inline_diff.is_none());
    }

    #[test]
    fn compare_text_one_empty() {
        let cmp = compare_text("some content", "");
        assert!(!cmp.matches);
        assert!(cmp.inline_diff.is_some());
    }

    // ── Error Display ────────────────────────────────────────────────

    #[test]
    fn golden_capture_error_empty_command_display() {
        let err = GoldenCaptureError::EmptyCommand;
        assert_eq!(err.to_string(), "command must not be empty");
    }

    #[test]
    fn golden_checksum_error_parse_display() {
        let err = GoldenChecksumError::Parse {
            line: 5,
            reason: "bad hash".to_string(),
        };
        assert!(err.to_string().contains("line 5"));
        assert!(err.to_string().contains("bad hash"));
    }

    // ── GoldenCommandRun serde ───────────────────────────────────────

    #[test]
    fn golden_command_run_serde_roundtrip() {
        let run = GoldenCommandRun {
            filename: "test.txt".to_string(),
            expected_exit_code: 0,
            exit_code: 0,
            normalized_stdout: "output\n".to_string(),
            normalized_stderr: String::new(),
            normalized_output: "output\n".to_string(),
        };
        let json = serde_json::to_string(&run).unwrap();
        let back: GoldenCommandRun = serde_json::from_str(&json).unwrap();
        assert_eq!(back, run);
    }

    // ── write_checksums_file deterministic order ─────────────────────

    #[test]
    fn write_checksums_file_deterministic_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("checksums.sha256");
        let mut checksums = BTreeMap::new();
        checksums.insert("z.txt".to_string(), sha256_hex("z"));
        checksums.insert("a.txt".to_string(), sha256_hex("a"));
        write_checksums_file(&path, &checksums).expect("write");
        let content = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = content.lines().collect();
        assert!(lines[0].contains("a.txt"));
        assert!(lines[1].contains("z.txt"));
    }
}
