//! Benchmark domain models for the `am bench` foundation.
//!
//! This module provides typed contracts for benchmark configuration, execution
//! results, summary aggregation, and deterministic fixture identity.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use chrono::Utc;
use mcp_agent_mail_db::sqlmodel::Value;
use mcp_agent_mail_db::DbConn;
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
/// Human key used by operational benchmark fixtures.
pub const BENCH_PROJECT_HUMAN_KEY: &str = "/tmp/bench";
/// Deterministic slug used for operational benchmark fixtures.
pub const BENCH_PROJECT_SLUG: &str = "tmp-bench";
/// Primary benchmark sender agent.
pub const BENCH_AGENT_BLUE: &str = "BlueLake";
/// Secondary benchmark sender/recipient agent.
pub const BENCH_AGENT_RED: &str = "RedFox";
/// Number of `BlueLake -> RedFox` seed messages.
pub const BENCH_SEED_FORWARD_MESSAGES: u32 = 50;
/// Number of `RedFox -> BlueLake` reply seed messages.
pub const BENCH_SEED_REPLY_MESSAGES: u32 = 10;
/// Total number of seed messages for operational benchmarks.
pub const BENCH_SEED_TOTAL_MESSAGES: u32 = BENCH_SEED_FORWARD_MESSAGES + BENCH_SEED_REPLY_MESSAGES;

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

/// Timing phase for one benchmark process invocation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimingPhase {
    Warmup,
    Measurement,
}

/// Failure details for one benchmark process invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimingFailure {
    pub phase: TimingPhase,
    pub iteration: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_us: i64,
}

/// Result envelope from timed benchmark execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimingResult {
    pub samples_seconds: Vec<f64>,
    pub warmup_runs: u32,
    pub measurement_runs: u32,
    pub warmup_failures: u32,
    pub measurement_failures: u32,
    pub failures: Vec<TimingFailure>,
    pub total_elapsed_us: i64,
}

impl TimingResult {
    #[must_use]
    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }
}

/// Timing harness failures.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum BenchTimingError {
    #[error("benchmark command must not be empty")]
    EmptyCommand,
    #[error("warmup must be greater than zero")]
    ZeroWarmup,
    #[error("runs must be greater than zero")]
    ZeroRuns,
    #[error("no successful measurement runs; attempted={attempted_runs}, failures={failure_count}")]
    NoSuccessfulMeasurementRuns {
        attempted_runs: u32,
        failure_count: u32,
    },
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
    BENCH_PROJECT_HUMAN_KEY,
    "--agent",
    BENCH_AGENT_BLUE,
    "--json",
];
const CMD_MAIL_SEND: &[&str] = &[
    "mail",
    "send",
    "--project",
    BENCH_PROJECT_HUMAN_KEY,
    "--from",
    BENCH_AGENT_BLUE,
    "--to",
    BENCH_AGENT_RED,
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
    BENCH_PROJECT_HUMAN_KEY,
    "--json",
    "bench",
];
const CMD_THREADS_LIST: &[&str] = &[
    "mail",
    "threads",
    "--project",
    BENCH_PROJECT_HUMAN_KEY,
    "--json",
];
const CMD_DOCTOR_CHECK: &[&str] = &["doctor", "check", "--json"];
const CMD_MESSAGE_COUNT: &[&str] = &[
    "mail",
    "count",
    "--project",
    BENCH_PROJECT_HUMAN_KEY,
    "--json",
];
const CMD_AGENTS_LIST: &[&str] = &[
    "agents",
    "list",
    "--project",
    BENCH_PROJECT_HUMAN_KEY,
    "--json",
];

/// Errors emitted by native benchmark fixture seeding.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum BenchSeedError {
    #[error("database error while {context}: {message}")]
    Database {
        context: &'static str,
        message: String,
    },
    #[error("expected row missing: {0}")]
    MissingRow(&'static str),
}

/// Structured diagnostics for benchmark fixture seeding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchSeedReport {
    pub project_id: i64,
    pub skipped: bool,
    pub reseeded: bool,
    pub existing_messages: u32,
    pub inserted_agents: u32,
    pub inserted_messages: u32,
    pub elapsed_us: i64,
}

fn db_error(context: &'static str, err: impl std::fmt::Display) -> BenchSeedError {
    BenchSeedError::Database {
        context,
        message: err.to_string(),
    }
}

fn select_project_id(conn: &DbConn) -> Result<Option<i64>, BenchSeedError> {
    let rows = conn
        .query_sync(
            "SELECT id FROM projects WHERE human_key = ? ORDER BY id ASC LIMIT 1",
            &[Value::Text(BENCH_PROJECT_HUMAN_KEY.to_string())],
        )
        .map_err(|e| db_error("selecting benchmark project", e))?;
    Ok(rows.first().and_then(|row| row.get_named("id").ok()))
}

