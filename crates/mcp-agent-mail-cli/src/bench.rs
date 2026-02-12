//! Benchmark domain models for the `am bench` foundation.
//!
//! This module provides typed contracts for benchmark configuration, execution
//! results, summary aggregation, and deterministic fixture identity.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Current JSON schema version for benchmark summary artifacts.
pub const BENCH_SCHEMA_VERSION: u32 = 1;

/// Default warmup iterations for a normal benchmark run.
pub const DEFAULT_WARMUP: u32 = 3;
/// Default measured iterations for a normal benchmark run.
pub const DEFAULT_RUNS: u32 = 10;
/// Warmup iterations for `--quick`.
pub const QUICK_WARMUP: u32 = 1;
/// Measured iterations for `--quick`.
pub const QUICK_RUNS: u32 = 3;

fn default_warmup() -> u32 {
    DEFAULT_WARMUP
}

fn default_runs() -> u32 {
    DEFAULT_RUNS
}

fn always_condition() -> BenchCondition {
    BenchCondition::Always
}

/// Explicit benchmark process profiles.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BenchProfile {
    #[default]
    Normal,
    Quick,
}

impl BenchProfile {
    #[must_use]
    pub const fn warmup(self) -> u32 {
        match self {
            Self::Normal => DEFAULT_WARMUP,
            Self::Quick => QUICK_WARMUP,
        }
    }

    #[must_use]
    pub const fn runs(self) -> u32 {
        match self {
            Self::Normal => DEFAULT_RUNS,
            Self::Quick => QUICK_RUNS,
        }
    }
}

/// Broad category grouping for benchmark suites.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BenchCategory {
    Startup,
    Analysis,
    StubEncoder,
    Operational,
}

/// Runtime condition required for a benchmark to be runnable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BenchCondition {
    Always,
    StubEncoderScriptPresent,
    SeededDatabaseReady,
}

/// Current benchmark-runtime condition values.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct BenchConditionContext {
    pub stub_encoder_available: bool,
    pub seeded_database_available: bool,
}

impl BenchCondition {
    #[must_use]
    pub const fn evaluate(self, ctx: BenchConditionContext) -> bool {
        match self {
            Self::Always => true,
            Self::StubEncoderScriptPresent => ctx.stub_encoder_available,
            Self::SeededDatabaseReady => ctx.seeded_database_available,
        }
    }
}

/// Optional setup step for a benchmark before timing starts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchSetup {
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

/// Validation failures for benchmark data contracts.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum BenchValidationError {
    #[error("benchmark name must not be empty")]
    EmptyName,
    #[error("benchmark command must not be empty")]
    EmptyCommand,
    #[error("warmup must be greater than zero")]
    ZeroWarmup,
    #[error("runs must be greater than zero")]
    ZeroRuns,
    #[error("setup command must not be empty")]
    EmptySetupCommand,
    #[error("benchmark samples must not be empty")]
    EmptySamples,
}

/// Canonical benchmark configuration for one benchmark case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchConfig {
    pub name: String,
    pub command: Vec<String>,
    pub category: BenchCategory,
    #[serde(default = "default_warmup")]
    pub warmup: u32,
    #[serde(default = "default_runs")]
    pub runs: u32,
    #[serde(default)]
    pub requires_seeded_db: bool,
    #[serde(default)]
    pub conditional: bool,
    #[serde(default = "always_condition")]
    pub condition: BenchCondition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<BenchSetup>,
}

impl BenchConfig {
    /// Validate runtime-facing invariants.
    pub fn validate(&self) -> Result<(), BenchValidationError> {
        if self.name.trim().is_empty() {
            return Err(BenchValidationError::EmptyName);
        }
        if self.command.is_empty() {
            return Err(BenchValidationError::EmptyCommand);
        }
        if self.warmup == 0 {
            return Err(BenchValidationError::ZeroWarmup);
        }
        if self.runs == 0 {
            return Err(BenchValidationError::ZeroRuns);
        }
        if self
            .setup
            .as_ref()
            .is_some_and(|setup| setup.command.is_empty())
        {
            return Err(BenchValidationError::EmptySetupCommand);
        }
        Ok(())
    }

