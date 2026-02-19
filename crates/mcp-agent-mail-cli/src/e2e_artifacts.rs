//! E2E Artifact Writer Library for native E2E workflows.
//!
//! This module implements typed writers for E2E test artifacts, providing
//! Rust equivalents of the high-value outputs from `scripts/e2e_lib.sh`.
//!
//! Implements: `br-2ynj` (T9.2)
//!
//! # Schemas
//!
//! | Artifact | Schema | Description |
//! |----------|--------|-------------|
//! | `summary.json` | summary.v1 | Test counts, timestamps, suite name |
//! | `meta.json` | meta.v1 | Extended metadata (git, timestamps) |
//! | `metrics.json` | metrics.v1 | Performance metrics and timing |
//! | `repro.json` | repro.v1 | Deterministic replay metadata |
//! | `fixtures.json` | fixtures.v1 | Fixture identifiers |
//! | `bundle.json` | mcp-agent-mail-artifacts.1.0 | Complete manifest |
//! | `trace/events.jsonl` | trace-events.v1 | Event trace log |
//!
//! # Determinism Controls
//!
//! Environment variables for reproducible runs:
//! - `E2E_CLOCK_MODE`: `wall` (default) or `deterministic`
//! - `E2E_SEED`: RNG seed (numeric)
//! - `E2E_TIMESTAMP`: Artifact directory timestamp (YYYYmmdd_HHMMSS)
//! - `E2E_RUN_STARTED_AT`: RFC3339 logical start time
//! - `E2E_RUN_START_EPOCH_S`: Epoch seconds for logical clock

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ──────────────────────────────────────────────────────────────────────────────
// Clock Mode
// ──────────────────────────────────────────────────────────────────────────────

/// Clock mode for E2E runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ClockMode {
    /// Wall clock time (default).
    #[default]
    Wall,
    /// Deterministic time derived from seed.
    Deterministic,
}

impl ClockMode {
    /// Parse from string (case-insensitive).
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "deterministic" => Self::Deterministic,
            _ => Self::Wall,
        }
    }

    /// Returns the string representation.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Wall => "wall",
            Self::Deterministic => "deterministic",
        }
    }
}

impl std::fmt::Display for ClockMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Deterministic RNG
// ──────────────────────────────────────────────────────────────────────────────

/// Deterministic RNG using LCG (glibc constants).
///
/// NOT cryptographically secure. For stable ID generation only.
#[derive(Debug, Clone)]
pub struct SeededRng {
    state: u64,
}

impl SeededRng {
    /// Creates a new RNG with the given seed.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed & 0x7fff_ffff,
        }
    }

    /// Returns the next pseudo-random u32.
    pub fn next_u32(&mut self) -> u32 {
        // LCG with glibc constants, masked to 31 bits
        self.state = (1_103_515_245u64
            .wrapping_mul(self.state)
            .wrapping_add(12345))
            & 0x7fff_ffff;
        self.state as u32
    }

    /// Returns a hex string (8 chars).
    pub fn next_hex(&mut self) -> String {
        format!("{:08x}", self.next_u32())
    }

    /// Returns a seeded ID with prefix.
    pub fn next_id(&mut self, prefix: &str) -> String {
        format!("{}_{}", prefix, self.next_hex())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Test Counters
// ──────────────────────────────────────────────────────────────────────────────

/// Thread-safe test counters.
#[derive(Debug, Default)]
pub struct TestCounters {
    total: AtomicU64,
    pass: AtomicU64,
    fail: AtomicU64,
    skip: AtomicU64,
}

impl TestCounters {
    /// Creates new zeroed counters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Increments total and returns new value.
    pub fn inc_total(&self) -> u64 {
        self.total.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Increments pass count.
    pub fn inc_pass(&self) {
        self.pass.fetch_add(1, Ordering::SeqCst);
    }

    /// Increments fail count.
    pub fn inc_fail(&self) {
        self.fail.fetch_add(1, Ordering::SeqCst);
    }

    /// Increments skip count.
    pub fn inc_skip(&self) {
        self.skip.fetch_add(1, Ordering::SeqCst);
    }

    /// Returns current counts snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Counts {
        Counts {
            total: self.total.load(Ordering::SeqCst),
            pass: self.pass.load(Ordering::SeqCst),
            fail: self.fail.load(Ordering::SeqCst),
            skip: self.skip.load(Ordering::SeqCst),
        }
    }
}

/// Immutable counts snapshot.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Counts {
    pub total: u64,
    pub pass: u64,
    pub fail: u64,
    pub skip: u64,
}

// ──────────────────────────────────────────────────────────────────────────────
// Git Metadata
// ──────────────────────────────────────────────────────────────────────────────

/// Git repository metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitInfo {
    pub commit: String,
    pub branch: String,
    pub dirty: bool,
}

impl GitInfo {
    /// Captures git info from the current directory.
    #[must_use]
    pub fn capture() -> Self {
        let commit = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let dirty = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .output()
            .ok()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false);