fn select_agent_id(
    conn: &DbConn,
    project_id: i64,
    agent_name: &str,
) -> Result<Option<i64>, BenchSeedError> {
    let rows = conn
        .query_sync(
            "SELECT id FROM agents WHERE project_id = ? AND name = ? LIMIT 1",
            &[
                Value::BigInt(project_id),
                Value::Text(agent_name.to_string()),
            ],
        )
        .map_err(|e| db_error("selecting benchmark agent", e))?;
    Ok(rows.first().and_then(|row| row.get_named("id").ok()))
}

fn count_project_messages(conn: &DbConn, project_id: i64) -> Result<i64, BenchSeedError> {
    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS count FROM messages WHERE project_id = ?",
            &[Value::BigInt(project_id)],
        )
        .map_err(|e| db_error("counting benchmark messages", e))?;
    Ok(rows
        .first()
        .and_then(|row| row.get_named("count").ok())
        .unwrap_or(0))
}

fn ensure_project(conn: &DbConn, now_us: i64) -> Result<i64, BenchSeedError> {
    if let Some(project_id) = select_project_id(conn)? {
        return Ok(project_id);
    }

    conn.execute_sync(
        "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
        &[
            Value::Text(BENCH_PROJECT_SLUG.to_string()),
            Value::Text(BENCH_PROJECT_HUMAN_KEY.to_string()),
            Value::BigInt(now_us),
        ],
    )
    .map_err(|e| db_error("creating benchmark project", e))?;
    select_project_id(conn)?.ok_or(BenchSeedError::MissingRow("project"))
}

fn purge_project_rows(conn: &DbConn, project_id: i64) -> Result<(), BenchSeedError> {
    conn.execute_sync(
        "DELETE FROM message_recipients \
         WHERE message_id IN (SELECT id FROM messages WHERE project_id = ?)",
        &[Value::BigInt(project_id)],
    )
    .map_err(|e| db_error("deleting benchmark message recipients", e))?;
    conn.execute_sync(
        "DELETE FROM messages WHERE project_id = ?",
        &[Value::BigInt(project_id)],
    )
    .map_err(|e| db_error("deleting benchmark messages", e))?;
    conn.execute_sync(
        "DELETE FROM file_reservations WHERE project_id = ?",
        &[Value::BigInt(project_id)],
    )
    .map_err(|e| db_error("deleting benchmark file reservations", e))?;
    conn.execute_sync(
        "DELETE FROM agent_links \
         WHERE a_project_id = ? OR b_project_id = ? \
         OR a_agent_id IN (SELECT id FROM agents WHERE project_id = ?) \
         OR b_agent_id IN (SELECT id FROM agents WHERE project_id = ?)",
        &[
            Value::BigInt(project_id),
            Value::BigInt(project_id),
            Value::BigInt(project_id),
            Value::BigInt(project_id),
        ],
    )
    .map_err(|e| db_error("deleting benchmark agent links", e))?;
    conn.execute_sync(
        "DELETE FROM product_project_links WHERE project_id = ?",
        &[Value::BigInt(project_id)],
    )
    .map_err(|e| db_error("deleting benchmark product links", e))?;
    conn.execute_sync(
        "DELETE FROM project_sibling_suggestions WHERE project_a_id = ? OR project_b_id = ?",
        &[Value::BigInt(project_id), Value::BigInt(project_id)],
    )
    .map_err(|e| db_error("deleting benchmark sibling suggestions", e))?;
    conn.execute_sync(
        "DELETE FROM agents WHERE project_id = ?",
        &[Value::BigInt(project_id)],
    )
    .map_err(|e| db_error("deleting benchmark agents", e))?;
    Ok(())
}

fn insert_agent(
    conn: &DbConn,
    project_id: i64,
    agent_name: &str,
    now_us: i64,
) -> Result<i64, BenchSeedError> {
    conn.execute_sync(
        "INSERT OR IGNORE INTO agents (\
            project_id, name, program, model, task_description, inception_ts, \
            last_active_ts, attachments_policy, contact_policy\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(project_id),
            Value::Text(agent_name.to_string()),
            Value::Text("bench".to_string()),
            Value::Text("bench".to_string()),
            Value::Text("benchmark fixture".to_string()),
            Value::BigInt(now_us),
            Value::BigInt(now_us),
            Value::Text("auto".to_string()),
            Value::Text("auto".to_string()),
        ],
    )
    .map_err(|e| db_error("inserting benchmark agent", e))?;
    select_agent_id(conn, project_id, agent_name)?
        .ok_or(BenchSeedError::MissingRow("agent after insert"))
}