    #[must_use]
    pub fn with_profile(mut self, profile: BenchProfile) -> Self {
        self.warmup = profile.warmup();
        self.runs = profile.runs();
        self
    }

    #[must_use]
    pub fn enabled_for(&self, ctx: BenchConditionContext) -> bool {
        if !self.conditional {
            return true;
        }
        self.condition.evaluate(ctx)
    }
}

/// Catalog entry for built-in benchmark cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchmarkDef {
    pub name: &'static str,
    pub command: &'static [&'static str],
    pub category: BenchCategory,
    pub default_runs: u32,
    pub requires_seeded_db: bool,
    pub conditional: bool,
    pub condition: BenchCondition,
}

impl BenchmarkDef {
    #[must_use]
    pub fn to_config(self, profile: BenchProfile) -> BenchConfig {
        BenchConfig {
            name: self.name.to_string(),
            command: self.command.iter().map(ToString::to_string).collect(),
            category: self.category,
            warmup: profile.warmup(),
            runs: if matches!(profile, BenchProfile::Quick) {
                QUICK_RUNS
            } else {
                self.default_runs
            },
            requires_seeded_db: self.requires_seeded_db,
            conditional: self.conditional,
            condition: self.condition,
            setup: None,
        }
    }
}

const CMD_HELP: &[&str] = &["--help"];
const CMD_LINT: &[&str] = &["lint"];
const CMD_TYPECHECK: &[&str] = &["typecheck"];
const CMD_STUB_ENCODE_1K: &[&str] = &["stub-encode", "--size", "1024"];
const CMD_STUB_ENCODE_10K: &[&str] = &["stub-encode", "--size", "10240"];
const CMD_STUB_ENCODE_100K: &[&str] = &["stub-encode", "--size", "102400"];
const CMD_MAIL_INBOX: &[&str] = &[
    "mail",
    "inbox",
    "--project",
    "/tmp/bench",
    "--agent",
    "BlueLake",
    "--json",
];
const CMD_MAIL_SEND: &[&str] = &[
    "mail",
    "send",
    "--project",
    "/tmp/bench",
    "--from",
    "BlueLake",
    "--to",
    "RedFox",
    "--subject",
    "bench",
    "--body",
    "bench",
    "--json",
];
const CMD_MAIL_SEARCH: &[&str] = &[
    "mail",
    "search",
    "--project",
    "/tmp/bench",
    "--json",
    "bench",
];
const CMD_THREADS_LIST: &[&str] = &["mail", "threads", "--project", "/tmp/bench", "--json"];
const CMD_DOCTOR_CHECK: &[&str] = &["doctor", "check", "--json"];
const CMD_MESSAGE_COUNT: &[&str] = &["mail", "count", "--project", "/tmp/bench", "--json"];
const CMD_AGENTS_LIST: &[&str] = &["agents", "list", "--project", "/tmp/bench", "--json"];