        Self {
            commit,
            branch,
            dirty,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Schema: summary.v1
// ──────────────────────────────────────────────────────────────────────────────

/// Summary artifact (summary.v1 schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub schema_version: u32,
    pub suite: String,
    pub timestamp: String,
    pub started_at: String,
    pub ended_at: String,
    pub total: u64,
    pub pass: u64,
    pub fail: u64,
    pub skip: u64,
}

impl Summary {
    /// Creates a summary from run context and counts.
    #[must_use]
    pub fn new(ctx: &RunContext, counts: &Counts, ended_at: &str) -> Self {
        Self {
            schema_version: 1,
            suite: ctx.suite.clone(),
            timestamp: ctx.timestamp.clone(),
            started_at: ctx.started_at.clone(),
            ended_at: ended_at.to_string(),
            total: counts.total,
            pass: counts.pass,
            fail: counts.fail,
            skip: counts.skip,
        }
    }

    /// Writes to file.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let file = File::create(path)?;
        serde_json::to_writer_pretty(file, self)?;
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Schema: meta.v1
// ──────────────────────────────────────────────────────────────────────────────

/// Meta artifact (meta.v1 schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub schema_version: u32,
    pub suite: String,
    pub timestamp: String,
    pub started_at: String,
    pub ended_at: String,
    pub git: GitInfo,
    pub determinism: DeterminismInfo,
}

/// Determinism metadata for reproducibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeterminismInfo {
    pub clock_mode: String,
    pub seed: u64,
    pub run_start_epoch_s: i64,
}

impl Meta {
    /// Creates metadata from run context.
    #[must_use]
    pub fn new(ctx: &RunContext, ended_at: &str, git: GitInfo) -> Self {
        Self {
            schema_version: 1,
            suite: ctx.suite.clone(),
            timestamp: ctx.timestamp.clone(),
            started_at: ctx.started_at.clone(),
            ended_at: ended_at.to_string(),
            git,
            determinism: DeterminismInfo {
                clock_mode: ctx.clock_mode.to_string(),
                seed: ctx.seed,
                run_start_epoch_s: ctx.start_epoch_s,
            },
        }
    }

    /// Writes to file.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let file = File::create(path)?;
        serde_json::to_writer_pretty(file, self)?;
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Schema: metrics.v1
// ──────────────────────────────────────────────────────────────────────────────

/// Metrics artifact (metrics.v1 schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metrics {
    pub schema_version: u32,
    pub suite: String,
    pub timestamp: String,
    pub timing: TimingInfo,
    pub counts: Counts,
}

/// Timing information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimingInfo {
    pub start_epoch_s: i64,
    pub end_epoch_s: i64,
    pub duration_s: i64,
}

impl Metrics {
    /// Creates metrics from run context and counts.
    #[must_use]
    pub fn new(ctx: &RunContext, counts: &Counts, end_epoch_s: i64) -> Self {
        let duration_s = end_epoch_s.saturating_sub(ctx.start_epoch_s).max(0);
        Self {
            schema_version: 1,
            suite: ctx.suite.clone(),
            timestamp: ctx.timestamp.clone(),
            timing: TimingInfo {
                start_epoch_s: ctx.start_epoch_s,
                end_epoch_s,
                duration_s,
            },
            counts: *counts,
        }
    }

    /// Writes to file.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let file = File::create(path)?;
        serde_json::to_writer_pretty(file, self)?;
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Schema: repro.v1
// ──────────────────────────────────────────────────────────────────────────────

/// Repro artifact (repro.v1 schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repro {
    pub schema_version: u32,
    pub suite: String,
    pub timestamp: String,
    pub clock_mode: String,
    pub seed: u64,
    pub run_started_at: String,
    pub run_start_epoch_s: i64,
    pub command: String,
}

impl Repro {
    /// Creates repro metadata from run context.
    #[must_use]
    pub fn new(ctx: &RunContext, project_root: &Path) -> Self {
        let command = format!(
            "cd {} && AM_E2E_KEEP_TMP=1 E2E_CLOCK_MODE={} E2E_SEED={} E2E_RUN_STARTED_AT='{}' E2E_RUN_START_EPOCH_S={} ./scripts/e2e_test.sh {}",
            project_root.display(),
            ctx.clock_mode,
            ctx.seed,
            ctx.started_at,
            ctx.start_epoch_s,
            ctx.suite
        );

        Self {
            schema_version: 1,
            suite: ctx.suite.clone(),
            timestamp: ctx.timestamp.clone(),
            clock_mode: ctx.clock_mode.to_string(),
            seed: ctx.seed,
            run_started_at: ctx.started_at.clone(),
            run_start_epoch_s: ctx.start_epoch_s,
            command,
        }
    }