fn insert_message(
    conn: &DbConn,
    project_id: i64,
    sender_id: i64,
    recipient_id: i64,
    subject: &str,
    body_md: &str,
    created_ts: i64,
) -> Result<(), BenchSeedError> {
    conn.execute_sync(
        "INSERT INTO messages (\
            project_id, sender_id, thread_id, subject, body_md, importance, \
            ack_required, created_ts, attachments\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(project_id),
            Value::BigInt(sender_id),
            Value::Null,
            Value::Text(subject.to_string()),
            Value::Text(body_md.to_string()),
            Value::Text("normal".to_string()),
            Value::BigInt(0),
            Value::BigInt(created_ts),
            Value::Text("[]".to_string()),
        ],
    )
    .map_err(|e| db_error("inserting benchmark message", e))?;

    // FrankenConnection does not support last_insert_rowid(); use MAX(id).
    let message_id: i64 = conn
        .query_sync("SELECT MAX(id) AS id FROM messages", &[])
        .map_err(|e| db_error("reading inserted benchmark message id", e))?
        .first()
        .and_then(|row| row.get_named("id").ok())
        .ok_or(BenchSeedError::MissingRow(
            "message id after benchmark insert",
        ))?;

    conn.execute_sync(
        "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)",
        &[
            Value::BigInt(message_id),
            Value::BigInt(recipient_id),
            Value::Text("to".to_string()),
        ],
    )
    .map_err(|e| db_error("inserting benchmark message recipient", e))?;

    Ok(())
}

/// Seed the benchmark fixture dataset directly through native SQLite writes.
///
/// This removes the benchmark setup bottleneck caused by repeatedly spawning
/// CLI subprocesses for project/agent/message creation.
pub fn seed_bench_database(
    conn: &DbConn,
    reseed: bool,
) -> Result<BenchSeedReport, BenchSeedError> {
    let started = Instant::now();
    conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql())
        .map_err(|e| db_error("initializing schema for benchmark seed", e))?;
    conn.execute_raw("BEGIN IMMEDIATE")
        .map_err(|e| db_error("starting benchmark seed transaction", e))?;

    let seed_result = (|| {
        let now_us = mcp_agent_mail_db::timestamps::now_micros();
        let project_id = ensure_project(conn, now_us)?;
        let existing_messages = count_project_messages(conn, project_id)?;
        let existing_messages_u32 = u32::try_from(existing_messages).unwrap_or(u32::MAX);

        if existing_messages > 0 && !reseed {
            return Ok(BenchSeedReport {
                project_id,
                skipped: true,
                reseeded: false,
                existing_messages: existing_messages_u32,
                inserted_agents: 0,
                inserted_messages: 0,
                elapsed_us: 0,
            });
        }

        purge_project_rows(conn, project_id)?;
        let blue_id = insert_agent(conn, project_id, BENCH_AGENT_BLUE, now_us)?;
        let red_id = insert_agent(conn, project_id, BENCH_AGENT_RED, now_us)?;

        let mut inserted_messages = 0_u32;

        insert_message(
            conn,
            project_id,
            blue_id,
            red_id,
            "seed-0",
            "initial seed",
            now_us,
        )?;
        inserted_messages += 1;

        for idx in 1..BENCH_SEED_FORWARD_MESSAGES {
            let offset = i64::from(idx);
            insert_message(
                conn,
                project_id,
                blue_id,
                red_id,
                &format!("bench message {idx}"),
                &format!("body of message {idx} for benchmarking"),
                now_us + offset,
            )?;
            inserted_messages += 1;
        }

        for reply in 1..=BENCH_SEED_REPLY_MESSAGES {
            let offset = i64::from(BENCH_SEED_FORWARD_MESSAGES + reply);
            insert_message(
                conn,
                project_id,
                red_id,
                blue_id,
                &format!("reply {reply}"),
                &format!("reply body {reply}"),
                now_us + offset,
            )?;
            inserted_messages += 1;
        }

        Ok(BenchSeedReport {
            project_id,
            skipped: false,
            reseeded: reseed,
            existing_messages: existing_messages_u32,
            inserted_agents: 2,
            inserted_messages,
            elapsed_us: 0,
        })
    })();

    match seed_result {
        Ok(mut report) => {
            conn.execute_raw("COMMIT")
                .map_err(|e| db_error("committing benchmark seed transaction", e))?;
            report.elapsed_us = i64::try_from(started.elapsed().as_micros()).unwrap_or(i64::MAX);
            Ok(report)
        }
        Err(err) => {
            let _ = conn.execute_raw("ROLLBACK");
            Err(err)
        }
    }
}