/// Built-in benchmark catalog, aligned to the existing benchmark script.
pub const DEFAULT_BENCHMARKS: &[BenchmarkDef] = &[
    BenchmarkDef {
        name: "help",
        command: CMD_HELP,
        category: BenchCategory::Startup,
        default_runs: DEFAULT_RUNS,
        requires_seeded_db: false,
        conditional: false,
        condition: BenchCondition::Always,
    },
    BenchmarkDef {
        name: "lint",
        command: CMD_LINT,
        category: BenchCategory::Analysis,
        default_runs: 5,
        requires_seeded_db: false,
        conditional: false,
        condition: BenchCondition::Always,
    },
    BenchmarkDef {
        name: "typecheck",
        command: CMD_TYPECHECK,
        category: BenchCategory::Analysis,
        default_runs: 5,
        requires_seeded_db: false,
        conditional: false,
        condition: BenchCondition::Always,
    },
    BenchmarkDef {
        name: "stub_encode_1k",
        command: CMD_STUB_ENCODE_1K,
        category: BenchCategory::StubEncoder,
        default_runs: DEFAULT_RUNS,
        requires_seeded_db: false,
        conditional: true,
        condition: BenchCondition::StubEncoderScriptPresent,
    },
    BenchmarkDef {
        name: "stub_encode_10k",
        command: CMD_STUB_ENCODE_10K,
        category: BenchCategory::StubEncoder,
        default_runs: DEFAULT_RUNS,
        requires_seeded_db: false,
        conditional: true,
        condition: BenchCondition::StubEncoderScriptPresent,
    },
    BenchmarkDef {
        name: "stub_encode_100k",
        command: CMD_STUB_ENCODE_100K,
        category: BenchCategory::StubEncoder,
        default_runs: DEFAULT_RUNS,
        requires_seeded_db: false,
        conditional: true,
        condition: BenchCondition::StubEncoderScriptPresent,
    },
    BenchmarkDef {
        name: "mail_inbox",
        command: CMD_MAIL_INBOX,
        category: BenchCategory::Operational,
        default_runs: DEFAULT_RUNS,
        requires_seeded_db: true,
        conditional: true,
        condition: BenchCondition::SeededDatabaseReady,
    },
    BenchmarkDef {
        name: "mail_send",
        command: CMD_MAIL_SEND,
        category: BenchCategory::Operational,
        default_runs: DEFAULT_RUNS,
        requires_seeded_db: true,
        conditional: true,
        condition: BenchCondition::SeededDatabaseReady,
    },
    BenchmarkDef {
        name: "mail_search",
        command: CMD_MAIL_SEARCH,
        category: BenchCategory::Operational,
        default_runs: DEFAULT_RUNS,
        requires_seeded_db: true,
        conditional: true,
        condition: BenchCondition::SeededDatabaseReady,
    },
    BenchmarkDef {
        name: "mail_threads",
        command: CMD_THREADS_LIST,
        category: BenchCategory::Operational,
        default_runs: DEFAULT_RUNS,
        requires_seeded_db: true,
        conditional: true,
        condition: BenchCondition::SeededDatabaseReady,
    },
    BenchmarkDef {
        name: "doctor_check",
        command: CMD_DOCTOR_CHECK,
        category: BenchCategory::Operational,
        default_runs: DEFAULT_RUNS,
        requires_seeded_db: true,
        conditional: true,
        condition: BenchCondition::SeededDatabaseReady,
    },
    BenchmarkDef {
        name: "message_count",
        command: CMD_MESSAGE_COUNT,
        category: BenchCategory::Operational,
        default_runs: DEFAULT_RUNS,
        requires_seeded_db: true,
        conditional: true,
        condition: BenchCondition::SeededDatabaseReady,
    },
    BenchmarkDef {
        name: "agents_list",
        command: CMD_AGENTS_LIST,
        category: BenchCategory::Operational,
        default_runs: DEFAULT_RUNS,
        requires_seeded_db: true,
        conditional: true,
        condition: BenchCondition::SeededDatabaseReady,
    },
];

/// Baseline comparison metadata embedded in a benchmark result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct BaselineComparison {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_p95_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_p95_ms: Option<f64>,
    #[serde(default)]
    pub regression: bool,
}

/// Aggregated metrics for one benchmark case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchResult {
    pub name: String,
    pub mean_ms: f64,
    pub stddev_ms: f64,
    pub variance_ms2: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub median_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub timeseries_ms: Vec<f64>,
    pub command: String,
    pub fixture_signature: String,
    #[serde(flatten)]
    pub baseline: BaselineComparison,
}

fn round_to(value: f64, decimals: i32) -> f64 {
    let factor = 10_f64.powi(decimals);
    (value * factor).round() / factor
}

fn percentile(sorted_values: &[f64], p: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted_values.len() as f64 - 1.0)).round();
    let idx = idx.clamp(0.0, (sorted_values.len() - 1) as f64) as usize;
    sorted_values[idx]
}