    /// Writes to file.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let file = File::create(path)?;
        serde_json::to_writer_pretty(file, self)?;
        Ok(())
    }

    /// Writes human-readable repro.txt.
    pub fn write_txt(&self, path: &Path) -> std::io::Result<()> {
        fs::write(path, &self.command)?;
        Ok(())
    }

    /// Writes sourceable repro.env.
    pub fn write_env(&self, path: &Path) -> std::io::Result<()> {
        let content = format!(
            "export E2E_CLOCK_MODE='{}'\nexport E2E_SEED='{}'\nexport E2E_RUN_STARTED_AT='{}'\nexport E2E_RUN_START_EPOCH_S='{}'\nexport E2E_SUITE='{}'\n",
            self.clock_mode, self.seed, self.run_started_at, self.run_start_epoch_s, self.suite
        );
        fs::write(path, content)?;
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Schema: fixtures.v1
// ──────────────────────────────────────────────────────────────────────────────

/// Fixtures artifact (fixtures.v1 schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fixtures {
    pub schema_version: u32,
    pub suite: String,
    pub timestamp: String,
    pub fixture_ids: Vec<String>,
}

impl Fixtures {
    /// Creates fixtures from run context and IDs.
    #[must_use]
    pub fn new(ctx: &RunContext, fixture_ids: Vec<String>) -> Self {
        Self {
            schema_version: 1,
            suite: ctx.suite.clone(),
            timestamp: ctx.timestamp.clone(),
            fixture_ids,
        }
    }

    /// Writes to file.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let file = File::create(path)?;
        serde_json::to_writer_pretty(file, self)?;
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Schema: trace-events.v1
// ──────────────────────────────────────────────────────────────────────────────

/// A single trace event (trace-events.v1 schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEvent {
    pub schema_version: u32,
    pub suite: String,
    pub run_timestamp: String,
    pub ts: String,
    pub kind: String,
    pub case: String,
    pub message: String,
    pub counters: Counts,
}

impl TraceEvent {
    /// Serializes to JSONL format (single line, no trailing newline).
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Trace event writer (JSONL format).
pub struct TraceWriter {
    writer: BufWriter<File>,
    ctx: RunContext,
}

impl TraceWriter {
    /// Creates a new trace writer.
    pub fn new(path: &Path, ctx: RunContext) -> std::io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            ctx,
        })
    }

    /// Writes a trace event.
    pub fn write_event(
        &mut self,
        kind: &str,
        case: &str,
        message: &str,
        counters: &Counts,
    ) -> std::io::Result<()> {
        let event = TraceEvent {
            schema_version: 1,
            suite: self.ctx.suite.clone(),
            run_timestamp: self.ctx.timestamp.clone(),
            ts: Utc::now().to_rfc3339(),
            kind: kind.to_string(),
            case: case.to_string(),
            message: message.to_string(),
            counters: *counters,
        };
        writeln!(self.writer, "{}", event.to_jsonl())?;
        self.writer.flush()?;
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Schema: mcp-agent-mail-artifacts.1.0 (bundle.json)
// ──────────────────────────────────────────────────────────────────────────────

/// Bundle schema info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleSchema {
    pub name: String,
    pub major: u32,
    pub minor: u32,
}

/// File entry in bundle manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

/// Artifact reference in bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

/// Artifacts section of bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleArtifacts {
    pub metadata: ArtifactRef,
    pub metrics: ArtifactRef,
    pub summary: ArtifactRef,
    pub diagnostics: HashMap<String, ArtifactRef>,
    pub trace: HashMap<String, ArtifactRef>,
    pub logs: HashMap<String, ArtifactRef>,
    pub screenshots: HashMap<String, ArtifactRef>,
    pub fixtures: ArtifactRef,
    pub replay: HashMap<String, ArtifactRef>,
}

/// Bundle manifest (mcp-agent-mail-artifacts.1.0 schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub schema: BundleSchema,
    pub suite: String,
    pub timestamp: String,
    pub generated_at: String,
    pub started_at: String,
    pub ended_at: String,
    pub counts: Counts,
    pub git: GitInfo,
    pub artifacts: BundleArtifacts,
    pub files: Vec<BundleFile>,
}