fn elapsed_us(elapsed: std::time::Duration) -> i64 {
    i64::try_from(elapsed.as_micros()).unwrap_or(i64::MAX)
}

fn run_once(
    command: &[String],
    env: &BTreeMap<String, String>,
    working_dir: Option<&Path>,
) -> (Option<i32>, Option<String>, i64, bool) {
    let started = Instant::now();
    let mut process = Command::new(&command[0]);
    process.args(&command[1..]);
    process.stdout(Stdio::null());
    process.stderr(Stdio::null());
    if let Some(cwd) = working_dir {
        process.current_dir(cwd);
    }
    process.envs(env);

    match process.status() {
        Ok(status) => (
            status.code(),
            None,
            elapsed_us(started.elapsed()),
            status.success(),
        ),
        Err(err) => (
            None,
            Some(err.to_string()),
            elapsed_us(started.elapsed()),
            false,
        ),
    }
}

/// Run a benchmark command with warmup+measurement loops and per-invocation timing.
///
/// `samples_seconds` contains successful measurement runs only.
/// Failed invocations are captured in `failures`.
pub fn run_timed(
    command: &[String],
    warmup: u32,
    runs: u32,
    env: &BTreeMap<String, String>,
    working_dir: Option<&Path>,
) -> Result<TimingResult, BenchTimingError> {
    if command.is_empty() {
        return Err(BenchTimingError::EmptyCommand);
    }
    if warmup == 0 {
        return Err(BenchTimingError::ZeroWarmup);
    }
    if runs == 0 {
        return Err(BenchTimingError::ZeroRuns);
    }

    let started = Instant::now();
    let mut samples_seconds: Vec<f64> = Vec::new();
    let mut failures: Vec<TimingFailure> = Vec::new();
    let mut warmup_failures = 0_u32;
    let mut measurement_failures = 0_u32;

    for iteration in 1..=warmup {
        let (exit_code, error, elapsed_us, success) = run_once(command, env, working_dir);
        if !success {
            warmup_failures += 1;
            failures.push(TimingFailure {
                phase: TimingPhase::Warmup,
                iteration,
                exit_code,
                error,
                elapsed_us,
            });
        }
    }

    for iteration in 1..=runs {
        let (exit_code, error, elapsed_us, success) = run_once(command, env, working_dir);
        if success {
            samples_seconds.push(elapsed_us as f64 / 1_000_000.0);
            continue;
        }

        measurement_failures += 1;
        failures.push(TimingFailure {
            phase: TimingPhase::Measurement,
            iteration,
            exit_code,
            error,
            elapsed_us,
        });
    }

    if samples_seconds.is_empty() {
        return Err(BenchTimingError::NoSuccessfulMeasurementRuns {
            attempted_runs: runs,
            failure_count: measurement_failures,
        });
    }

    Ok(TimingResult {
        samples_seconds,
        warmup_runs: warmup,
        measurement_runs: runs,
        warmup_failures,
        measurement_failures,
        failures,
        total_elapsed_us: elapsed_us(started.elapsed()),
    })
}

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

/// Persisted baseline data: benchmark name -> baseline p95 in milliseconds.
pub type BaselineData = BTreeMap<String, f64>;

/// Regression comparison details against a baseline snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BaselineComparisonResult {
    pub name: String,
    pub current_p95_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_p95_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_p95_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_pct: Option<f64>,
    pub regression: bool,
}

/// Baseline load/save/comparison failures.
#[derive(Debug, thiserror::Error)]
pub enum BenchBaselineError {
    #[error("failed to create baseline directory at '{path}': {source}")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write baseline file at '{path}': {source}")]
    WriteFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read baseline file at '{path}': {source}")]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse baseline JSON at '{path}': {source}")]
    ParseJson {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("baseline root must be a JSON object at '{path}'")]
    InvalidRoot { path: String },
    #[error("baseline entry for '{benchmark}' must be a number or object with numeric p95_ms")]
    InvalidEntry { benchmark: String },
}

fn path_display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn baseline_regression(delta_p95_ms: f64, baseline_p95_ms: f64, threshold_pct: f64) -> bool {
    if delta_p95_ms <= 0.0 {
        return false;
    }
    if baseline_p95_ms <= f64::EPSILON {
        return true;
    }
    (delta_p95_ms / baseline_p95_ms) > threshold_pct.max(0.0)
}