impl BenchResult {
    /// Construct a benchmark result from duration samples measured in seconds.
    pub fn from_samples(
        name: impl Into<String>,
        command: impl Into<String>,
        samples_seconds: &[f64],
        fixture_signature: impl Into<String>,
        baseline_p95_ms: Option<f64>,
    ) -> Result<Self, BenchValidationError> {
        if samples_seconds.is_empty() {
            return Err(BenchValidationError::EmptySamples);
        }

        let mut samples_ms: Vec<f64> = samples_seconds
            .iter()
            .map(|s| round_to(s * 1000.0, 4))
            .collect();
        samples_ms.sort_by(|a, b| a.total_cmp(b));

        let mean_ms = samples_ms.iter().sum::<f64>() / samples_ms.len() as f64;
        let variance_ms2 = samples_ms
            .iter()
            .map(|sample| {
                let delta = *sample - mean_ms;
                delta * delta
            })
            .sum::<f64>()
            / samples_ms.len() as f64;
        let stddev_ms = variance_ms2.sqrt();

        let p95_ms = round_to(percentile(&samples_ms, 95.0), 2);
        let delta_p95_ms = baseline_p95_ms.map(|base| round_to(p95_ms - base, 2));

        Ok(Self {
            name: name.into(),
            mean_ms: round_to(mean_ms, 2),
            stddev_ms: round_to(stddev_ms, 2),
            variance_ms2: round_to(variance_ms2, 4),
            min_ms: round_to(*samples_ms.first().unwrap_or(&0.0), 2),
            max_ms: round_to(*samples_ms.last().unwrap_or(&0.0), 2),
            median_ms: round_to(percentile(&samples_ms, 50.0), 2),
            p95_ms,
            p99_ms: round_to(percentile(&samples_ms, 99.0), 2),
            timeseries_ms: samples_ms,
            command: command.into(),
            fixture_signature: fixture_signature.into(),
            baseline: BaselineComparison {
                baseline_p95_ms,
                delta_p95_ms,
                regression: delta_p95_ms.is_some_and(|delta| delta > 0.0),
            },
        })
    }
}

/// Benchmark host identity captured in summaries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HardwareInfo {
    pub hostname: String,
    pub arch: String,
    pub kernel: String,
}

impl HardwareInfo {
    #[must_use]
    pub fn detect() -> Self {
        let hostname = std::env::var("HOSTNAME")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "unknown-host".to_string());
        let arch = std::env::consts::ARCH.to_string();
        let kernel = std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|out| out.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| std::env::consts::OS.to_string());

        Self {
            hostname,
            arch,
            kernel,
        }
    }
}

/// Benchmark summary envelope written to JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchSummary {
    pub timestamp: String,
    pub schema_version: u32,
    pub hardware: HardwareInfo,
    pub benchmarks: BTreeMap<String, BenchResult>,
}