impl Bundle {
    /// Creates a bundle manifest for the given artifact directory.
    pub fn create(
        artifact_dir: &Path,
        ctx: &RunContext,
        counts: &Counts,
        ended_at: &str,
        git: GitInfo,
    ) -> std::io::Result<Self> {
        let generated_at = Utc::now().to_rfc3339();

        let artifacts = BundleArtifacts {
            metadata: ArtifactRef {
                path: "meta.json".to_string(),
                schema: Some("meta.v1".to_string()),
            },
            metrics: ArtifactRef {
                path: "metrics.json".to_string(),
                schema: Some("metrics.v1".to_string()),
            },
            summary: ArtifactRef {
                path: "summary.json".to_string(),
                schema: Some("summary.v1".to_string()),
            },
            diagnostics: {
                let mut m = HashMap::new();
                m.insert(
                    "env_redacted".to_string(),
                    ArtifactRef {
                        path: "diagnostics/env_redacted.txt".to_string(),
                        schema: None,
                    },
                );
                m.insert(
                    "tree".to_string(),
                    ArtifactRef {
                        path: "diagnostics/tree.txt".to_string(),
                        schema: None,
                    },
                );
                m
            },
            trace: {
                let mut m = HashMap::new();
                m.insert(
                    "events".to_string(),
                    ArtifactRef {
                        path: "trace/events.jsonl".to_string(),
                        schema: Some("trace-events.v1".to_string()),
                    },
                );
                m
            },
            logs: {
                let mut m = HashMap::new();
                m.insert(
                    "index".to_string(),
                    ArtifactRef {
                        path: "logs/index.json".to_string(),
                        schema: Some("logs-index.v1".to_string()),
                    },
                );
                m
            },
            screenshots: {
                let mut m = HashMap::new();
                m.insert(
                    "index".to_string(),
                    ArtifactRef {
                        path: "screenshots/index.json".to_string(),
                        schema: Some("screenshots-index.v1".to_string()),
                    },
                );
                m
            },
            fixtures: ArtifactRef {
                path: "fixtures.json".to_string(),
                schema: Some("fixtures.v1".to_string()),
            },
            replay: {
                let mut m = HashMap::new();
                m.insert(
                    "command".to_string(),
                    ArtifactRef {
                        path: "repro.txt".to_string(),
                        schema: None,
                    },
                );
                m.insert(
                    "environment".to_string(),
                    ArtifactRef {
                        path: "repro.env".to_string(),
                        schema: None,
                    },
                );
                m.insert(
                    "metadata".to_string(),
                    ArtifactRef {
                        path: "repro.json".to_string(),
                        schema: Some("repro.v1".to_string()),
                    },
                );
                m
            },
        };

        // Collect files with hashes
        let mut files = Vec::new();
        for entry in walkdir::WalkDir::new(artifact_dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            let rel_path = path.strip_prefix(artifact_dir).unwrap_or(path);
            let rel_str = rel_path.to_string_lossy().to_string();

            // Skip bundle.json itself
            if rel_str == "bundle.json" {
                continue;
            }

            let bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let sha256 = compute_sha256(path).unwrap_or_default();
            let (kind, schema) = classify_artifact(&rel_str);

            files.push(BundleFile {
                path: rel_str,
                sha256,
                bytes,
                kind,
                schema,
            });
        }

        // Sort files for determinism
        files.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(Self {
            schema: BundleSchema {
                name: "mcp-agent-mail-artifacts".to_string(),
                major: 1,
                minor: 0,
            },
            suite: ctx.suite.clone(),
            timestamp: ctx.timestamp.clone(),
            generated_at,
            started_at: ctx.started_at.clone(),
            ended_at: ended_at.to_string(),
            counts: *counts,
            git,
            artifacts,
            files,
        })
    }

    /// Writes to file.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let file = File::create(path)?;
        serde_json::to_writer_pretty(file, self)?;
        Ok(())
    }
}