/// Save a baseline snapshot from current benchmark results.
pub fn save_baseline(
    results: &BTreeMap<String, BenchResult>,
    path: &Path,
) -> Result<(), BenchBaselineError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|source| BenchBaselineError::CreateDir {
            path: path_display(parent),
            source,
        })?;
    }

    let baseline: BaselineData = results
        .iter()
        .map(|(name, result)| (name.clone(), round_to(result.p95_ms, 2)))
        .collect();

    let encoded = serde_json::to_string_pretty(&baseline).map_err(|source| {
        BenchBaselineError::ParseJson {
            path: path_display(path),
            source,
        }
    })?;

    fs::write(path, encoded).map_err(|source| BenchBaselineError::WriteFile {
        path: path_display(path),
        source,
    })?;
    Ok(())
}

/// Load baseline data from disk.
///
/// Supported formats:
/// - `{ "bench": 12.34 }`
/// - `{ "bench": { "p95_ms": 12.34 } }`
pub fn load_baseline(path: &Path) -> Result<BaselineData, BenchBaselineError> {
    let raw = fs::read_to_string(path).map_err(|source| BenchBaselineError::ReadFile {
        path: path_display(path),
        source,
    })?;
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).map_err(|source| BenchBaselineError::ParseJson {
            path: path_display(path),
            source,
        })?;

    let Some(entries) = parsed.as_object() else {
        return Err(BenchBaselineError::InvalidRoot {
            path: path_display(path),
        });
    };

    let mut baseline = BaselineData::new();
    for (benchmark, value) in entries {
        let p95_ms = value
            .as_f64()
            .or_else(|| value.get("p95_ms").and_then(serde_json::Value::as_f64))
            .ok_or_else(|| BenchBaselineError::InvalidEntry {
                benchmark: benchmark.clone(),
            })?;
        baseline.insert(benchmark.clone(), round_to(p95_ms, 2));
    }

    Ok(baseline)
}

/// Compare current results with a loaded baseline and flag regressions.
///
/// `threshold_pct` is expressed as a ratio (0.10 = 10%).
#[must_use]
pub fn compare_baseline(
    results: &BTreeMap<String, BenchResult>,
    baseline: &BaselineData,
    threshold_pct: f64,
) -> Vec<BaselineComparisonResult> {
    let mut comparisons = Vec::with_capacity(results.len());

    for (name, result) in results {
        let baseline_p95_ms = baseline.get(name).copied();
        let delta_p95_ms = baseline_p95_ms.map(|base| round_to(result.p95_ms - base, 2));
        let delta_pct = baseline_p95_ms.and_then(|base| {
            if base <= f64::EPSILON {
                return None;
            }
            delta_p95_ms.map(|delta| round_to((delta / base) * 100.0, 2))
        });
        let regression = baseline_p95_ms.is_some_and(|base| {
            delta_p95_ms
                .map(|delta| baseline_regression(delta, base, threshold_pct))
                .unwrap_or(false)
        });

        comparisons.push(BaselineComparisonResult {
            name: name.clone(),
            current_p95_ms: result.p95_ms,
            baseline_p95_ms,
            delta_p95_ms,
            delta_pct,
            regression,
        });
    }

    comparisons
}