impl BenchSummary {
    #[must_use]
    pub fn new(hardware: HardwareInfo) -> Self {
        Self {
            timestamp: Utc::now().format("%Y%m%d_%H%M%S").to_string(),
            schema_version: BENCH_SCHEMA_VERSION,
            hardware,
            benchmarks: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, result: BenchResult) {
        self.benchmarks.insert(result.name.clone(), result);
    }
}

/// Explicit benchmark process exit codes for orchestration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[repr(i32)]
pub enum BenchExitCode {
    Success = 0,
    RuntimeError = 1,
    UsageError = 2,
    RegressionDetected = 3,
}

impl BenchExitCode {
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
}

/// Deterministic fixture signature used for baseline comparability.
#[must_use]
pub fn fixture_signature(
    benchmark_name: &str,
    command: &str,
    parameters_json: &str,
    hardware: &HardwareInfo,
) -> String {
    let material = format!(
        "{benchmark_name}|{command}|{parameters_json}|{}|{}",
        hardware.arch, hardware.kernel
    );
    let mut hasher = Sha256::new();
    hasher.update(material.as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hw() -> HardwareInfo {
        HardwareInfo {
            hostname: "h1".to_string(),
            arch: "x86_64".to_string(),
            kernel: "6.8.0".to_string(),
        }
    }

    #[test]
    fn bench_profile_sets_expected_runs() {
        assert_eq!(BenchProfile::Normal.warmup(), 3);
        assert_eq!(BenchProfile::Normal.runs(), 10);
        assert_eq!(BenchProfile::Quick.warmup(), 1);
        assert_eq!(BenchProfile::Quick.runs(), 3);
    }

    #[test]
    fn bench_config_validate_rejects_empty_command() {
        let cfg = BenchConfig {
            name: "x".to_string(),
            command: Vec::new(),
            category: BenchCategory::Startup,
            warmup: 1,
            runs: 1,
            requires_seeded_db: false,
            conditional: false,
            condition: BenchCondition::Always,
            setup: None,
        };
        assert_eq!(cfg.validate(), Err(BenchValidationError::EmptyCommand));
    }

    #[test]
    fn bench_config_validate_rejects_empty_setup_command() {
        let cfg = BenchConfig {
            name: "x".to_string(),
            command: vec!["--help".to_string()],
            category: BenchCategory::Startup,
            warmup: 1,
            runs: 1,
            requires_seeded_db: false,
            conditional: false,
            condition: BenchCondition::Always,
            setup: Some(BenchSetup {
                command: Vec::new(),
                env: BTreeMap::new(),
                working_dir: None,
            }),
        };
        assert_eq!(cfg.validate(), Err(BenchValidationError::EmptySetupCommand));
    }

    #[test]
    fn default_benchmark_catalog_has_expected_size() {
        assert_eq!(DEFAULT_BENCHMARKS.len(), 13);
    }

    #[test]
    fn fixture_signature_is_stable() {
        let hw = test_hw();
        let a = fixture_signature("help", "--help", "{}", &hw);
        let b = fixture_signature("help", "--help", "{}", &hw);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn bench_result_from_samples_computes_stats() {
        let result = BenchResult::from_samples(
            "help",
            "--help",
            &[0.001, 0.002, 0.004, 0.010],
            "sig",
            Some(7.0),
        )
        .expect("result");

        assert!((result.mean_ms - 4.25).abs() < 0.001);
        // Nearest-rank interpolation: idx = round(0.5 * 3) = 2 → sorted_values[2] = 4.0
        assert!((result.median_ms - 4.0).abs() < 0.001);
        assert!((result.p95_ms - 10.0).abs() < 0.001);
        assert!(result.baseline.regression);
        assert_eq!(result.baseline.delta_p95_ms, Some(3.0));
    }

    #[test]
    fn bench_result_from_samples_requires_samples() {
        let result = BenchResult::from_samples("help", "--help", &[], "sig", None);
        assert_eq!(result, Err(BenchValidationError::EmptySamples));
    }

    #[test]
    fn bench_summary_serializes_schema() {
        let mut summary = BenchSummary::new(test_hw());
        summary.insert(
            BenchResult::from_samples("help", "--help", &[0.001], "sig", None).expect("result"),
        );
        let json = serde_json::to_value(&summary).expect("json");
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["benchmarks"]["help"]["fixture_signature"], "sig");
    }

    #[test]
    fn conditional_benchmark_gating_works() {
        let cfg = BenchConfig {
            name: "mail_inbox".to_string(),
            command: vec!["mail".to_string(), "inbox".to_string()],
            category: BenchCategory::Operational,
            warmup: 1,
            runs: 1,
            requires_seeded_db: true,
            conditional: true,
            condition: BenchCondition::SeededDatabaseReady,
            setup: None,
        };

        assert!(!cfg.enabled_for(BenchConditionContext {
            stub_encoder_available: true,
            seeded_database_available: false,
        }));
        assert!(cfg.enabled_for(BenchConditionContext {
            stub_encoder_available: false,
            seeded_database_available: true,
        }));
    }
}