/// Computes SHA256 hash of a file.
fn compute_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Classifies an artifact file by path.
fn classify_artifact(path: &str) -> (String, Option<String>) {
    match path {
        "summary.json" => ("metrics".to_string(), Some("summary.v1".to_string())),
        "meta.json" => ("metadata".to_string(), Some("meta.v1".to_string())),
        "metrics.json" => ("metrics".to_string(), Some("metrics.v1".to_string())),
        "trace/events.jsonl" => ("trace".to_string(), Some("trace-events.v1".to_string())),
        "logs/index.json" => ("logs".to_string(), Some("logs-index.v1".to_string())),
        "screenshots/index.json" => (
            "screenshots".to_string(),
            Some("screenshots-index.v1".to_string()),
        ),
        "fixtures.json" => ("fixture".to_string(), Some("fixtures.v1".to_string())),
        "repro.json" => ("replay".to_string(), Some("repro.v1".to_string())),
        "repro.txt" | "repro.env" => ("replay".to_string(), None),
        _ if path.starts_with("diagnostics/") => ("diagnostics".to_string(), None),
        _ if path.starts_with("logs/") => ("logs".to_string(), None),
        _ if path.starts_with("screenshots/") => ("screenshots".to_string(), None),
        _ if path.starts_with("trace/") => ("trace".to_string(), None),
        _ if path.starts_with("transcript/") => ("transcript".to_string(), None),
        _ => ("opaque".to_string(), None),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Run Context
// ──────────────────────────────────────────────────────────────────────────────

/// Context for an E2E test run.
#[derive(Debug, Clone)]
pub struct RunContext {
    pub suite: String,
    pub timestamp: String,
    pub clock_mode: ClockMode,
    pub seed: u64,
    pub started_at: String,
    pub start_epoch_s: i64,
}

impl RunContext {
    /// Creates a new run context from environment variables.
    #[must_use]
    pub fn from_env(suite: &str) -> Self {
        let clock_mode = std::env::var("E2E_CLOCK_MODE")
            .map(|s| ClockMode::from_str_lossy(&s))
            .unwrap_or_default();

        let timestamp = std::env::var("E2E_TIMESTAMP")
            .unwrap_or_else(|_| Utc::now().format("%Y%m%d_%H%M%S").to_string());

        let seed = std::env::var("E2E_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                // Default: numeric form of timestamp
                timestamp.replace('_', "").parse().unwrap_or(0)
            });

        let (started_at, start_epoch_s) = if clock_mode == ClockMode::Deterministic {
            let epoch = std::env::var("E2E_RUN_START_EPOCH_S")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| 1_700_000_000 + (seed as i64 % 86400));

            let started = std::env::var("E2E_RUN_STARTED_AT").unwrap_or_else(|_| {
                DateTime::from_timestamp(epoch, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_else(|| Utc::now().to_rfc3339())
            });

            (started, epoch)
        } else {
            let now = Utc::now();
            let started = std::env::var("E2E_RUN_STARTED_AT").unwrap_or_else(|_| now.to_rfc3339());
            let epoch = std::env::var("E2E_RUN_START_EPOCH_S")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| now.timestamp());

            (started, epoch)
        };

        Self {
            suite: suite.to_string(),
            timestamp,
            clock_mode,
            seed,
            started_at,
            start_epoch_s,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Artifact Directory Manager
// ──────────────────────────────────────────────────────────────────────────────

/// Manages artifact directory creation and writing.
pub struct ArtifactManager {
    dir: PathBuf,
    ctx: RunContext,
    counters: TestCounters,
    fixture_ids: Vec<String>,
    project_root: PathBuf,
}

impl ArtifactManager {
    /// Creates a new artifact manager.
    ///
    /// # Arguments
    /// * `base_dir` - Base artifacts directory (e.g., `tests/artifacts`)
    /// * `suite` - Suite name
    /// * `project_root` - Project root for repro commands
    pub fn new(base_dir: &Path, suite: &str, project_root: &Path) -> std::io::Result<Self> {
        let ctx = RunContext::from_env(suite);
        let dir = base_dir.join(suite).join(&ctx.timestamp);

        // Create directory structure
        fs::create_dir_all(&dir)?;
        fs::create_dir_all(dir.join("trace"))?;
        fs::create_dir_all(dir.join("logs"))?;
        fs::create_dir_all(dir.join("screenshots"))?;
        fs::create_dir_all(dir.join("diagnostics"))?;
        fs::create_dir_all(dir.join("transcript"))?;

        Ok(Self {
            dir,
            ctx,
            counters: TestCounters::new(),
            fixture_ids: Vec::new(),
            project_root: project_root.to_path_buf(),
        })
    }

    /// Returns the artifact directory path.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the run context.
    #[must_use]
    pub fn context(&self) -> &RunContext {
        &self.ctx
    }

    /// Returns the test counters.
    #[must_use]
    pub fn counters(&self) -> &TestCounters {
        &self.counters
    }

    /// Adds a fixture ID.
    pub fn add_fixture_id(&mut self, id: String) {
        self.fixture_ids.push(id);
    }

    /// Creates a trace writer for this run.
    pub fn trace_writer(&self) -> std::io::Result<TraceWriter> {
        TraceWriter::new(&self.dir.join("trace/events.jsonl"), self.ctx.clone())
    }

    /// Writes all final artifacts.
    pub fn finalize(&self) -> std::io::Result<()> {
        let now = Utc::now();
        let ended_at = now.to_rfc3339();
        let end_epoch_s = now.timestamp();
        let counts = self.counters.snapshot();
        let git = GitInfo::capture();

        // Write summary.json
        let summary = Summary::new(&self.ctx, &counts, &ended_at);
        summary.write_to(&self.dir.join("summary.json"))?;

        // Write meta.json
        let meta = Meta::new(&self.ctx, &ended_at, git.clone());
        meta.write_to(&self.dir.join("meta.json"))?;

        // Write metrics.json
        let metrics = Metrics::new(&self.ctx, &counts, end_epoch_s);
        metrics.write_to(&self.dir.join("metrics.json"))?;

        // Write repro.json and friends
        let repro = Repro::new(&self.ctx, &self.project_root);
        repro.write_to(&self.dir.join("repro.json"))?;
        repro.write_txt(&self.dir.join("repro.txt"))?;
        repro.write_env(&self.dir.join("repro.env"))?;

        // Write fixtures.json
        let fixtures = Fixtures::new(&self.ctx, self.fixture_ids.clone());
        fixtures.write_to(&self.dir.join("fixtures.json"))?;

        // Write empty index files if they don't exist
        self.ensure_index_file("logs/index.json", "logs-index.v1")?;
        self.ensure_index_file("screenshots/index.json", "screenshots-index.v1")?;

        // Write bundle.json (must be last - includes hashes of all other files)
        let bundle = Bundle::create(&self.dir, &self.ctx, &counts, &ended_at, git)?;
        bundle.write_to(&self.dir.join("bundle.json"))?;

        Ok(())
    }

    /// Ensures an index file exists with minimal schema.
    fn ensure_index_file(&self, rel_path: &str, schema: &str) -> std::io::Result<()> {
        let path = self.dir.join(rel_path);
        if !path.exists() {
            let content = serde_json::json!({
                "schema_version": 1,
                "schema": schema,
                "files": []
            });
            let file = File::create(path)?;
            serde_json::to_writer_pretty(file, &content)?;
        }
        Ok(())
    }

    /// Writes environment dump with secrets redacted.
    pub fn write_env_redacted(&self) -> std::io::Result<()> {
        let path = self.dir.join("diagnostics/env_redacted.txt");
        let mut file = File::create(path)?;

        for (key, value) in std::env::vars() {
            let redacted = if key.contains("TOKEN")
                || key.contains("SECRET")
                || key.contains("PASSWORD")
                || key.contains("KEY")
                || key.contains("CREDENTIAL")
            {
                "[REDACTED]".to_string()
            } else {
                value
            };
            writeln!(file, "{}={}", key, redacted)?;
        }
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_mode_parse() {
        assert_eq!(ClockMode::from_str_lossy("wall"), ClockMode::Wall);
        assert_eq!(ClockMode::from_str_lossy("WALL"), ClockMode::Wall);
        assert_eq!(
            ClockMode::from_str_lossy("deterministic"),
            ClockMode::Deterministic
        );
        assert_eq!(
            ClockMode::from_str_lossy("DETERMINISTIC"),
            ClockMode::Deterministic
        );
        assert_eq!(ClockMode::from_str_lossy("unknown"), ClockMode::Wall);
    }

    #[test]
    fn test_seeded_rng_determinism() {
        let mut rng1 = SeededRng::new(12345);
        let mut rng2 = SeededRng::new(12345);

        for _ in 0..100 {
            assert_eq!(rng1.next_u32(), rng2.next_u32());
        }
    }

    #[test]
    fn test_seeded_rng_hex() {
        let mut rng = SeededRng::new(42);
        let hex = rng.next_hex();
        assert_eq!(hex.len(), 8);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_test_counters() {
        let counters = TestCounters::new();
        assert_eq!(counters.inc_total(), 1);
        assert_eq!(counters.inc_total(), 2);
        counters.inc_pass();
        counters.inc_fail();
        counters.inc_skip();

        let snap = counters.snapshot();
        assert_eq!(snap.total, 2);
        assert_eq!(snap.pass, 1);
        assert_eq!(snap.fail, 1);
        assert_eq!(snap.skip, 1);
    }

    #[test]
    fn test_classify_artifact() {
        assert_eq!(
            classify_artifact("summary.json"),
            ("metrics".to_string(), Some("summary.v1".to_string()))
        );
        assert_eq!(classify_artifact("repro.txt"), ("replay".to_string(), None));
        assert_eq!(
            classify_artifact("unknown.dat"),
            ("opaque".to_string(), None)
        );
    }

    #[test]
    fn test_summary_serialization() {
        let ctx = RunContext {
            suite: "test_suite".to_string(),
            timestamp: "20260212_120000".to_string(),
            clock_mode: ClockMode::Wall,
            seed: 12345,
            started_at: "2026-02-12T12:00:00Z".to_string(),
            start_epoch_s: 1771070400,
        };
        let counts = Counts {
            total: 10,
            pass: 8,
            fail: 1,
            skip: 1,
        };
        let summary = Summary::new(&ctx, &counts, "2026-02-12T12:01:00Z");

        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"schema_version\":1"));
        assert!(json.contains("\"suite\":\"test_suite\""));
    }

    // ── ClockMode ──────────────────────────────────────────────────────

    #[test]
    fn clock_mode_default_is_wall() {
        assert_eq!(ClockMode::default(), ClockMode::Wall);
    }

    #[test]
    fn clock_mode_as_str() {
        assert_eq!(ClockMode::Wall.as_str(), "wall");
        assert_eq!(ClockMode::Deterministic.as_str(), "deterministic");
    }

    #[test]
    fn clock_mode_display() {
        assert_eq!(format!("{}", ClockMode::Wall), "wall");
        assert_eq!(format!("{}", ClockMode::Deterministic), "deterministic");
    }

    #[test]
    fn clock_mode_serde_roundtrip() {
        let wall: ClockMode = serde_json::from_str("\"wall\"").unwrap();
        assert_eq!(wall, ClockMode::Wall);
        let det: ClockMode = serde_json::from_str("\"deterministic\"").unwrap();
        assert_eq!(det, ClockMode::Deterministic);
        assert_eq!(serde_json::to_string(&ClockMode::Wall).unwrap(), "\"wall\"");
    }

    // ── SeededRng ──────────────────────────────────────────────────────

    #[test]
    fn seeded_rng_different_seeds_differ() {
        let mut r1 = SeededRng::new(1);
        let mut r2 = SeededRng::new(2);
        // At least one of the first 5 values should differ
        let vals1: Vec<u32> = (0..5).map(|_| r1.next_u32()).collect();
        let vals2: Vec<u32> = (0..5).map(|_| r2.next_u32()).collect();
        assert_ne!(vals1, vals2);
    }

    #[test]
    fn seeded_rng_next_id_format() {
        let mut rng = SeededRng::new(99);
        let id = rng.next_id("test");
        assert!(id.starts_with("test_"));
        assert_eq!(id.len(), 5 + 8); // "test_" + 8 hex chars
    }

    #[test]
    fn seeded_rng_state_masked_to_31_bits() {
        let rng = SeededRng::new(u64::MAX);
        // state should be masked
        assert!(rng.state <= 0x7fff_ffff);
    }

    // ── TestCounters ───────────────────────────────────────────────────

    #[test]
    fn test_counters_default() {
        let c = TestCounters::default();
        let snap = c.snapshot();
        assert_eq!(snap.total, 0);
        assert_eq!(snap.pass, 0);
        assert_eq!(snap.fail, 0);
        assert_eq!(snap.skip, 0);
    }

    // ── Counts ─────────────────────────────────────────────────────────

    #[test]
    fn counts_default() {
        let c = Counts::default();
        assert_eq!(c.total, 0);
        assert_eq!(c.pass, 0);
    }

    #[test]
    fn counts_serde_roundtrip() {
        let c = Counts {
            total: 10,
            pass: 7,
            fail: 2,
            skip: 1,
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: Counts = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total, c.total);
        assert_eq!(back.pass, c.pass);
        assert_eq!(back.fail, c.fail);
        assert_eq!(back.skip, c.skip);
    }

    // ── GitInfo ────────────────────────────────────────────────────────

    #[test]
    fn git_info_default() {
        let g = GitInfo::default();
        assert!(g.commit.is_empty());
        assert!(g.branch.is_empty());
        assert!(!g.dirty);
    }

    #[test]
    fn git_info_serde_roundtrip() {
        let g = GitInfo {
            commit: "abc123".to_string(),
            branch: "main".to_string(),
            dirty: true,
        };
        let json = serde_json::to_string(&g).unwrap();
        let back: GitInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.commit, "abc123");
        assert!(back.dirty);
    }

    // ── Summary ────────────────────────────────────────────────────────

    #[test]
    fn summary_fields_populated() {
        let ctx = RunContext {
            suite: "s".to_string(),
            timestamp: "ts".to_string(),
            clock_mode: ClockMode::Wall,
            seed: 0,
            started_at: "start".to_string(),
            start_epoch_s: 100,
        };
        let counts = Counts {
            total: 5,
            pass: 4,
            fail: 1,
            skip: 0,
        };
        let s = Summary::new(&ctx, &counts, "end");
        assert_eq!(s.schema_version, 1);
        assert_eq!(s.suite, "s");
        assert_eq!(s.ended_at, "end");
        assert_eq!(s.total, 5);
    }

    // ── Meta ───────────────────────────────────────────────────────────

    #[test]
    fn meta_includes_determinism_info() {
        let ctx = RunContext {
            suite: "suite".to_string(),
            timestamp: "ts".to_string(),
            clock_mode: ClockMode::Deterministic,
            seed: 42,
            started_at: "start".to_string(),
            start_epoch_s: 1700000000,
        };
        let git = GitInfo::default();
        let m = Meta::new(&ctx, "end", git);
        assert_eq!(m.determinism.clock_mode, "deterministic");
        assert_eq!(m.determinism.seed, 42);
        assert_eq!(m.determinism.run_start_epoch_s, 1700000000);
    }

    // ── Metrics ────────────────────────────────────────────────────────

    #[test]
    fn metrics_duration_calculation() {
        let ctx = RunContext {
            suite: "s".to_string(),
            timestamp: "ts".to_string(),
            clock_mode: ClockMode::Wall,
            seed: 0,
            started_at: "start".to_string(),
            start_epoch_s: 100,
        };
        let counts = Counts::default();
        let m = Metrics::new(&ctx, &counts, 150);
        assert_eq!(m.timing.duration_s, 50);
        assert_eq!(m.timing.start_epoch_s, 100);
        assert_eq!(m.timing.end_epoch_s, 150);
    }

    #[test]
    fn metrics_negative_duration_clamped_to_zero() {
        let ctx = RunContext {
            suite: "s".to_string(),
            timestamp: "ts".to_string(),
            clock_mode: ClockMode::Wall,
            seed: 0,
            started_at: "start".to_string(),
            start_epoch_s: 200,
        };
        let counts = Counts::default();
        let m = Metrics::new(&ctx, &counts, 100); // end < start
        assert_eq!(m.timing.duration_s, 0);
    }

    // ── Repro ──────────────────────────────────────────────────────────

    #[test]
    fn repro_command_contains_env_vars() {
        let ctx = RunContext {
            suite: "my_suite".to_string(),
            timestamp: "20260101_000000".to_string(),
            clock_mode: ClockMode::Deterministic,
            seed: 777,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            start_epoch_s: 1700000000,
        };
        let r = Repro::new(&ctx, Path::new("/project"));
        assert!(r.command.contains("E2E_CLOCK_MODE=deterministic"));
        assert!(r.command.contains("E2E_SEED=777"));
        assert!(r.command.contains("my_suite"));
        assert!(r.command.contains("/project"));
    }

    // ── Fixtures ───────────────────────────────────────────────────────

    #[test]
    fn fixtures_schema_and_ids() {
        let ctx = RunContext {
            suite: "s".to_string(),
            timestamp: "ts".to_string(),
            clock_mode: ClockMode::Wall,
            seed: 0,
            started_at: "start".to_string(),
            start_epoch_s: 0,
        };
        let f = Fixtures::new(&ctx, vec!["id1".to_string(), "id2".to_string()]);
        assert_eq!(f.schema_version, 1);
        assert_eq!(f.fixture_ids.len(), 2);
    }

    // ── TraceEvent ─────────────────────────────────────────────────────

    #[test]
    fn trace_event_to_jsonl_is_single_line() {
        let e = TraceEvent {
            schema_version: 1,
            suite: "s".to_string(),
            run_timestamp: "ts".to_string(),
            ts: "now".to_string(),
            kind: "pass".to_string(),
            case: "test_case".to_string(),
            message: "ok".to_string(),
            counters: Counts {
                total: 1,
                pass: 1,
                fail: 0,
                skip: 0,
            },
        };
        let line = e.to_jsonl();
        assert!(!line.contains('\n'));
        assert!(line.contains("\"kind\":\"pass\""));
        assert!(line.contains("\"case\":\"test_case\""));
    }

    // ── classify_artifact additional paths ─────────────────────────────

    #[test]
    fn classify_artifact_known_paths() {
        assert_eq!(classify_artifact("meta.json").0, "metadata");
        assert_eq!(classify_artifact("metrics.json").0, "metrics");
        assert_eq!(classify_artifact("fixtures.json").0, "fixture");
        assert_eq!(classify_artifact("repro.json").0, "replay");
        assert_eq!(classify_artifact("repro.env").0, "replay");
        assert_eq!(classify_artifact("trace/events.jsonl").0, "trace");
        assert_eq!(classify_artifact("logs/index.json").0, "logs");
        assert_eq!(classify_artifact("screenshots/index.json").0, "screenshots");
    }

    #[test]
    fn classify_artifact_prefix_paths() {
        assert_eq!(classify_artifact("diagnostics/env.txt").0, "diagnostics");
        assert_eq!(classify_artifact("logs/extra.log").0, "logs");
        assert_eq!(classify_artifact("screenshots/shot.png").0, "screenshots");
        assert_eq!(classify_artifact("trace/extra.jsonl").0, "trace");
        assert_eq!(classify_artifact("transcript/session.txt").0, "transcript");
    }

    // ── DeterminismInfo serde ──────────────────────────────────────────

    #[test]
    fn determinism_info_serde_roundtrip() {
        let d = DeterminismInfo {
            clock_mode: "deterministic".to_string(),
            seed: 42,
            run_start_epoch_s: 1700000000,
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: DeterminismInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seed, 42);
        assert_eq!(back.clock_mode, "deterministic");
    }

    // ── BundleSchema / ArtifactRef ─────────────────────────────────────

    #[test]
    fn bundle_schema_serde() {
        let s = BundleSchema {
            name: "mcp-agent-mail-artifacts".to_string(),
            major: 1,
            minor: 0,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"major\":1"));
        let back: BundleSchema = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "mcp-agent-mail-artifacts");
    }

    #[test]
    fn artifact_ref_optional_schema() {
        let with = ArtifactRef {
            path: "p".to_string(),
            schema: Some("v1".to_string()),
        };
        let json_with = serde_json::to_string(&with).unwrap();
        assert!(json_with.contains("\"schema\""));

        let without = ArtifactRef {
            path: "p".to_string(),
            schema: None,
        };
        let json_without = serde_json::to_string(&without).unwrap();
        assert!(!json_without.contains("\"schema\""));
    }

    // ── BundleFile optional schema ─────────────────────────────────────

    #[test]
    fn bundle_file_optional_schema_skip() {
        let f = BundleFile {
            path: "test.txt".to_string(),
            sha256: "abc".to_string(),
            bytes: 100,
            kind: "opaque".to_string(),
            schema: None,
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(!json.contains("\"schema\""));
    }

    // ── TimingInfo serde ───────────────────────────────────────────────

    #[test]
    fn timing_info_serde_roundtrip() {
        let t = TimingInfo {
            start_epoch_s: 100,
            end_epoch_s: 200,
            duration_s: 100,
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: TimingInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.duration_s, 100);
    }
}