/// Merge baseline comparison metadata into benchmark results for output emission.
pub fn apply_baseline_comparison(
    results: &mut BTreeMap<String, BenchResult>,
    baseline: &BaselineData,
    threshold_pct: f64,
) {
    for comparison in compare_baseline(results, baseline, threshold_pct) {
        if let Some(result) = results.get_mut(&comparison.name) {
            result.baseline.baseline_p95_ms = comparison.baseline_p95_ms;
            result.baseline.delta_p95_ms = comparison.delta_p95_ms;
            result.baseline.regression = comparison.regression;
        }
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

    fn count_with_params(conn: &DbConn, sql: &str, params: &[Value]) -> i64 {
        conn.query_sync(sql, params)
            .expect("query")
            .first()
            .and_then(|row| row.get_named("count").ok())
            .unwrap_or(0)
    }

    fn test_hw() -> HardwareInfo {
        HardwareInfo {
            hostname: "h1".to_string(),
            arch: "x86_64".to_string(),
            kernel: "6.8.0".to_string(),
        }
    }

    fn test_command_with_args(args: &[&str]) -> Vec<String> {
        let mut command = vec![
            std::env::current_exe()
                .expect("current exe path")
                .to_string_lossy()
                .into_owned(),
        ];
        command.extend(args.iter().map(ToString::to_string));
        command
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
        assert_eq!(a, "49d26a42643b7ee5");
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
    fn bench_result_from_samples_known_linear_sequence_statistics() {
        let result = BenchResult::from_samples(
            "linear",
            "linear",
            &[0.001, 0.002, 0.003, 0.004, 0.005],
            "sig-linear",
            None,
        )
        .expect("result");

        assert_eq!(result.mean_ms, 3.0);
        assert_eq!(result.median_ms, 3.0);
        assert_eq!(result.p95_ms, 5.0);
        assert_eq!(result.p99_ms, 5.0);
        assert_eq!(result.variance_ms2, 2.0);
        assert_eq!(result.stddev_ms, 1.41);
    }

    #[test]
    fn bench_result_from_single_sample_has_zero_spread() {
        let result = BenchResult::from_samples("single", "single", &[0.012], "sig-single", None)
            .expect("result");

        assert_eq!(result.mean_ms, 12.0);
        assert_eq!(result.median_ms, 12.0);
        assert_eq!(result.min_ms, 12.0);
        assert_eq!(result.max_ms, 12.0);
        assert_eq!(result.p95_ms, 12.0);
        assert_eq!(result.p99_ms, 12.0);
        assert_eq!(result.stddev_ms, 0.0);
        assert_eq!(result.variance_ms2, 0.0);
    }

    #[test]
    fn bench_result_from_identical_values_has_zero_variance() {
        let result = BenchResult::from_samples(
            "identical",
            "identical",
            &[0.007, 0.007, 0.007, 0.007],
            "sig-identical",
            None,
        )
        .expect("result");

        assert_eq!(result.mean_ms, 7.0);
        assert_eq!(result.stddev_ms, 0.0);
        assert_eq!(result.variance_ms2, 0.0);
        assert_eq!(result.p95_ms, 7.0);
        assert_eq!(result.p99_ms, 7.0);
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
    fn save_and_load_baseline_round_trip() {
        let mut results = BTreeMap::new();
        results.insert(
            "help".to_string(),
            BenchResult::from_samples("help", "--help", &[0.001, 0.002], "sig-a", None)
                .expect("result"),
        );
        results.insert(
            "lint".to_string(),
            BenchResult::from_samples("lint", "lint", &[0.050, 0.060], "sig-b", None)
                .expect("result"),
        );

        let temp = tempfile::tempdir().expect("tempdir");
        let baseline_path = temp.path().join("baseline.json");
        save_baseline(&results, &baseline_path).expect("save baseline");

        let loaded = load_baseline(&baseline_path).expect("load baseline");
        assert_eq!(loaded.get("help").copied(), Some(2.0));
        assert_eq!(loaded.get("lint").copied(), Some(60.0));
    }

    #[test]
    fn load_baseline_supports_legacy_nested_format() {
        let temp = tempfile::tempdir().expect("tempdir");
        let baseline_path = temp.path().join("legacy.json");
        fs::write(
            &baseline_path,
            r#"{"help":{"p95_ms":12.34},"lint":{"p95_ms":99.0}}"#,
        )
        .expect("write baseline");

        let loaded = load_baseline(&baseline_path).expect("load baseline");
        assert_eq!(loaded.get("help").copied(), Some(12.34));
        assert_eq!(loaded.get("lint").copied(), Some(99.0));
    }

    #[test]
    fn load_baseline_rejects_invalid_entry_shape() {
        let temp = tempfile::tempdir().expect("tempdir");
        let baseline_path = temp.path().join("invalid.json");
        fs::write(&baseline_path, r#"{"help":{"unexpected":1}}"#).expect("write baseline");

        let err = load_baseline(&baseline_path).expect_err("invalid baseline shape");
        assert!(matches!(
            err,
            BenchBaselineError::InvalidEntry { benchmark } if benchmark == "help"
        ));
    }

    #[test]
    fn compare_baseline_marks_regression_with_threshold() {
        let mut results = BTreeMap::new();
        results.insert(
            "help".to_string(),
            BenchResult::from_samples("help", "--help", &[0.011, 0.012], "sig-a", None)
                .expect("result"),
        );
        results.insert(
            "lint".to_string(),
            BenchResult::from_samples("lint", "lint", &[0.009, 0.010], "sig-b", None)
                .expect("result"),
        );

        let mut baseline = BaselineData::new();
        baseline.insert("help".to_string(), 10.0);
        baseline.insert("lint".to_string(), 20.0);

        let comparisons = compare_baseline(&results, &baseline, 0.10);
        assert_eq!(comparisons.len(), 2);

        let help = comparisons
            .iter()
            .find(|entry| entry.name == "help")
            .expect("help comparison");
        assert_eq!(help.delta_p95_ms, Some(2.0));
        assert_eq!(help.delta_pct, Some(20.0));
        assert!(help.regression);

        let lint = comparisons
            .iter()
            .find(|entry| entry.name == "lint")
            .expect("lint comparison");
        assert_eq!(lint.delta_p95_ms, Some(-10.0));
        assert_eq!(lint.delta_pct, Some(-50.0));
        assert!(!lint.regression);
    }

    #[test]
    fn compare_baseline_respects_threshold_boundaries() {
        let mut results = BTreeMap::new();
        results.insert(
            "under".to_string(),
            BenchResult::from_samples("under", "under", &[0.0109, 0.0109], "sig-under", None)
                .expect("result"),
        );
        results.insert(
            "over".to_string(),
            BenchResult::from_samples("over", "over", &[0.0111, 0.0111], "sig-over", None)
                .expect("result"),
        );

        let mut baseline = BaselineData::new();
        baseline.insert("under".to_string(), 10.0);
        baseline.insert("over".to_string(), 10.0);

        let comparisons = compare_baseline(&results, &baseline, 0.10);
        let under = comparisons
            .iter()
            .find(|entry| entry.name == "under")
            .expect("under comparison");
        assert_eq!(under.delta_p95_ms, Some(0.9));
        assert_eq!(under.delta_pct, Some(9.0));
        assert!(!under.regression, "9% delta must stay below 10% threshold");

        let over = comparisons
            .iter()
            .find(|entry| entry.name == "over")
            .expect("over comparison");
        assert_eq!(over.delta_p95_ms, Some(1.1));
        assert_eq!(over.delta_pct, Some(11.0));
        assert!(over.regression, "11% delta must exceed 10% threshold");
    }

    #[test]
    fn apply_baseline_comparison_updates_result_metadata() {
        let mut results = BTreeMap::new();
        results.insert(
            "help".to_string(),
            BenchResult::from_samples("help", "--help", &[0.011, 0.012], "sig-a", None)
                .expect("result"),
        );
        let mut baseline = BaselineData::new();
        baseline.insert("help".to_string(), 10.0);

        apply_baseline_comparison(&mut results, &baseline, 0.10);
        let result = results.get("help").expect("updated result");
        assert_eq!(result.baseline.baseline_p95_ms, Some(10.0));
        assert_eq!(result.baseline.delta_p95_ms, Some(2.0));
        assert!(result.baseline.regression);
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

    #[test]
    fn run_timed_collects_measurement_samples_and_ignores_warmup() {
        let command = test_command_with_args(&["--help"]);
        let result = run_timed(&command, 1, 2, &BTreeMap::new(), None).expect("run_timed");

        assert_eq!(result.warmup_runs, 1);
        assert_eq!(result.measurement_runs, 2);
        assert_eq!(result.samples_seconds.len(), 2);
        assert_eq!(result.warmup_failures, 0);
        assert_eq!(result.measurement_failures, 0);
        assert!(!result.has_failures());
        assert!(result.total_elapsed_us > 0);
    }

    #[test]
    fn run_timed_validates_basic_contracts() {
        let env = BTreeMap::new();
        assert_eq!(
            run_timed(&[], 1, 1, &env, None),
            Err(BenchTimingError::EmptyCommand)
        );
        assert_eq!(
            run_timed(&["true".to_string()], 0, 1, &env, None),
            Err(BenchTimingError::ZeroWarmup)
        );
        assert_eq!(
            run_timed(&["true".to_string()], 1, 0, &env, None),
            Err(BenchTimingError::ZeroRuns)
        );
    }

    #[test]
    fn run_timed_continues_on_failures_and_reports_when_no_successes() {
        let command = vec!["definitely-not-a-real-command-azure-pine".to_string()];
        let err = run_timed(&command, 2, 3, &BTreeMap::new(), None)
            .expect_err("expected no successful measurement runs");

        assert_eq!(
            err,
            BenchTimingError::NoSuccessfulMeasurementRuns {
                attempted_runs: 3,
                failure_count: 3,
            }
        );
    }

    #[test]
    fn seed_bench_database_populates_expected_fixture() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("bench-seed.sqlite3");
        let conn =
            DbConn::open_file(db_path.display().to_string()).expect("open sqlite db");

        let report = seed_bench_database(&conn, false).expect("seed benchmark db");
        assert!(!report.skipped);
        assert_eq!(report.inserted_agents, 2);
        assert_eq!(report.inserted_messages, BENCH_SEED_TOTAL_MESSAGES);
        assert!(report.elapsed_us >= 0);

        // FrankenConnection: JOIN + COUNT(*) returns NULL; use IN subqueries.
        let blue_messages = count_with_params(
            &conn,
            "SELECT COUNT(*) AS count \
             FROM messages \
             WHERE project_id = ? AND sender_id IN (SELECT id FROM agents WHERE name = ?)",
            &[
                Value::BigInt(report.project_id),
                Value::Text(BENCH_AGENT_BLUE.to_string()),
            ],
        );
        let red_messages = count_with_params(
            &conn,
            "SELECT COUNT(*) AS count \
             FROM messages \
             WHERE project_id = ? AND sender_id IN (SELECT id FROM agents WHERE name = ?)",
            &[
                Value::BigInt(report.project_id),
                Value::Text(BENCH_AGENT_RED.to_string()),
            ],
        );
        let recipient_rows = count_with_params(
            &conn,
            "SELECT COUNT(*) AS count \
             FROM message_recipients \
             WHERE message_id IN (SELECT id FROM messages WHERE project_id = ?)",
            &[Value::BigInt(report.project_id)],
        );

        assert_eq!(blue_messages, i64::from(BENCH_SEED_FORWARD_MESSAGES));
        assert_eq!(red_messages, i64::from(BENCH_SEED_REPLY_MESSAGES));
        assert_eq!(recipient_rows, i64::from(BENCH_SEED_TOTAL_MESSAGES));
    }

    #[test]
    fn seed_bench_database_is_idempotent_without_reseed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("bench-idempotent.sqlite3");
        let conn =
            DbConn::open_file(db_path.display().to_string()).expect("open sqlite db");

        let first = seed_bench_database(&conn, false).expect("first seed");
        assert!(!first.skipped);

        let second = seed_bench_database(&conn, false).expect("second seed");
        assert!(second.skipped);
        assert_eq!(second.inserted_agents, 0);
        assert_eq!(second.inserted_messages, 0);
        assert_eq!(second.existing_messages, BENCH_SEED_TOTAL_MESSAGES);

        let total_messages = count_with_params(
            &conn,
            "SELECT COUNT(*) AS count FROM messages WHERE project_id = ?",
            &[Value::BigInt(first.project_id)],
        );
        assert_eq!(total_messages, i64::from(BENCH_SEED_TOTAL_MESSAGES));
    }

    #[test]
    fn seed_bench_database_reseed_rebuilds_fixture() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("bench-reseed.sqlite3");
        let conn =
            DbConn::open_file(db_path.display().to_string()).expect("open sqlite db");

        let initial = seed_bench_database(&conn, false).expect("initial seed");
        let blue_id = select_agent_id(&conn, initial.project_id, BENCH_AGENT_BLUE)
            .expect("select blue")
            .expect("blue exists");
        let red_id = select_agent_id(&conn, initial.project_id, BENCH_AGENT_RED)
            .expect("select red")
            .expect("red exists");

        insert_message(
            &conn,
            initial.project_id,
            blue_id,
            red_id,
            "extra message",
            "extra body",
            mcp_agent_mail_db::timestamps::now_micros(),
        )
        .expect("insert extra message");

        let pre_reseed_total = count_with_params(
            &conn,
            "SELECT COUNT(*) AS count FROM messages WHERE project_id = ?",
            &[Value::BigInt(initial.project_id)],
        );
        assert_eq!(pre_reseed_total, i64::from(BENCH_SEED_TOTAL_MESSAGES) + 1);

        let reseeded = seed_bench_database(&conn, true).expect("reseed");
        assert!(!reseeded.skipped);
        assert!(reseeded.reseeded);
        assert_eq!(reseeded.inserted_messages, BENCH_SEED_TOTAL_MESSAGES);

        let final_total = count_with_params(
            &conn,
            "SELECT COUNT(*) AS count FROM messages WHERE project_id = ?",
            &[Value::BigInt(initial.project_id)],
        );
        assert_eq!(final_total, i64::from(BENCH_SEED_TOTAL_MESSAGES));
    }

    #[test]
    fn seed_bench_database_reuses_existing_human_key_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("bench-existing-project.sqlite3");
        let conn =
            DbConn::open_file(db_path.display().to_string()).expect("open sqlite db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql())
            .expect("init schema");

        let now_us = mcp_agent_mail_db::timestamps::now_micros();
        conn.execute_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
            &[
                Value::Text("legacy-bench-slug".to_string()),
                Value::Text(BENCH_PROJECT_HUMAN_KEY.to_string()),
                Value::BigInt(now_us),
            ],
        )
        .expect("insert existing benchmark project");

        let report = seed_bench_database(&conn, false).expect("seed benchmark db");
        let duplicate_projects = count_with_params(
            &conn,
            "SELECT COUNT(*) AS count FROM projects WHERE human_key = ?",
            &[Value::Text(BENCH_PROJECT_HUMAN_KEY.to_string())],
        );

        assert_eq!(duplicate_projects, 1);
        assert!(!report.skipped);
        assert_eq!(report.inserted_messages, BENCH_SEED_TOTAL_MESSAGES);
    }
}
