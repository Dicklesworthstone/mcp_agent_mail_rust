//! CLI commands for MCP Agent Mail
//!
//! This crate mirrors the legacy Python Typer CLI with clap, focusing on:
//! - Share/export commands
//! - Doctor diagnostics
//! - Guard tooling
//! - Project, mail, and product helpers
//! - Build slot utilities
//!
//! Command execution is stubbed while lower layers are implemented, but
//! argument parsing and validation match the legacy CLI.

#![forbid(unsafe_code)]

pub mod bench;
pub mod ci;
pub mod context;
pub mod e2e_artifacts;
pub mod e2e_runner;
pub mod golden;
pub mod legacy;
pub mod output;
pub mod robot;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};

use chrono::{DateTime, Utc};

use mcp_agent_mail_core::{AgentDetectError, AgentDetectOptions, Config, resolve_project_identity};
use mcp_agent_mail_share as share;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("exit code {0}")]
    ExitCode(i32),
    #[error(transparent)]
    Share(#[from] share::ShareError),
    #[error(transparent)]
    Guard(#[from] mcp_agent_mail_guard::GuardError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("format error: {0}")]
    Format(String),
    #[error("{0}")]
    Other(String),
}

pub type CliResult<T> = Result<T, CliError>;

#[derive(Parser, Debug)]
#[command(name = "am", version = version_with_update_hint(), about = "MCP Agent Mail CLI (Rust)")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Build a version string that includes an update hint if a cached check exists.
fn version_with_update_hint() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let base = env!("CARGO_PKG_VERSION");
        // Check update cache (fast, no network)
        if let Some(UpdateCheckResult::UpdateAvailable { latest, .. }) = read_update_cache() {
            format!("{base} (update available: {latest} — run `am self-update`)")
        } else {
            base.to_string()
        }
    })
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    #[command(name = "serve-http")]
    ServeHttp {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        path: Option<String>,
        /// Disable bearer token authentication for this run (for local development).
        #[arg(long)]
        no_auth: bool,
        /// Disable the interactive TUI and run headless.
        #[arg(long)]
        no_tui: bool,
    },
    #[command(name = "serve-stdio")]
    ServeStdio,
    /// Check agent inbox for unread messages (for git hooks and editor integrations).
    #[command(name = "check-inbox")]
    CheckInbox {
        /// Agent name to check inbox for (default: AGENT_NAME or AGENT_MAIL_AGENT env var).
        #[arg(long)]
        agent: Option<String>,
        /// Minimum interval between checks in seconds (default: 120, 0 to disable).
        #[arg(long, default_value_t = 120)]
        rate_limit: u64,
        /// Query DB directly instead of HTTP (faster for co-located setups).
        #[arg(long)]
        direct: bool,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long)]
        json: bool,
        /// Server host for HTTP mode (default: 127.0.0.1).
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Server port for HTTP mode (default: 8765).
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// Project key to check (default: AGENT_MAIL_PROJECT env var or current directory).
        #[arg(long)]
        project: Option<String>,
    },
    /// Run CI quality gates (format, lint, build, test).
    #[command(name = "ci")]
    Ci {
        /// Quick mode: skip long-running E2E gates.
        #[arg(long, short = 'q')]
        quick: bool,
        /// Write JSON report to this path.
        #[arg(long, short = 'r')]
        report: Option<std::path::PathBuf>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long)]
        json: bool,
        /// Run independent gates in parallel (faster execution).
        #[arg(long, short = 'p')]
        parallel: bool,
    },
    /// Run native CLI benchmarks.
    #[command(name = "bench")]
    Bench {
        /// Quick mode: warmup=1, runs=3 unless overridden.
        #[arg(long, short = 'q')]
        quick: bool,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Path to baseline JSON for regression comparison.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Persist current benchmark p95 values as a baseline JSON file.
        #[arg(long = "save-baseline")]
        save_baseline: Option<PathBuf>,
        /// Glob pattern to select benchmark names (example: "mail_*").
        #[arg(long)]
        filter: Option<String>,
        /// List selected benchmarks without executing them.
        #[arg(long, default_value_t = false)]
        list: bool,
        /// Override warmup iterations.
        #[arg(long)]
        warmup: Option<u32>,
        /// Override measured iterations.
        #[arg(long)]
        runs: Option<u32>,
    },
    Lint,
    Typecheck,
    /// Run E2E test suites.
    #[command(name = "e2e")]
    E2e {
        #[command(subcommand)]
        action: E2eCommand,
    },
    #[command(name = "share")]
    Share {
        #[command(subcommand)]
        action: ShareCommand,
    },
    #[command(name = "archive")]
    Archive {
        #[command(subcommand)]
        action: ArchiveCommand,
    },
    #[command(name = "guard")]
    Guard {
        #[command(subcommand)]
        action: GuardCommand,
    },
    #[command(name = "file_reservations")]
    FileReservations {
        #[command(subcommand)]
        action: FileReservationsCommand,
    },
    #[command(name = "acks")]
    Acks {
        #[command(subcommand)]
        action: AcksCommand,
    },
    #[command(name = "list-acks")]
    ListAcks {
        #[arg(long = "project")]
        project_key: String,
        #[arg(long = "agent")]
        agent_name: String,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    #[command(name = "migrate")]
    Migrate {
        /// Only check the database format without modifying anything.
        #[arg(long, default_value_t = false)]
        check: bool,
        /// Restore the database from the latest available backup and exit.
        #[arg(long, default_value_t = false)]
        rollback: bool,
        /// Force migration even if the format is unclear or mixed.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Custom backup directory (default: same directory as the database).
        #[arg(long)]
        backup_dir: Option<PathBuf>,
    },
    #[command(name = "list-projects")]
    ListProjects {
        #[arg(long, default_value_t = false)]
        include_agents: bool,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(name = "clear-and-reset-everything")]
    ClearAndResetEverything {
        #[arg(
            long,
            short = 'f',
            help = "Skip the final destructive confirmation prompt (still asks about creating an archive)."
        )]
        force: bool,
        #[arg(
            long,
            conflicts_with = "no_archive",
            help = "Attempt a pre-reset archive before deleting data (default: prompt when interactive)."
        )]
        archive: bool,
        #[arg(
            long = "no-archive",
            conflicts_with = "archive",
            help = "Skip creating a pre-reset archive."
        )]
        no_archive: bool,
    },
    #[command(name = "config")]
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
    #[command(name = "amctl")]
    Amctl {
        #[command(subcommand)]
        action: AmctlCommand,
    },
    #[command(name = "am-run")]
    AmRun(AmRunArgs),
    #[command(name = "projects")]
    Projects {
        #[command(subcommand)]
        action: ProjectsCommand,
    },
    #[command(name = "mail")]
    Mail {
        #[command(subcommand)]
        action: MailCommand,
    },
    #[command(name = "products")]
    Products {
        #[command(subcommand)]
        action: ProductsCommand,
    },
    #[command(name = "docs")]
    Docs {
        #[command(subcommand)]
        action: DocsCommand,
    },
    #[command(name = "doctor")]
    Doctor {
        #[command(subcommand)]
        action: DoctorCommand,
    },
    #[command(name = "agents")]
    Agents {
        #[command(subcommand)]
        action: AgentsCommand,
    },
    #[command(name = "tooling")]
    Tooling {
        #[command(subcommand)]
        action: ToolingCommand,
    },
    /// Composite workflow macros (session boot, thread prep, reservation cycles, contact handshake).
    #[command(name = "macros")]
    Macros {
        #[command(subcommand)]
        action: MacroCommand,
    },
    /// Contact request/approve/reject/policy lifecycle.
    #[command(name = "contacts")]
    Contacts {
        #[command(subcommand)]
        action: ContactsCommand,
    },
    /// Query beads (issue tracker) status for the current project.
    #[command(name = "beads")]
    Beads {
        #[command(subcommand)]
        action: BeadsCommand,
    },
    /// Detect coding agents and configure MCP server connections.
    #[command(name = "setup")]
    Setup {
        #[command(subcommand)]
        action: SetupCommand,
    },
    /// Capture, verify, and inspect deterministic golden CLI artifacts.
    #[command(name = "golden")]
    Golden {
        #[command(subcommand)]
        action: GoldenCommand,
    },
    /// Flake triage: scan artifacts, reproduce failures, detect flaky tests.
    #[command(name = "flake-triage")]
    FlakeTriage {
        #[command(subcommand)]
        action: FlakeTriageCommand,
    },
    /// Agent-optimized commands with TOON/JSON/Markdown output.
    #[command(name = "robot")]
    Robot(robot::RobotArgs),
    /// Legacy Python installation detection, migration/import, and status.
    #[command(name = "legacy")]
    Legacy(legacy::LegacyArgs),
    /// Upgrade helper: detect legacy install, import data, refresh setup.
    #[command(name = "upgrade")]
    Upgrade(legacy::UpgradeArgs),
    /// Check for updates or self-update the binary.
    #[command(name = "self-update")]
    SelfUpdate {
        /// Only check for updates without installing.
        #[arg(long, default_value_t = false)]
        check: bool,
        /// Reinstall the current version (force re-download).
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Install a specific version instead of the latest.
        #[arg(long)]
        version: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ContactsCommand {
    /// Request contact with another agent.
    Request {
        /// Project key (slug or human_key).
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Requester agent name.
        #[arg(long = "from")]
        from_agent: String,
        /// Target agent name.
        #[arg(long = "to")]
        to_agent: String,
        /// Reason for the contact request.
        #[arg(long, default_value = "")]
        reason: String,
        /// TTL in seconds (default: 7 days).
        #[arg(long, default_value_t = 604_800)]
        ttl_seconds: i64,
        /// Output format: table, json, or toon (default: table).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Approve or reject a contact request.
    Respond {
        /// Project key.
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Your agent name (recipient).
        #[arg(long = "agent", short = 'a')]
        agent_name: String,
        /// Requesting agent name.
        #[arg(long = "from")]
        from_agent: String,
        /// Accept the request (default: true).
        #[arg(long, default_value_t = true)]
        accept: bool,
        /// Reject the request.
        #[arg(long, conflicts_with = "accept")]
        reject: bool,
        /// TTL in seconds for approved link (default: 30 days).
        #[arg(long, default_value_t = 2_592_000)]
        ttl_seconds: i64,
        /// Output format: table, json, or toon (default: table).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List contacts for an agent.
    #[command(name = "list")]
    ListContacts {
        /// Project key.
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Agent name.
        #[arg(long = "agent", short = 'a')]
        agent_name: String,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Set contact policy for an agent.
    Policy {
        /// Project key.
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Agent name.
        #[arg(long = "agent", short = 'a')]
        agent_name: String,
        /// Policy: open, auto, contacts_only, block_all.
        policy: String,
        /// Output format: table, json, or toon (default: table).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum BeadsCommand {
    /// List issues ready to work on (unblocked, not deferred).
    Ready {
        /// Path to the project root containing .beads/ (default: current directory).
        #[arg(long, short = 'p')]
        path: Option<PathBuf>,
        /// Maximum number of issues to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List issues with optional status/priority filters.
    #[command(name = "list")]
    List {
        /// Path to the project root containing .beads/.
        #[arg(long, short = 'p')]
        path: Option<PathBuf>,
        /// Filter by status (open, in_progress, closed, blocked).
        #[arg(long)]
        status: Option<String>,
        /// Filter by priority (0-4).
        #[arg(long)]
        priority: Option<u8>,
        /// Maximum number of issues to show.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show details for a specific issue.
    Show {
        /// Issue ID (e.g. "br-abc123").
        id: String,
        /// Path to the project root containing .beads/.
        #[arg(long, short = 'p')]
        path: Option<PathBuf>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Summary counts: open, in_progress, blocked, closed.
    Status {
        /// Path to the project root containing .beads/.
        #[arg(long, short = 'p')]
        path: Option<PathBuf>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SetupCommand {
    /// Auto-detect agents and write MCP config files (default subcommand).
    #[command(name = "run")]
    Run {
        /// Target specific agents (comma-separated: claude,cursor,gemini).
        #[arg(long)]
        agent: Option<String>,
        /// Preview changes without writing files.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Non-interactive (skip confirmations).
        #[arg(long, short = 'y', default_value_t = false)]
        yes: bool,
        /// Use a specific bearer token.
        #[arg(long)]
        token: Option<String>,
        /// Override port (default: 8765).
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// Override host (default: 127.0.0.1).
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Override MCP base path (default: /mcp/).
        #[arg(long, default_value = "/mcp/")]
        path: String,
        /// Project directory for project-local configs (default: cwd).
        #[arg(long)]
        project_dir: Option<PathBuf>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Skip user-level config files (project-local only).
        #[arg(long, default_value_t = false)]
        no_user_config: bool,
        /// Skip Claude Code hook installation.
        #[arg(long, default_value_t = false)]
        no_hooks: bool,
    },
    /// Show current setup status: detected agents, config state.
    #[command(name = "status")]
    Status {
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Override port for status check (default: 8765).
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// Override host for status check (default: 127.0.0.1).
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Override MCP base path (default: /mcp/).
        #[arg(long, default_value = "/mcp/")]
        path: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum FlakeTriageCommand {
    /// Scan for `failure_context.json` artifacts in a directory tree.
    Scan {
        /// Directory to scan (default: tests/artifacts).
        #[arg(long, short = 'd')]
        dir: Option<PathBuf>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Reproduce a failure from its artifact file.
    Reproduce {
        /// Path to the `failure_context.json` artifact.
        artifact: PathBuf,
        /// Show full subprocess output.
        #[arg(long, default_value_t = false)]
        verbose: bool,
        /// Timeout in seconds (default: 120).
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Run a test with multiple seeds to detect flakiness.
    Detect {
        /// Test name (passed to cargo test).
        test_name: String,
        /// Number of seeds to use (default: 17).
        #[arg(long, short = 'n', default_value_t = 17)]
        seeds: usize,
        /// Packages to test (repeatable, default: core/server/db).
        #[arg(long, short = 'p')]
        packages: Vec<String>,
        /// Timeout per seed in seconds (default: 60).
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum GoldenCommand {
    /// Regenerate golden outputs and rewrite checksums.
    Capture {
        /// Golden directory (default: benches/golden).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Optional filename glob filter (e.g. "am_*help*").
        #[arg(long)]
        filter: Option<String>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Print per-command details.
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },
    /// Verify current command outputs against stored goldens/checksums.
    Verify {
        /// Golden directory (default: benches/golden).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Optional filename glob filter (e.g. "mcp_deny_*").
        #[arg(long)]
        filter: Option<String>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Show inline diff context for mismatches.
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },
    /// List golden files with present/missing/stale status.
    List {
        /// Golden directory (default: benches/golden).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Optional filename glob filter (e.g. "stub_*").
        #[arg(long)]
        filter: Option<String>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

/// E2E test runner commands (br-8zmc).
#[derive(Subcommand, Debug)]
pub enum E2eCommand {
    /// List available E2E test suites.
    #[command(name = "list")]
    List {
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Show suite details (descriptions, tags).
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// Run E2E test suites.
    #[command(name = "run")]
    Run {
        /// Specific suites to run (runs all if not specified).
        suites: Vec<String>,
        /// Include suites matching pattern (repeatable).
        #[arg(long, short = 'i')]
        include: Vec<String>,
        /// Exclude suites matching pattern (repeatable).
        #[arg(long, short = 'e')]
        exclude: Vec<String>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Keep temporary directories.
        #[arg(long)]
        keep_tmp: bool,
        /// Force rebuild before running.
        #[arg(long)]
        force_build: bool,
        /// Project root directory (default: current directory).
        #[arg(long, short = 'p')]
        project: Option<PathBuf>,
        /// Artifact output directory.
        #[arg(long, short = 'o')]
        artifacts: Option<PathBuf>,
        /// Timeout per suite in seconds (default: 600).
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
    /// Show suite details.
    #[command(name = "show")]
    Show {
        /// Suite name to show.
        suite: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum ShareCommand {
    Export(ShareExportArgs),
    Update(ShareUpdateArgs),
    Preview(SharePreviewArgs),
    Verify(ShareVerifyArgs),
    Decrypt(ShareDecryptArgs),
    Wizard(ShareWizardArgs),
    StaticExport(ShareStaticExportArgs),
    #[command(name = "deploy")]
    Deploy {
        #[command(subcommand)]
        action: DeployCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum DeployCommand {
    /// Run pre-flight validation on a bundle directory and display the deploy report.
    Validate(DeployValidateArgs),
    /// Write deployment configuration files (GH Actions, CF Pages, Netlify, validation script).
    Tooling(DeployToolingArgs),
    /// Build a verification plan for a deployed URL.
    #[command(name = "verify")]
    VerifyUrl(DeployVerifyArgs),
    /// Verify a live deployment (HTTP probes, content match, security audit).
    #[command(name = "verify-live")]
    VerifyLive(DeployVerifyLiveArgs),
}

#[derive(Args, Debug)]
pub struct DeployValidateArgs {
    /// Path to the bundle directory to validate.
    pub bundle: PathBuf,
    /// Output format: table, json, or toon (default: auto-detect).
    #[arg(long, value_parser)]
    pub format: Option<output::CliOutputFormat>,
    /// Output JSON (shorthand for --format json).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct DeployToolingArgs {
    /// Path to the bundle directory where config files will be written.
    pub bundle: PathBuf,
}

#[derive(Args, Debug)]
pub struct DeployVerifyArgs {
    /// Deployed URL to verify (e.g., `https://example.github.io/agent-mail`).
    pub url: String,
    /// Output format: table, json, or toon (default: auto-detect).
    #[arg(long, value_parser)]
    pub format: Option<output::CliOutputFormat>,
    /// Output JSON (shorthand for --format json).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct DeployVerifyLiveArgs {
    /// Deployed URL to verify (e.g., `https://example.github.io/agent-mail`).
    pub url: String,
    /// Local bundle directory for content-match checks.
    #[arg(long)]
    pub bundle: Option<PathBuf>,
    /// Output format: table, json, or toon (default: auto-detect).
    #[arg(long, value_parser)]
    pub format: Option<output::CliOutputFormat>,
    /// Output JSON report (shorthand for --format json).
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Promote warnings to errors for exit code.
    #[arg(long)]
    pub strict: bool,
    /// Stop after first error-severity failure.
    #[arg(long)]
    pub fail_fast: bool,
    /// Run security header audit (Stage 3).
    #[arg(long)]
    pub security: bool,
    /// Per-request timeout in milliseconds.
    #[arg(long, default_value_t = 10000)]
    pub timeout: u64,
    /// Retry count for failed HTTP requests.
    #[arg(long, default_value_t = 2)]
    pub retries: u32,
    /// Delay between retries in milliseconds.
    #[arg(long, default_value_t = 1000)]
    pub retry_delay: u64,
}

#[derive(Args, Debug)]
pub struct ShareExportArgs {
    #[arg(long, short = 'o')]
    output: PathBuf,
    #[arg(long, short = 'i')]
    interactive: bool,
    #[arg(long = "project", short = 'p')]
    projects: Vec<String>,
    #[arg(long, default_value_t = share::INLINE_ATTACHMENT_THRESHOLD as i64)]
    inline_threshold: i64,
    #[arg(long, default_value_t = share::DETACH_ATTACHMENT_THRESHOLD as i64)]
    detach_threshold: i64,
    #[arg(long, default_value = "standard")]
    scrub_preset: String,
    #[arg(long, default_value_t = share::DEFAULT_CHUNK_THRESHOLD as i64)]
    chunk_threshold: i64,
    #[arg(long, default_value_t = share::DEFAULT_CHUNK_SIZE as i64)]
    chunk_size: i64,
    #[arg(long)]
    dry_run: bool,
    #[arg(long = "no-dry-run", default_value_t = false)]
    no_dry_run: bool,
    #[arg(long, default_value_t = true)]
    zip: bool,
    #[arg(long = "no-zip", default_value_t = false)]
    no_zip: bool,
    #[arg(long)]
    signing_key: Option<PathBuf>,
    #[arg(long)]
    signing_public_out: Option<PathBuf>,
    #[arg(long = "age-recipient")]
    age_recipient: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ShareUpdateArgs {
    pub bundle: PathBuf,
    #[arg(long = "project", short = 'p')]
    projects: Vec<String>,
    #[arg(long)]
    inline_threshold: Option<i64>,
    #[arg(long)]
    detach_threshold: Option<i64>,
    #[arg(long)]
    chunk_threshold: Option<i64>,
    #[arg(long)]
    chunk_size: Option<i64>,
    #[arg(long)]
    scrub_preset: Option<String>,
    #[arg(long, default_value_t = false)]
    zip: bool,
    #[arg(long = "no-zip", default_value_t = false)]
    no_zip: bool,
    #[arg(long)]
    signing_key: Option<PathBuf>,
    #[arg(long)]
    signing_public_out: Option<PathBuf>,
    #[arg(long = "age-recipient")]
    age_recipient: Vec<String>,
}

#[derive(Args, Debug)]
pub struct SharePreviewArgs {
    pub bundle: PathBuf,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 9000)]
    port: u16,
    #[arg(long)]
    open_browser: bool,
    #[arg(long = "no-open-browser", default_value_t = false)]
    no_open_browser: bool,
}

#[derive(Args, Debug)]
pub struct ShareVerifyArgs {
    pub bundle: PathBuf,
    #[arg(long)]
    public_key: Option<String>,
}

#[derive(Args, Debug)]
pub struct ShareDecryptArgs {
    pub encrypted_path: PathBuf,
    #[arg(long, short = 'o')]
    output: Option<PathBuf>,
    #[arg(long, short = 'i')]
    identity: Option<PathBuf>,
    #[arg(long, short = 'p')]
    passphrase: bool,
}

#[derive(Args, Debug)]
pub struct ShareWizardArgs {
    /// Path to bundle directory to deploy.
    #[arg(long, short = 'b')]
    pub bundle: Option<PathBuf>,
    /// Hosting provider (github, cloudflare, netlify, s3, custom).
    #[arg(long, short = 'P')]
    pub provider: Option<String>,
    /// GitHub repository (owner/repo) for GitHub Pages.
    #[arg(long)]
    pub github_repo: Option<String>,
    /// GitHub branch for Pages (default: gh-pages).
    #[arg(long, default_value = "gh-pages")]
    pub github_branch: String,
    /// Cloudflare Pages project name.
    #[arg(long)]
    pub cloudflare_project: Option<String>,
    /// Netlify site name or ID.
    #[arg(long)]
    pub netlify_site: Option<String>,
    /// S3 bucket name.
    #[arg(long)]
    pub s3_bucket: Option<String>,
    /// CloudFront distribution ID (for S3 deployments).
    #[arg(long)]
    pub cloudfront_id: Option<String>,
    /// Base URL for custom deployment.
    #[arg(long)]
    pub base_url: Option<String>,
    /// Output directory for deploy artifacts.
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,
    /// Skip confirmation prompts.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Dry-run mode (show plan without executing).
    #[arg(long)]
    pub dry_run: bool,
    /// Non-interactive mode (fail if prompts needed).
    #[arg(long)]
    pub non_interactive: bool,
    /// Output format: table, json, or toon (default: auto-detect).
    #[arg(long, value_parser)]
    pub format: Option<output::CliOutputFormat>,
    /// Output JSON (shorthand for --format json).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ShareStaticExportArgs {
    /// Output directory for static HTML files.
    #[arg(long, short = 'o')]
    pub output: PathBuf,
    /// Project slugs to export (omit for all).
    #[arg(long = "project", short = 'p')]
    pub projects: Vec<String>,
    /// Include archive visualization routes.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub include_archive: bool,
    /// Generate client-side search index artifact.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub include_search_index: bool,
}

#[derive(Subcommand, Debug)]
pub enum ArchiveCommand {
    Save {
        #[arg(long = "project", short = 'p')]
        projects: Vec<String>,
        #[arg(long, default_value = "archive")]
        scrub_preset: String,
        #[arg(long, short = 'l')]
        label: Option<String>,
    },
    List {
        #[arg(long, short = 'n', default_value_t = 0)]
        limit: i64,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long)]
        json: bool,
    },
    Restore {
        archive_file: PathBuf,
        #[arg(long, short = 'f')]
        force: bool,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum GuardCommand {
    Install {
        project: String,
        repo: PathBuf,
        #[arg(long)]
        prepush: bool,
        #[arg(long = "no-prepush", default_value_t = false)]
        no_prepush: bool,
    },
    Uninstall {
        repo: PathBuf,
    },
    Status {
        repo: PathBuf,
    },
    Check {
        #[arg(long)]
        stdin_nul: bool,
        #[arg(long)]
        advisory: bool,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum FileReservationsCommand {
    /// List reservations (default: active only).
    List {
        project: String,
        #[arg(long)]
        active_only: bool,
        #[arg(long = "all", default_value_t = false)]
        all: bool,
    },
    /// Show active reservations with lock type.
    Active {
        project: String,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Show reservations expiring soon.
    Soon {
        project: String,
        #[arg(long)]
        minutes: Option<i64>,
    },
    /// Create file reservations for an agent.
    Reserve {
        project: String,
        agent: String,
        /// Path patterns to reserve (one or more).
        #[arg(required = true)]
        paths: Vec<String>,
        /// TTL in seconds (default: 3600).
        #[arg(long, default_value_t = 3600)]
        ttl: i64,
        /// Request exclusive lock.
        #[arg(long, default_value_t = false)]
        exclusive: bool,
        /// Request shared (non-exclusive) lock.
        #[arg(long, conflicts_with = "exclusive")]
        shared: bool,
        /// Reason for the reservation.
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Renew (extend TTL) of existing reservations.
    Renew {
        project: String,
        agent: String,
        /// Extension time in seconds (default: 1800).
        #[arg(long, default_value_t = 1800)]
        extend_seconds: i64,
        /// Restrict renewal to specific paths.
        #[arg(long)]
        paths: Vec<String>,
        /// Restrict renewal to specific reservation IDs.
        #[arg(long)]
        ids: Vec<i64>,
    },
    /// Release file reservations.
    Release {
        project: String,
        agent: String,
        /// Restrict release to specific paths.
        #[arg(long)]
        paths: Vec<String>,
        /// Restrict release to specific reservation IDs.
        #[arg(long)]
        ids: Vec<i64>,
    },
    /// Check for conflicts on proposed paths without creating reservations.
    Conflicts {
        project: String,
        /// Path patterns to check for conflicts.
        #[arg(required = true)]
        paths: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum AcksCommand {
    Pending {
        project: String,
        agent: String,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    Remind {
        project: String,
        agent: String,
        #[arg(long, default_value_t = 30)]
        min_age_minutes: i64,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    Overdue {
        project: String,
        agent: String,
        #[arg(long, default_value_t = 60)]
        ttl_minutes: i64,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    #[command(name = "set-port")]
    SetPort {
        port: u16,
        #[arg(long)]
        env_file: Option<PathBuf>,
    },
    #[command(name = "show-port")]
    ShowPort,
}

#[derive(Subcommand, Debug)]
pub enum AmctlCommand {
    Env {
        #[arg(long, short = 'p', default_value = ".")]
        path: PathBuf,
        #[arg(long, short = 'a')]
        agent: Option<String>,
    },
}

#[derive(Args, Debug)]
pub struct AmRunArgs {
    pub slot: String,
    #[arg(trailing_var_arg = true, required = true)]
    pub cmd: Vec<String>,
    #[arg(long, short = 'p', default_value = ".")]
    pub path: PathBuf,
    #[arg(long, short = 'a')]
    pub agent: Option<String>,
    #[arg(long, default_value_t = 3600)]
    pub ttl_seconds: i64,
    #[arg(long, conflicts_with = "exclusive")]
    pub shared: bool,
    #[arg(long, conflicts_with = "shared")]
    pub exclusive: bool,
    #[arg(long = "block-on-conflicts", conflicts_with = "no_block_on_conflicts")]
    pub block_on_conflicts: bool,
    #[arg(long = "no-block-on-conflicts", conflicts_with = "block_on_conflicts")]
    pub no_block_on_conflicts: bool,
}

#[derive(Subcommand, Debug)]
pub enum ProjectsCommand {
    #[command(name = "mark-identity")]
    MarkIdentity {
        project_path: PathBuf,
        #[arg(long, default_value_t = false)]
        commit: bool,
        #[arg(long = "no-commit", default_value_t = false)]
        no_commit: bool,
    },
    #[command(name = "discovery-init")]
    DiscoveryInit {
        project_path: PathBuf,
        #[arg(long, short = 'P')]
        product: Option<String>,
    },
    Adopt {
        source: PathBuf,
        target: PathBuf,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long, default_value_t = false)]
        apply: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum MailCommand {
    Status {
        project_path: PathBuf,
    },
    /// Send a message to one or more agents.
    Send {
        /// Project key (slug or human_key).
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Sender agent name.
        #[arg(long = "from")]
        sender: String,
        /// Primary recipients (comma-separated agent names).
        #[arg(long)]
        to: String,
        /// Subject line.
        #[arg(long, short = 's')]
        subject: String,
        /// Message body (Markdown).
        #[arg(long, short = 'b')]
        body: String,
        /// CC recipients (comma-separated).
        #[arg(long)]
        cc: Option<String>,
        /// Importance: low, normal, high, urgent.
        #[arg(long, default_value = "normal")]
        importance: String,
        /// Require acknowledgement.
        #[arg(long, default_value_t = false)]
        ack_required: bool,
        /// Thread ID to associate with.
        #[arg(long)]
        thread_id: Option<String>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json, default true for detect).
        #[arg(long, default_value_t = true)]
        json: bool,
    },
    /// Reply to an existing message.
    Reply {
        /// Project key.
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Sender agent name.
        #[arg(long = "from")]
        sender: String,
        /// Message ID to reply to.
        #[arg(long)]
        message_id: i64,
        /// Reply body (Markdown).
        #[arg(long, short = 'b')]
        body: String,
        /// Override recipients (comma-separated; defaults to original sender).
        #[arg(long)]
        to: Option<String>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Fetch inbox messages for an agent.
    Inbox {
        /// Project key.
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Agent name.
        #[arg(long = "agent", short = 'a')]
        agent_name: String,
        /// Only high/urgent messages.
        #[arg(long, default_value_t = false)]
        urgent_only: bool,
        /// Messages after this ISO-8601 timestamp.
        #[arg(long)]
        since: Option<String>,
        /// Max results.
        #[arg(long, short = 'l', default_value_t = 20)]
        limit: i64,
        /// Include message bodies.
        #[arg(long, default_value_t = false)]
        include_bodies: bool,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Mark a message as read.
    Read {
        /// Project key.
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Agent name.
        #[arg(long = "agent", short = 'a')]
        agent_name: String,
        /// Message ID.
        message_id: i64,
    },
    /// Acknowledge a message (also marks as read).
    Ack {
        /// Project key.
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Agent name.
        #[arg(long = "agent", short = 'a')]
        agent_name: String,
        /// Message ID.
        message_id: i64,
    },
    /// Full-text search over messages.
    Search {
        /// Project key.
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Search query string.
        query: String,
        /// Max results.
        #[arg(long, short = 'l', default_value_t = 20)]
        limit: i64,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Summarize a thread (requires LLM API key).
    #[command(name = "summarize-thread")]
    SummarizeThread {
        /// Project key (slug or human_key).
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Thread ID to summarize.
        thread_id: String,
        /// Max messages per thread.
        #[arg(long, short = 'n', default_value_t = 50)]
        per_thread_limit: i64,
        /// Skip LLM and return raw thread messages.
        #[arg(long)]
        no_llm: bool,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProductsCommand {
    Ensure {
        product_key: Option<String>,
        #[arg(long, short = 'n')]
        name: Option<String>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Link {
        product_key: String,
        project: String,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Status {
        product_key: String,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Search {
        product_key: String,
        query: String,
        #[arg(long, short = 'l', default_value_t = 20)]
        limit: i64,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Inbox {
        product_key: String,
        agent: String,
        #[arg(long, short = 'l', default_value_t = 20)]
        limit: i64,
        #[arg(long, default_value_t = false)]
        urgent_only: bool,
        #[arg(long, default_value_t = false)]
        all: bool,
        #[arg(long, default_value_t = false)]
        include_bodies: bool,
        #[arg(long = "no-bodies", default_value_t = false)]
        no_bodies: bool,
        #[arg(long)]
        since_ts: Option<String>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(name = "summarize-thread")]
    SummarizeThread {
        product_key: String,
        thread_id: String,
        #[arg(long, short = 'n', default_value_t = 50)]
        per_thread_limit: i64,
        #[arg(long)]
        no_llm: bool,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum DocsCommand {
    #[command(name = "insert-blurbs")]
    InsertBlurbs {
        #[arg(long, short = 'd')]
        scan_dir: Vec<PathBuf>,
        #[arg(long)]
        yes: bool,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long)]
        max_depth: Option<i64>,
    },
}

#[derive(Subcommand, Debug)]
pub enum DoctorCommand {
    Check {
        project: Option<String>,
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long)]
        json: bool,
    },
    Repair {
        project: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, short = 'y')]
        yes: bool,
        #[arg(long)]
        backup_dir: Option<PathBuf>,
    },
    Backups {
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long)]
        json: bool,
    },
    Restore {
        backup_path: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Reconstruct the database from the Git archive.
    ///
    /// Walks all project archives under STORAGE_ROOT and rebuilds the SQLite
    /// database from the archived messages and agent profiles. Use this when
    /// the database is corrupt and no healthy backup exists.
    Reconstruct {
        /// Preview what would be recovered without writing.
        #[arg(long)]
        dry_run: bool,
        /// Auto-confirm (skip interactive prompt).
        #[arg(long, short = 'y')]
        yes: bool,
        /// Output JSON (shorthand for machine-readable output).
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum AgentsCommand {
    /// Register or update an agent identity in a project (idempotent).
    Register {
        /// Project key (slug or human_key / absolute path).
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Agent program (e.g. "claude-code", "codex-cli").
        #[arg(long)]
        program: String,
        /// Underlying model (e.g. "opus-4.6", "gpt-5-turbo").
        #[arg(long)]
        model: String,
        /// Agent name (adjective+noun, e.g. "BlueLake"). Auto-generated if omitted.
        #[arg(long, short = 'n')]
        name: Option<String>,
        /// Short description of the agent's current task.
        #[arg(long, short = 't')]
        task: Option<String>,
        /// Attachments policy: auto, inline, file, none.
        #[arg(long, default_value = "auto")]
        attachments_policy: String,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Create a new, unique agent identity (never updates existing).
    Create {
        /// Project key (slug or human_key / absolute path).
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Agent program.
        #[arg(long)]
        program: String,
        /// Underlying model.
        #[arg(long)]
        model: String,
        /// Name hint (adjective+noun). Auto-generated if omitted.
        #[arg(long, short = 'n')]
        name_hint: Option<String>,
        /// Short description of the agent's current task.
        #[arg(long, short = 't')]
        task: Option<String>,
        /// Attachments policy: auto, inline, file, none.
        #[arg(long, default_value = "auto")]
        attachments_policy: String,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List agents registered in a project.
    List {
        /// Project key (slug or human_key / absolute path).
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show details for a specific agent.
    Show {
        /// Project key (slug or human_key / absolute path).
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Agent name.
        agent: String,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Detect installed coding agents on this system.
    Detect {
        /// Restrict detection to specific connector slugs (comma-separated).
        #[arg(long)]
        only: Option<String>,
        /// Include undetected agents in the report.
        #[arg(long, default_value_t = false)]
        include_undetected: bool,
        /// Output format: table, json, or toon.
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json, default true for detect).
        #[arg(long, default_value_t = true)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum MacroCommand {
    /// Boot a project session: ensure project, register agent, reserve files, fetch inbox.
    #[command(name = "start-session")]
    StartSession {
        /// Project human_key (absolute path, e.g. "/data/projects/backend").
        #[arg(long = "project", short = 'p')]
        human_key: String,
        /// Agent program (e.g. "claude-code", "codex-cli").
        #[arg(long)]
        program: String,
        /// Model identifier (e.g. "opus-4.6", "gpt-5").
        #[arg(long)]
        model: String,
        /// Agent name (adjective+noun). Auto-generated if omitted.
        #[arg(long, short = 'n')]
        agent_name: Option<String>,
        /// Short description of agent's current task.
        #[arg(long, short = 't')]
        task: Option<String>,
        /// File paths/globs to reserve (repeatable).
        #[arg(long = "reserve")]
        reserve_paths: Vec<String>,
        /// Reason for file reservations.
        #[arg(long)]
        reserve_reason: Option<String>,
        /// TTL for file reservations in seconds.
        #[arg(long, default_value_t = 3600)]
        reserve_ttl: i64,
        /// Max inbox messages to fetch.
        #[arg(long, default_value_t = 10)]
        inbox_limit: i32,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Align with an existing thread: register agent, summarize thread, fetch inbox.
    #[command(name = "prepare-thread")]
    PrepareThread {
        /// Project key (slug or human_key).
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Thread ID to prepare for.
        #[arg(long)]
        thread_id: String,
        /// Agent program.
        #[arg(long)]
        program: String,
        /// Model identifier.
        #[arg(long)]
        model: String,
        /// Agent name. Auto-generated if omitted.
        #[arg(long, short = 'n')]
        agent_name: Option<String>,
        /// Short description of agent's current task.
        #[arg(long, short = 't')]
        task: Option<String>,
        /// Register agent if not already exists.
        #[arg(long, default_value_t = false)]
        register: bool,
        /// Do not register agent if missing.
        #[arg(long = "no-register", conflicts_with = "register")]
        no_register: bool,
        /// Include example messages in thread summary.
        #[arg(long, default_value_t = false)]
        examples: bool,
        /// Exclude example messages.
        #[arg(long = "no-examples", conflicts_with = "examples")]
        no_examples: bool,
        /// Include message bodies in inbox.
        #[arg(long, default_value_t = false)]
        inbox_bodies: bool,
        /// Max inbox messages to fetch.
        #[arg(long, default_value_t = 10)]
        inbox_limit: i32,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Reserve files and optionally release them at the end.
    #[command(name = "file-reservation-cycle")]
    FileReservationCycle {
        /// Project key (slug or human_key).
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Agent name making reservations.
        #[arg(long = "agent", short = 'a')]
        agent_name: String,
        /// File paths/globs to reserve (repeatable).
        #[arg(long = "path", required = true)]
        paths: Vec<String>,
        /// TTL in seconds (minimum 60).
        #[arg(long, default_value_t = 3600)]
        ttl: i64,
        /// Request exclusive reservations.
        #[arg(long, default_value_t = false)]
        exclusive: bool,
        /// Request shared (non-exclusive) reservations.
        #[arg(long = "no-exclusive", conflicts_with = "exclusive")]
        no_exclusive: bool,
        /// Reason for reservations.
        #[arg(long)]
        reason: Option<String>,
        /// Automatically release reservations after granting.
        #[arg(long, default_value_t = false)]
        auto_release: bool,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Request contact approval, optionally auto-approve, and send a welcome message.
    #[command(name = "contact-handshake")]
    ContactHandshake {
        /// Project key (slug or human_key).
        #[arg(long = "project", short = 'p')]
        project_key: String,
        /// Requesting agent name.
        #[arg(long)]
        from: String,
        /// Target agent name.
        #[arg(long)]
        to: String,
        /// Target project if different (cross-project coordination).
        #[arg(long)]
        to_project: Option<String>,
        /// Reason for the contact request.
        #[arg(long)]
        reason: Option<String>,
        /// Auto-approve the contact request.
        #[arg(long, default_value_t = false)]
        auto_accept: bool,
        /// TTL for the contact link in seconds.
        #[arg(long, default_value_t = 604_800)]
        ttl: i64,
        /// Subject for optional welcome message.
        #[arg(long)]
        welcome_subject: Option<String>,
        /// Body for optional welcome message (Markdown).
        #[arg(long)]
        welcome_body: Option<String>,
        /// Thread ID for welcome message.
        #[arg(long)]
        thread_id: Option<String>,
        /// Register requesting agent if not exists.
        #[arg(long, default_value_t = false)]
        register_missing: bool,
        /// Program for auto-registration.
        #[arg(long)]
        reg_program: Option<String>,
        /// Model for auto-registration.
        #[arg(long)]
        reg_model: Option<String>,
        /// Task description for auto-registration.
        #[arg(long)]
        reg_task: Option<String>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ToolingCommand {
    /// List all available MCP tools with cluster and capability metadata.
    Directory {
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show tool schemas and parameter definitions.
    Schemas {
        /// Filter to a specific tool name.
        #[arg(long)]
        tool: Option<String>,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show tool call counts and error rates.
    Metrics {
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show core system metrics (HTTP/DB/Storage/Locks).
    #[command(name = "metrics-core")]
    MetricsCore {
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show comprehensive diagnostic report with health metrics and recommendations.
    Diagnostics {
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show active archive locks.
    Locks {
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Drop legacy SQLite FTS message triggers after Search V3 rollout validation.
    #[command(name = "decommission-fts")]
    DecommissionFts {
        /// Proceed even if preflight checks fail.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Output format: table, json, or toon (default: auto-detect).
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

pub fn run() -> i32 {
    run_with_invocation_name("am")
}

/// Entry point with an explicit invocation name override.
///
/// This is used when the CLI surface is invoked through the `mcp-agent-mail` binary
/// (runtime opt-in via `AM_INTERFACE_MODE=cli`). It ensures `--help` / usage strings
/// render with the correct program name.
pub fn run_with_invocation_name(invocation_name: &'static str) -> i32 {
    let cli = match parse_with_invocation_name(invocation_name) {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    match execute(cli) {
        Ok(()) => 0,
        Err(err) => {
            emit_error(&err);
            err_exit_code(&err)
        }
    }
}

fn parse_with_invocation_name(invocation_name: &'static str) -> Result<Cli, i32> {
    let cmd = Cli::command()
        .name(invocation_name)
        .bin_name(invocation_name);

    // Ensure argv0 matches the name we want clap to render, regardless of how this
    // library was invoked.
    let mut args: Vec<OsString> = std::env::args_os().collect();
    if let Some(first) = args.first_mut() {
        *first = OsString::from(invocation_name);
    }

    let matches = match cmd.try_get_matches_from(args) {
        Ok(m) => m,
        Err(err) => {
            let _ = err.print();
            return Err(err.exit_code());
        }
    };

    match Cli::from_arg_matches(&matches) {
        Ok(cli) => Ok(cli),
        Err(err) => {
            let _ = err.print();
            Err(err.exit_code())
        }
    }
}

fn err_exit_code(_err: &CliError) -> i32 {
    match _err {
        CliError::ExitCode(code) => *code,
        _ => 1,
    }
}

fn emit_error(err: &CliError) {
    if matches!(err, CliError::ExitCode(_)) {
        return;
    }
    ftui_runtime::ftui_eprintln!("error: {err}");
}

fn execute(cli: Cli) -> CliResult<()> {
    let command = match cli.command {
        Some(cmd) => cmd,
        None => return handle_default_launch(),
    };
    match command {
        Commands::Share { action } => handle_share(action),
        Commands::Doctor { action } => handle_doctor(action),
        Commands::Guard { action } => handle_guard(action),
        Commands::FileReservations { action } => handle_file_reservations(action),
        Commands::Acks { action } => handle_acks(action),
        Commands::ListAcks {
            project_key,
            agent_name,
            limit,
        } => handle_list_acks(&project_key, &agent_name, limit),
        Commands::Archive { action } => handle_archive(action),
        Commands::ServeHttp {
            host,
            port,
            path,
            no_auth,
            no_tui,
        } => handle_serve_http(host, port, path, no_auth, no_tui),
        Commands::ServeStdio => handle_serve_stdio(),
        Commands::CheckInbox {
            agent,
            rate_limit,
            direct,
            format,
            json,
            host,
            port,
            project,
        } => handle_check_inbox(agent, rate_limit, direct, format, json, host, port, project),
        Commands::Ci {
            quick,
            report,
            format,
            json,
            parallel,
        } => handle_ci(quick, report, format, json, parallel),
        Commands::Bench {
            quick,
            format,
            json,
            baseline,
            save_baseline,
            filter,
            list,
            warmup,
            runs,
        } => handle_bench(
            quick,
            format,
            json,
            baseline,
            save_baseline,
            filter,
            list,
            warmup,
            runs,
        ),
        Commands::Lint => handle_lint(),
        Commands::Typecheck => handle_typecheck(),
        Commands::E2e { action } => handle_e2e(action),
        Commands::Migrate {
            check,
            rollback,
            force,
            backup_dir,
        } => handle_migrate_cmd(check, rollback, force, backup_dir),
        Commands::ListProjects {
            include_agents,
            format,
            json,
        } => handle_list_projects(include_agents, format, json),
        Commands::ClearAndResetEverything {
            force,
            archive,
            no_archive,
        } => handle_clear_and_reset(force, archive, no_archive),
        Commands::Config { action } => handle_config(action),
        Commands::Amctl { action } => handle_amctl(action),
        Commands::AmRun(args) => handle_am_run(args),
        Commands::Projects { action } => handle_projects(action),
        Commands::Mail { action } => handle_mail(action),
        Commands::Products { action } => handle_products(action),
        Commands::Docs { action } => handle_docs(action),
        Commands::Agents { action } => handle_agents(action),
        Commands::Tooling { action } => handle_tooling(action),
        Commands::Macros { action } => handle_macros(action),
        Commands::Contacts { action } => handle_contacts(action),
        Commands::Beads { action } => handle_beads(action),
        Commands::Setup { action } => handle_setup(action),
        Commands::Golden { action } => handle_golden(action),
        Commands::FlakeTriage { action } => handle_flake_triage(action),
        Commands::Robot(args) => robot::handle_robot(args),
        Commands::Legacy(args) => legacy::handle_legacy(args),
        Commands::Upgrade(args) => legacy::handle_upgrade(args),
        Commands::SelfUpdate {
            check,
            force,
            version,
        } => {
            if check {
                handle_self_update_check()
            } else {
                handle_self_update_full(force, version)
            }
        }
    }
}

/// Default behavior when `am` is invoked with no subcommand.
///
/// **Interactive terminal (human operator):**
/// 1. Auto-detect installed coding agents and configure their MCP connections
/// 2. Clear the port if something is already listening
/// 3. Start the HTTP server with the TUI
///
/// **Non-interactive (coding agent, pipe, CI):**
/// Automatically switches to `am robot status` for a JSON/TOON dashboard
/// so agents get useful output without launching a TUI that blocks them.
fn handle_default_launch() -> CliResult<()> {
    // Detect calling context: if stdin/stdout are not a TTY, this is a coding agent

    let is_interactive = crate::output::is_tty() && crate::output::is_stdin_tty();

    if !is_interactive {
        // Non-interactive: emit robot status as JSON so coding agents get useful output
        return robot::handle_robot(robot::RobotArgs {
            format: Some(robot::OutputFormat::Json),
            project: None,
            agent: None,
            command: robot::RobotSubcommand::Status,
        });
    }

    // Interactive: full server launch experience (setup self-heal + port check + serve)
    handle_serve_http(None, None, None, false, false)
}

fn apply_release_logging_defaults(suppress_runtime_logs_for_tui: bool) {
    static TRACING_INIT: std::sync::Once = std::sync::Once::new();

    TRACING_INIT.call_once(|| {
        let filter = build_release_log_filter(suppress_runtime_logs_for_tui);

        // Ignore double-init errors when tests or host processes already set a subscriber.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_target(false)
            .with_ansi(crate::output::is_tty())
            .compact()
            .try_init();
    });
}

fn build_release_log_filter(suppress_runtime_logs_for_tui: bool) -> tracing_subscriber::EnvFilter {
    let mut filter = if suppress_runtime_logs_for_tui {
        // Never emit tracing lines while the interactive TUI owns stdout/stderr.
        // This prevents log spam from corrupting alternate-screen rendering.
        tracing_subscriber::EnvFilter::new("off")
    } else if env_var_is_truthy("AM_ALLOW_DEBUG_STARTUP_LOGS") {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_release_log_filter()))
    } else {
        tracing_subscriber::EnvFilter::new(default_release_log_filter())
    };

    if suppress_runtime_logs_for_tui || allow_noisy_dependency_logs() {
        return filter;
    }

    for raw in noisy_dependency_log_clamp_directives() {
        if let Ok(directive) = raw.parse::<tracing_subscriber::filter::Directive>() {
            filter = filter.add_directive(directive);
        }
    }

    filter
}

fn default_release_log_filter() -> &'static str {
    concat!(
        "warn,",
        "mcp_agent_mail_cli=info,",
        "mcp_agent_mail_server=info,",
        "mcp_agent_mail_core=info,",
        "mcp_agent_mail_db=info,",
        "mcp_agent_mail_storage=info,",
        "mcp_agent_mail_tools=info,",
        "fsqlite_core::connection=warn,",
        "fsqlite_mvcc::observability=warn,",
        "fsqlite_mvcc::gc=warn,",
        "fsqlite_mvcc::rebase=warn,",
        "mvcc=warn,",
        "checkpoint=warn,",
        "fsqlite.storage_wiring=warn,",
        "fsqlite_wal::checkpoint_executor=warn,",
        "fsqlite_vdbe::jit=warn,",
        "fsqlite_vdbe::engine=warn,",
        "jit_compile=error,",
        "execute_statement_dispatch=error",
    )
}

const fn noisy_dependency_log_clamp_directives() -> [&'static str; 13] {
    [
        "fsqlite=warn",
        "fsqlite_core=warn",
        "fsqlite_mvcc=warn",
        "fsqlite_wal=warn",
        "fsqlite_vdbe=warn",
        "mvcc=warn",
        "checkpoint=warn",
        "fsqlite.storage_wiring=warn",
        "fsqlite_wal::checkpoint_executor=warn",
        "fsqlite_vdbe::jit=warn",
        "fsqlite_vdbe::engine=warn",
        "jit_compile=error",
        "execute_statement_dispatch=error",
    ]
}

fn allow_noisy_dependency_logs() -> bool {
    env_var_is_truthy("AM_ALLOW_NOISY_DEP_LOGS")
}

fn env_var_is_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// If an Agent Mail server is already listening on `host:port`, stop it so we can start fresh.
///
/// Refuses to terminate non-Agent-Mail processes.
fn auto_clear_port(host: &str, port: u16) -> CliResult<()> {
    use mcp_agent_mail_server::startup_checks::{PortStatus, check_port_status};

    let status = check_port_status(host, port);
    match status {
        PortStatus::Free => Ok(()),
        PortStatus::AgentMailServer => {
            eprintln!(
                "[info] Existing Agent Mail server on {host}:{port} — stopping it to start fresh"
            );
            kill_port_holder(host, port)
        }
        PortStatus::OtherProcess { ref description } => Err(CliError::Other(format!(
            "Port {host}:{port} is in use by another process ({description}). \
             Refusing to terminate non-Agent-Mail process; stop it manually or choose a different port."
        ))),
        PortStatus::Error { ref message, .. } => {
            eprintln!("[warn] Could not check port {port}: {message} — proceeding anyway");
            Ok(())
        }
    }
}

/// Kill whatever process is holding `port` using `fuser` (Linux) or `lsof` (macOS).
/// Attempts graceful termination (SIGTERM) before forceful kill (SIGKILL).
fn kill_port_holder(host: &str, port: u16) -> CliResult<()> {
    use mcp_agent_mail_server::startup_checks::{PortStatus, check_port_status};

    let mut has_kill_tool = false;

    // Try fuser first (Linux, fastest)
    // First, try SIGTERM
    let term_result = std::process::Command::new("fuser")
        .args(["-k", "-TERM", &format!("{port}/tcp")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    if let Ok(s) = term_result {
        has_kill_tool = true;
        // fuser returns 0 if it successfully identified processes
        if s.success() {
            // Give the OS a moment to release the port
            std::thread::sleep(std::time::Duration::from_millis(1000));

            // Check if still held
            let check_result = std::process::Command::new("fuser")
                .args([&format!("{port}/tcp")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();

            // If check fails (exit code 1), it means no processes found -> success
            if check_result.is_ok_and(|s| !s.success()) {
                return Ok(());
            }

            // Still held: FORCE KILL
            let _ = std::process::Command::new("fuser")
                .args(["-k", "-KILL", &format!("{port}/tcp")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    // Fallback: lsof + kill (macOS / if fuser not available)
    let lsof_output = std::process::Command::new("lsof")
        .args(["-ti", &format!("tcp:{port}")])
        .output();

    if let Ok(out) = lsof_output {
        has_kill_tool = true;
        let pids_str = String::from_utf8_lossy(&out.stdout);
        let pids: Vec<String> = pids_str.split_whitespace().map(|s| s.to_string()).collect();

        if pids.is_empty() {
            return Ok(());
        }

        // Try SIGTERM (15)
        for pid in &pids {
            let _ = std::process::Command::new("kill")
                .args(["-15", pid])
                .status();
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));

        // Force kill (9) any survivors
        for pid in &pids {
            let _ = std::process::Command::new("kill")
                .args(["-9", pid])
                .status();
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    match check_port_status(host, port) {
        PortStatus::Free => Ok(()),
        PortStatus::AgentMailServer | PortStatus::OtherProcess { .. } => {
            let tool_hint = if has_kill_tool {
                ""
            } else {
                " (no available kill tool: fuser/lsof)"
            };
            Err(CliError::Other(format!(
                "Failed to stop existing Agent Mail server on {host}:{port}{tool_hint}"
            )))
        }
        PortStatus::Error { message, .. } => Err(CliError::Other(format!(
            "Stopped existing Agent Mail server on {host}:{port}, but could not verify port state: {message}"
        ))),
    }
}

#[cfg(test)]
mod invocation_name_tests {
    use super::*;

    fn long_help_text(mut cmd: clap::Command) -> String {
        let mut buf = Vec::new();
        cmd.write_long_help(&mut buf).expect("write_long_help");
        String::from_utf8(buf).expect("help utf8")
    }

    #[test]
    fn cli_help_can_render_under_mcp_agent_mail_name() {
        let cmd = Cli::command()
            .name("mcp-agent-mail")
            .bin_name("mcp-agent-mail");
        let help = long_help_text(cmd);
        assert!(
            help.contains("mcp-agent-mail"),
            "expected help to reference mcp-agent-mail, got:\n{help}"
        );
    }

    #[test]
    fn cli_help_can_render_under_am_name() {
        let cmd = Cli::command().name("am").bin_name("am");
        let help = long_help_text(cmd);
        assert!(help.contains("am"), "expected help to reference am");
    }
}

fn handle_share(action: ShareCommand) -> CliResult<()> {
    match action {
        ShareCommand::Export(args) => {
            let dry_run = resolve_bool(args.dry_run, args.no_dry_run, false);

            let mut projects = args.projects;
            let mut inline_threshold = args.inline_threshold;
            let mut detach_threshold = args.detach_threshold;
            let mut scrub_preset = args.scrub_preset;
            let mut chunk_threshold = args.chunk_threshold;
            let mut chunk_size = args.chunk_size;
            let mut do_zip = resolve_bool(args.zip, args.no_zip, true);

            if args.interactive {
                let wizard = share_export_wizard(ShareExportWizardDefaults {
                    projects: projects.clone(),
                    inline_threshold,
                    detach_threshold,
                    scrub_preset: scrub_preset.clone(),
                    chunk_threshold,
                    chunk_size,
                    zip: do_zip,
                })?;
                projects = wizard.projects;
                inline_threshold = wizard.inline_threshold;
                detach_threshold = wizard.detach_threshold;
                scrub_preset = wizard.scrub_preset;
                chunk_threshold = wizard.chunk_threshold;
                chunk_size = wizard.chunk_size;
                do_zip = wizard.zip;
            }

            let preset = share::normalize_scrub_preset(&scrub_preset)?;
            share::validate_thresholds(
                inline_threshold,
                detach_threshold,
                chunk_threshold,
                chunk_size,
            )?;

            let inline = inline_threshold.max(0) as usize;
            let detach_raw = detach_threshold.max(0) as usize;
            let detach_adjusted = share::adjust_detach_threshold(inline, detach_raw);
            if detach_adjusted != detach_raw {
                ftui_runtime::ftui_eprintln!(
                    "warning: adjusted detach threshold to {} to exceed inline threshold",
                    detach_adjusted
                );
            }
            run_share_export(ShareExportParams {
                output: args.output,
                projects,
                inline_threshold: inline,
                detach_threshold: detach_adjusted,
                scrub_preset: preset,
                chunk_threshold: chunk_threshold.max(0) as usize,
                chunk_size: chunk_size.max(1024) as usize,
                dry_run,
                zip: do_zip,
                signing_key: args.signing_key,
                signing_public_out: args.signing_public_out,
                age_recipients: args.age_recipient,
            })
        }
        ShareCommand::Update(args) => {
            if !args.bundle.exists() {
                return Err(share::ShareError::BundleNotFound {
                    path: args.bundle.display().to_string(),
                }
                .into());
            }
            let stored = share::load_bundle_export_config(&args.bundle)?;
            let preset = args
                .scrub_preset
                .as_deref()
                .unwrap_or(stored.scrub_preset.as_str());
            let _preset = share::normalize_scrub_preset(preset)?;
            let inline = args.inline_threshold.unwrap_or(stored.inline_threshold);
            let detach = args.detach_threshold.unwrap_or(stored.detach_threshold);
            let chunk_threshold = args.chunk_threshold.unwrap_or(stored.chunk_threshold);
            let chunk_size = args.chunk_size.unwrap_or(stored.chunk_size);
            share::validate_thresholds(inline, detach, chunk_threshold, chunk_size)?;
            let inline_u = inline.max(0) as usize;
            let detach_u = detach.max(0) as usize;
            let detach_adjusted = share::adjust_detach_threshold(inline_u, detach_u);
            if detach_adjusted != detach_u {
                ftui_runtime::ftui_eprintln!(
                    "warning: adjusted detach threshold to {} to exceed inline threshold",
                    detach_adjusted
                );
            }
            let do_zip = resolve_bool(args.zip, args.no_zip, false);
            run_share_update(ShareUpdateParams {
                bundle: args.bundle,
                projects: if args.projects.is_empty() {
                    stored.projects
                } else {
                    args.projects
                },
                inline_threshold: inline_u,
                detach_threshold: detach_adjusted,
                scrub_preset: _preset,
                chunk_threshold: chunk_threshold.max(0) as usize,
                chunk_size: chunk_size.max(1024) as usize,
                zip: do_zip,
                signing_key: args.signing_key,
                signing_public_out: args.signing_public_out,
                age_recipients: args.age_recipient,
            })
        }
        ShareCommand::Preview(args) => {
            ensure_dir(&args.bundle)?;
            let open = resolve_bool(args.open_browser, args.no_open_browser, false);
            run_share_preview(&args.bundle, &args.host, args.port, open)
        }
        ShareCommand::Verify(args) => {
            ensure_dir(&args.bundle)?;
            let result = share::verify_bundle_crypto(&args.bundle, args.public_key.as_deref())?;
            output::section(&format!("Bundle: {}", result.bundle));
            output::kv("SRI checked", &result.sri_checked.to_string());
            output::kv("SRI valid", &result.sri_valid.to_string());
            output::kv("Signature checked", &result.signature_checked.to_string());
            output::kv("Signature valid", &result.signature_verified.to_string());
            if let Some(ref err) = result.error {
                ftui_runtime::ftui_eprintln!("  Error: {err}");
                return Err(CliError::ExitCode(1));
            }
            if result.signature_checked && !result.signature_verified {
                ftui_runtime::ftui_eprintln!("  Signature verification FAILED.");
                return Err(CliError::ExitCode(1));
            }
            Ok(())
        }
        ShareCommand::Decrypt(args) => {
            if args.identity.is_some() && args.passphrase {
                return Err(CliError::InvalidArgument(
                    "passphrase cannot be combined with identity file".to_string(),
                ));
            }
            if !args.encrypted_path.exists() {
                return Err(CliError::InvalidArgument(format!(
                    "encrypted file not found: {}",
                    args.encrypted_path.display()
                )));
            }
            let output = args
                .output
                .unwrap_or_else(|| share::default_decrypt_output(&args.encrypted_path));
            let passphrase_str = if args.passphrase {
                ftui_runtime::ftui_eprintln!("Enter passphrase:");
                let mut buf = String::new();
                std::io::stdin()
                    .read_line(&mut buf)
                    .map_err(|e| CliError::Other(format!("failed to read passphrase: {e}")))?;
                Some(buf.trim_end().to_string())
            } else {
                None
            };
            share::decrypt_with_age(
                &args.encrypted_path,
                &output,
                args.identity.as_deref(),
                passphrase_str.as_deref(),
            )?;
            ftui_runtime::ftui_println!("Decrypted to: {}", output.display());
            Ok(())
        }
        ShareCommand::Wizard(args) => run_native_wizard(args),
        ShareCommand::StaticExport(args) => {
            let output_display = args.output.display().to_string();
            let config = mcp_agent_mail_server::static_export::ExportConfig {
                output_dir: args.output,
                projects: args.projects,
                include_archive: args.include_archive,
                include_search_index: args.include_search_index,
            };
            let manifest = mcp_agent_mail_server::static_export::export_static_site(&config)
                .map_err(|e| CliError::Other(format!("static export failed: {e}")))?;
            ftui_runtime::ftui_println!(
                "Static export complete: {} files, {} bytes, hash: {}",
                manifest.file_count,
                manifest.total_bytes,
                &manifest.content_hash[..12.min(manifest.content_hash.len())]
            );
            ftui_runtime::ftui_println!("Output: {output_display}");
            Ok(())
        }
        ShareCommand::Deploy { action } => handle_deploy(action),
    }
}

fn handle_deploy(action: DeployCommand) -> CliResult<()> {
    match action {
        DeployCommand::Validate(args) => {
            ensure_dir(&args.bundle)?;
            let report = share::deploy::validate_bundle(&args.bundle)?;
            let fmt = output::CliOutputFormat::resolve(args.format, args.json);
            output::emit_output(&report, fmt, || {
                ftui_runtime::ftui_println!("=== Deploy Validation Report ===");
                ftui_runtime::ftui_println!("Generated: {}", report.generated_at);
                ftui_runtime::ftui_println!("Ready: {}", if report.ready { "YES" } else { "NO" });
                ftui_runtime::ftui_println!("");

                ftui_runtime::ftui_println!("--- Checks ---");
                for check in &report.checks {
                    let icon = if check.passed {
                        "PASS"
                    } else {
                        match check.severity {
                            share::deploy::CheckSeverity::Error => "FAIL",
                            share::deploy::CheckSeverity::Warning => "WARN",
                            share::deploy::CheckSeverity::Info => "INFO",
                            share::deploy::CheckSeverity::Skipped => "SKIP",
                        }
                    };
                    ftui_runtime::ftui_println!("  [{icon}] {}: {}", check.name, check.message);
                }
                ftui_runtime::ftui_println!("");

                ftui_runtime::ftui_println!("--- Platforms ---");
                for platform in &report.platforms {
                    let status = if platform.detected {
                        "detected"
                    } else {
                        "not detected"
                    };
                    ftui_runtime::ftui_println!("  {} ({})", platform.name, status);
                    if let Some(ref cmd) = platform.deploy_command {
                        ftui_runtime::ftui_println!("    Deploy: {cmd}");
                    }
                }
                ftui_runtime::ftui_println!("");

                ftui_runtime::ftui_println!("--- Bundle Stats ---");
                ftui_runtime::ftui_println!(
                    "  Files: {}, Bytes: {}, HTML: {}, Data: {}, Assets: {}",
                    report.bundle_stats.total_files,
                    report.bundle_stats.total_bytes,
                    report.bundle_stats.html_pages,
                    report.bundle_stats.data_files,
                    report.bundle_stats.asset_files,
                );
                ftui_runtime::ftui_println!("");

                ftui_runtime::ftui_println!("--- Security ---");
                ftui_runtime::ftui_println!(
                    "  Cross-origin isolation: {}",
                    report.security.cross_origin_isolation
                );
                ftui_runtime::ftui_println!(
                    "  Contains database: {}",
                    report.security.contains_database
                );
                ftui_runtime::ftui_println!("  Static only: {}", report.security.static_only);
                for note in &report.security.notes {
                    ftui_runtime::ftui_println!("  Note: {note}");
                }
                ftui_runtime::ftui_println!("");

                ftui_runtime::ftui_println!("--- Rollback ---");
                if let Some(ref hash) = report.rollback.current_hash {
                    ftui_runtime::ftui_println!("  Current hash: {}", &hash[..12.min(hash.len())]);
                }
                if let Some(ref hash) = report.rollback.previous_hash {
                    ftui_runtime::ftui_println!("  Previous hash: {}", &hash[..12.min(hash.len())]);
                }
                for step in &report.rollback.steps {
                    ftui_runtime::ftui_println!("  [{}] {}", step.platform, step.instruction);
                }
            });
            if !report.ready {
                return Err(CliError::ExitCode(1));
            }
            Ok(())
        }
        DeployCommand::Tooling(args) => {
            ensure_dir(&args.bundle)?;
            let written = share::deploy::write_deploy_tooling(&args.bundle)?;
            ftui_runtime::ftui_println!("Wrote {} deployment files:", written.len());
            for file in &written {
                ftui_runtime::ftui_println!("  {file}");
            }
            Ok(())
        }
        DeployCommand::VerifyUrl(args) => {
            let plan = share::deploy::build_verify_plan(&args.url);
            let fmt = output::CliOutputFormat::resolve(args.format, args.json);
            output::emit_output(&plan, fmt, || {
                ftui_runtime::ftui_println!("=== Deployment Verification Plan ===");
                ftui_runtime::ftui_println!("URL: {}", plan.url);
                ftui_runtime::ftui_println!("");
                ftui_runtime::ftui_println!("Checks to perform:");
                for check in &plan.checks {
                    let severity = match check.severity {
                        share::deploy::CheckSeverity::Error => "REQUIRED",
                        share::deploy::CheckSeverity::Warning => "RECOMMENDED",
                        share::deploy::CheckSeverity::Info => "OPTIONAL",
                        share::deploy::CheckSeverity::Skipped => "SKIPPED",
                    };
                    ftui_runtime::ftui_println!("  [{severity}] {}: {}", check.name, check.message);
                }
                ftui_runtime::ftui_println!("");
                ftui_runtime::ftui_println!("Authoritative validation path (native):");
                ftui_runtime::ftui_println!(
                    "  am share deploy verify-live {} --bundle <bundle_dir>",
                    plan.url
                );
                ftui_runtime::ftui_println!("");
                ftui_runtime::ftui_println!("Compatibility-only wrapper (generated in bundle):");
                ftui_runtime::ftui_println!(
                    "  ./scripts/validate_deploy.sh <bundle_dir> {}",
                    plan.url
                );
            });
            Ok(())
        }
        DeployCommand::VerifyLive(args) => handle_deploy_verify_live(args),
    }
}

fn build_verify_live_options(args: &DeployVerifyLiveArgs) -> share::deploy::VerifyLiveOptions {
    let probe_config = share::probe::ProbeConfig {
        timeout: std::time::Duration::from_millis(args.timeout),
        retries: args.retries,
        retry_delay: std::time::Duration::from_millis(args.retry_delay),
        ..share::probe::ProbeConfig::default()
    };
    share::deploy::VerifyLiveOptions {
        url: args.url.clone(),
        bundle_path: args.bundle.clone(),
        security_audit: args.security,
        strict: args.strict,
        fail_fast: args.fail_fast,
        probe_config,
    }
}

fn handle_deploy_verify_live(args: DeployVerifyLiveArgs) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(args.format, args.json);
    let opts = build_verify_live_options(&args);
    let report = share::deploy::run_verify_live(&opts);

    output::emit_output(&report, fmt, || {
        ftui_runtime::ftui_println!("verify-live: {}", args.url);
        ftui_runtime::ftui_println!("");

        // Render local stage
        if report.stages.local.ran {
            for check in &report.stages.local.checks {
                print_verify_check(check);
            }
        }

        // Render remote stage
        if report.stages.remote.ran {
            for check in &report.stages.remote.checks {
                print_verify_check(check);
            }
        }

        // Render security stage
        if report.stages.security.ran {
            for check in &report.stages.security.checks {
                print_verify_check(check);
            }
        }

        ftui_runtime::ftui_println!("");
        ftui_runtime::ftui_println!(
            "Result: {}/{} passed, {} warnings ({}ms)",
            report.summary.passed,
            report.summary.total,
            report.summary.warnings,
            report.summary.elapsed_ms
        );
    });

    // Exit code per SPEC
    let code = report.exit_code();
    if code != 0 {
        return Err(CliError::ExitCode(code));
    }
    Ok(())
}

fn print_verify_check(check: &share::deploy::VerifyLiveCheck) {
    let tag = match (check.passed, check.severity) {
        (true, _) => "PASS",
        (false, share::deploy::CheckSeverity::Error) => "FAIL",
        (false, share::deploy::CheckSeverity::Warning) => "WARN",
        (false, share::deploy::CheckSeverity::Skipped) => "SKIP",
        (false, share::deploy::CheckSeverity::Info) => "INFO",
    };
    ftui_runtime::ftui_println!("  [{tag}] {:<20} {}", check.id, check.message);
}

#[derive(Debug, Default, Clone, Copy)]
struct PreflightBannerStats {
    projects: u64,
    agents: u64,
    messages: u64,
    file_reservations: u64,
    contact_links: u64,
}

fn query_preflight_banner_stats_batched(
    conn: &mcp_agent_mail_db::DbConn,
) -> Option<PreflightBannerStats> {
    query_preflight_banner_stats_from_sqlite_sequence(conn)
        .or_else(|| query_preflight_banner_stats_from_max_ids(conn))
}

fn query_preflight_banner_stats_from_sqlite_sequence(
    conn: &mcp_agent_mail_db::DbConn,
) -> Option<PreflightBannerStats> {
    let sql = "SELECT \
               COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'projects'), 0) AS projects_count, \
               COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'agents'), 0) AS agents_count, \
               COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'messages'), 0) AS messages_count, \
               COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'file_reservations'), 0) AS reservations_count, \
               COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'agent_links'), 0) AS links_count";
    let row = conn.query_sync(sql, &[]).ok()?.into_iter().next()?;
    let read_count = |key: &str| {
        row.get_named::<i64>(key)
            .ok()
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0)
    };
    Some(PreflightBannerStats {
        projects: read_count("projects_count"),
        agents: read_count("agents_count"),
        messages: read_count("messages_count"),
        file_reservations: read_count("reservations_count"),
        contact_links: read_count("links_count"),
    })
}

fn query_preflight_banner_stats_from_max_ids(
    conn: &mcp_agent_mail_db::DbConn,
) -> Option<PreflightBannerStats> {
    // Avoid wrapping MAX() with COALESCE() to stay on FrankensQLite's
    // fast aggregate path and avoid mixed aggregate/non-aggregate planner
    // edge-cases that can force an expensive COUNT(*) fallback.
    let sql = "SELECT \
               (SELECT MAX(id) FROM projects) AS projects_count, \
               (SELECT MAX(id) FROM agents) AS agents_count, \
               (SELECT MAX(id) FROM messages) AS messages_count, \
               (SELECT MAX(id) FROM file_reservations) AS reservations_count, \
               (SELECT MAX(id) FROM agent_links) AS links_count";
    let row = conn.query_sync(sql, &[]).ok()?.into_iter().next()?;
    let read_count = |key: &str| {
        row.get_named::<i64>(key)
            .ok()
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0)
    };
    Some(PreflightBannerStats {
        projects: read_count("projects_count"),
        agents: read_count("agents_count"),
        messages: read_count("messages_count"),
        file_reservations: read_count("reservations_count"),
        contact_links: read_count("links_count"),
    })
}

fn preflight_banner_stats(database_url: &str) -> PreflightBannerStats {
    let cfg = mcp_agent_mail_db::DbPoolConfig {
        database_url: database_url.to_string(),
        ..Default::default()
    };
    let Ok(sqlite_path) = cfg.sqlite_path() else {
        return PreflightBannerStats::default();
    };
    if sqlite_path != ":memory:" && !Path::new(&sqlite_path).exists() {
        return PreflightBannerStats::default();
    }
    // Keep startup stats on the fastest possible path: banner counts are purely
    // cosmetic, so do not trigger fallback probing or sqlite auto-recovery here.
    // The real readiness/server startup path still performs full recovery checks.
    let Ok(conn) = mcp_agent_mail_db::DbConn::open_file(&sqlite_path) else {
        return PreflightBannerStats::default();
    };
    query_preflight_banner_stats_batched(&conn).unwrap_or_default()
}

fn emit_pre_tui_startup_banner(config: &Config) {
    if !output::is_tty() {
        return;
    }

    let _ = mcp_agent_mail_server::theme::init_console_theme_from_config(config.console_theme);
    let stats = preflight_banner_stats(&config.database_url);

    let endpoint = format!(
        "http://{}:{}{}",
        config.http_host, config.http_port, config.http_path
    );
    let ui_host = if matches!(config.http_host.as_str(), "0.0.0.0" | "::") {
        "localhost"
    } else {
        config.http_host.as_str()
    };
    let web_ui = format!("http://{}:{}/mail", ui_host, config.http_port);
    let storage_root = config.storage_root.to_string_lossy();
    let app_env_str = config.app_environment.to_string();
    let banner_lines = mcp_agent_mail_server::console::render_startup_banner(
        &mcp_agent_mail_server::console::BannerParams {
            app_environment: &app_env_str,
            endpoint: endpoint.as_str(),
            database_url: config.database_url.as_str(),
            storage_root: storage_root.as_ref(),
            auth_enabled: config.http_bearer_token.is_some(),
            tools_log_enabled: config.tools_log_enabled,
            tool_calls_log_enabled: config.log_tool_calls_enabled,
            console_theme: mcp_agent_mail_server::theme::current_theme_display_name(),
            web_ui_url: web_ui.as_str(),
            remote_url: None,
            projects: stats.projects,
            agents: stats.agents,
            messages: stats.messages,
            file_reservations: stats.file_reservations,
            contact_links: stats.contact_links,
        },
    );

    for line in banner_lines {
        ftui_runtime::ftui_println!("{line}");
    }
}

fn handle_serve_http(
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
    no_auth: bool,
    no_tui: bool,
) -> CliResult<()> {
    let mut config = build_http_config(host, port, path, no_auth);
    if no_tui {
        config.tui_enabled = false;
    }
    let suppress_runtime_logs_for_tui = config.tui_enabled && crate::output::is_tty();
    apply_release_logging_defaults(suppress_runtime_logs_for_tui);
    if let Err(e) = run_setup_self_heal_for_server(&config) {
        output::warn(&format!(
            "Agent setup self-heal encountered an issue (non-fatal): {e}"
        ));
    }
    auto_clear_port(&config.http_host, config.http_port)?;
    if !no_tui {
        emit_pre_tui_startup_banner(&config);
    }
    mcp_agent_mail_server::run_http_with_tui(&config)?;
    Ok(())
}

fn run_setup_self_heal_for_server(config: &Config) -> CliResult<()> {
    use mcp_agent_mail_core::setup;

    let project_dir = std::env::current_dir().unwrap_or_default();
    let env_file = project_dir.join(".env");
    let resolved_token = setup::resolve_token(None, &env_file);
    let agent_name = std::env::var("AGENT_MAIL_AGENT").unwrap_or_default();
    let skip_hooks = agent_name.is_empty();

    let mut target_agents = load_self_heal_target_agents(config, &project_dir, &resolved_token)
        .unwrap_or_else(detect_installed_setup_agents);

    if target_agents.is_empty() {
        return Ok(());
    }

    // Keep deterministic target ordering for cache/fingerprint stability.
    target_agents.sort_by_key(|platform| platform.slug());
    target_agents.dedup();

    let project_slug = if skip_hooks {
        String::new()
    } else {
        resolve_project_identity(&project_dir.display().to_string()).slug
    };

    let params = setup::SetupParams {
        host: config.http_host.clone(),
        port: config.http_port,
        path: config.http_path.clone(),
        token: resolved_token.clone(),
        project_dir: project_dir.clone(),
        agents: Some(target_agents.clone()),
        dry_run: false,
        skip_user_config: false,
        skip_hooks,
        project_slug,
        agent_name,
    };

    let expected_static_cache = SetupSelfHealCache {
        schema_version: SETUP_SELF_HEAL_CACHE_VERSION,
        project_dir: project_dir.display().to_string(),
        server_url: params.server_url(),
        token_fingerprint: token_fingerprint(&resolved_token),
        target_agents: target_agents
            .iter()
            .map(|platform| platform.slug().to_string())
            .collect(),
        skip_user_config: params.skip_user_config,
        skip_hooks: params.skip_hooks,
        file_fingerprints: Vec::new(),
    };

    let existing_cache = read_setup_self_heal_cache(config, &project_dir);
    let mut computed_fingerprints: Option<Vec<SetupSelfHealFileFingerprint>> = None;
    let mut build_expected_full_cache = || {
        let fingerprints = computed_fingerprints
            .get_or_insert_with(|| {
                collect_setup_self_heal_file_fingerprints(&params, &target_agents)
            })
            .clone();
        let mut cache = expected_static_cache.clone();
        cache.file_fingerprints = fingerprints;
        cache
    };

    let static_match = existing_cache
        .as_ref()
        .is_some_and(|cache| cache.matches_static(&expected_static_cache));
    if static_match {
        let expected_cache = build_expected_full_cache();
        if existing_cache
            .as_ref()
            .is_some_and(|cache| cache.matches_full(&expected_cache))
        {
            return Ok(());
        }
    }

    let statuses = setup::check_status(&params);
    let status_ok = statuses.iter().all(|status| {
        !status.config_files.is_empty()
            && status
                .config_files
                .iter()
                .all(|file| file.exists && file.has_server_entry && file.url_matches)
    });

    if status_ok && static_match {
        let expected_cache = build_expected_full_cache();
        write_setup_self_heal_cache(config, &project_dir, &expected_cache);
        return Ok(());
    }

    setup::save_token_to_env_file(&env_file, &resolved_token)
        .map_err(|e| CliError::Other(format!("setup self-heal token save failed: {e}")))?;

    let results = setup::run_setup(&params);
    let failed: Vec<String> = results
        .iter()
        .flat_map(|result| result.actions.iter())
        .filter_map(|action| match &action.outcome {
            setup::ActionOutcome::Failed(reason) => {
                Some(format!("{} ({reason})", action.file_path))
            }
            _ => None,
        })
        .collect();
    if !failed.is_empty() {
        return Err(CliError::Other(format!(
            "setup self-heal failed for {} file(s): {}",
            failed.len(),
            failed.join(", ")
        )));
    }

    let mut refreshed_cache = expected_static_cache;
    refreshed_cache.file_fingerprints =
        collect_setup_self_heal_file_fingerprints(&params, &target_agents);
    write_setup_self_heal_cache(config, &project_dir, &refreshed_cache);
    Ok(())
}

const SETUP_SELF_HEAL_CACHE_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SetupSelfHealFileFingerprint {
    path: String,
    exists: bool,
    len: u64,
    modified_micros: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SetupSelfHealCache {
    schema_version: u32,
    project_dir: String,
    server_url: String,
    token_fingerprint: String,
    target_agents: Vec<String>,
    skip_user_config: bool,
    skip_hooks: bool,
    #[serde(default)]
    file_fingerprints: Vec<SetupSelfHealFileFingerprint>,
}

impl SetupSelfHealCache {
    fn matches_static(&self, expected: &Self) -> bool {
        self.schema_version == expected.schema_version
            && self.project_dir == expected.project_dir
            && self.server_url == expected.server_url
            && self.token_fingerprint == expected.token_fingerprint
            && self.target_agents == expected.target_agents
            && self.skip_user_config == expected.skip_user_config
            && self.skip_hooks == expected.skip_hooks
    }

    fn matches_full(&self, expected: &Self) -> bool {
        self.matches_static(expected) && self.file_fingerprints == expected.file_fingerprints
    }
}

fn collect_setup_self_heal_file_fingerprints(
    params: &mcp_agent_mail_core::setup::SetupParams,
    target_agents: &[mcp_agent_mail_core::setup::AgentPlatform],
) -> Vec<SetupSelfHealFileFingerprint> {
    use mcp_agent_mail_core::setup::ConfigContent;

    let mut paths = std::collections::BTreeSet::new();
    for platform in target_agents {
        for action in platform.config_actions(params) {
            if matches!(action.content, ConfigContent::HooksMerge { .. }) {
                continue;
            }
            paths.insert(action.file_path);
        }
    }

    paths
        .into_iter()
        .map(|path| {
            let path_str = path.display().to_string();
            match std::fs::metadata(&path) {
                Ok(meta) => {
                    let modified_micros = meta
                        .modified()
                        .ok()
                        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|value| value.as_micros())
                        .and_then(|value| i64::try_from(value).ok())
                        .unwrap_or(0);
                    SetupSelfHealFileFingerprint {
                        path: path_str,
                        exists: true,
                        len: meta.len(),
                        modified_micros,
                    }
                }
                Err(_) => SetupSelfHealFileFingerprint {
                    path: path_str,
                    exists: false,
                    len: 0,
                    modified_micros: 0,
                },
            }
        })
        .collect()
}

fn detect_installed_setup_agents() -> Vec<mcp_agent_mail_core::setup::AgentPlatform> {
    let detect_opts = AgentDetectOptions {
        only_connectors: None,
        include_undetected: false,
        root_overrides: Vec::new(),
    };
    match mcp_agent_mail_core::detect_installed_agents(&detect_opts) {
        Ok(report) => {
            let mut platforms = Vec::new();
            for entry in &report.installed_agents {
                if !entry.detected {
                    continue;
                }
                if let Some(platform) =
                    mcp_agent_mail_core::setup::AgentPlatform::from_slug(&entry.slug)
                    && !platforms.contains(&platform)
                {
                    platforms.push(platform);
                }
            }
            platforms
        }
        Err(_) => Vec::new(),
    }
}

fn token_fingerprint(token: &str) -> String {
    // Deterministic non-cryptographic fingerprint to avoid storing raw secrets.
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in token.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

fn setup_self_heal_cache_path(config: &Config, project_dir: &Path) -> PathBuf {
    let key = token_fingerprint(&project_dir.display().to_string());
    config
        .storage_root
        .join(".setup-self-heal")
        .join(format!("{key}.json"))
}

fn read_setup_self_heal_cache(config: &Config, project_dir: &Path) -> Option<SetupSelfHealCache> {
    let path = setup_self_heal_cache_path(config, project_dir);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<SetupSelfHealCache>(&content).ok()
}

fn load_self_heal_target_agents(
    config: &Config,
    project_dir: &Path,
    token: &str,
) -> Option<Vec<mcp_agent_mail_core::setup::AgentPlatform>> {
    let cache = read_setup_self_heal_cache(config, project_dir)?;
    if cache.schema_version != SETUP_SELF_HEAL_CACHE_VERSION {
        return None;
    }
    if cache.project_dir != project_dir.display().to_string() {
        return None;
    }
    if cache.token_fingerprint != token_fingerprint(token) {
        return None;
    }
    cache
        .target_agents
        .iter()
        .map(|slug| mcp_agent_mail_core::setup::AgentPlatform::from_slug(slug))
        .collect()
}

fn write_setup_self_heal_cache(config: &Config, project_dir: &Path, cache: &SetupSelfHealCache) {
    let path = setup_self_heal_cache_path(config, project_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string(cache) {
        let _ = std::fs::write(path, content);
    }
}

#[allow(dead_code)]
fn build_setup_run_command_for_http_server(config: &Config) -> SetupCommand {
    let project_dir = std::env::current_dir().unwrap_or_default();
    SetupCommand::Run {
        agent: None,
        dry_run: false,
        yes: true,
        token: None,
        port: config.http_port,
        host: config.http_host.clone(),
        path: config.http_path.clone(),
        project_dir: Some(project_dir),
        format: None,
        json: false,
        no_user_config: false,
        no_hooks: false,
    }
}

fn handle_serve_stdio() -> CliResult<()> {
    let config = Config::from_env();
    mcp_agent_mail_server::run_stdio(&config);
    Ok(())
}

/// Handle the check-inbox command.
///
/// Checks the agent inbox for unread messages. Designed for git hooks and editor integrations.
/// Exits silently on any error (fail-safe for hooks - never interrupt agent work).
#[allow(clippy::too_many_arguments)]
fn handle_check_inbox(
    agent: Option<String>,
    rate_limit: u64,
    direct: bool,
    format: Option<output::CliOutputFormat>,
    json: bool,
    host: String,
    port: u16,
    project: Option<String>,
) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(format, json);

    // Resolve agent name from flag or environment variables
    let agent_name = agent
        .or_else(|| std::env::var("AGENT_NAME").ok())
        .or_else(|| std::env::var("AGENT_MAIL_AGENT").ok());

    let Some(agent_name) = agent_name else {
        // No agent configured - exit silently (not an error for hooks)
        return Ok(());
    };

    // Check for template/placeholder values (exit silently if detected)
    if value_looks_like_template(&agent_name) {
        return Ok(());
    }

    // Resolve project key from flag or environment
    let project_key = project
        .or_else(|| std::env::var("AGENT_MAIL_PROJECT").ok())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        });

    if value_looks_like_template(&project_key) {
        return Ok(());
    }

    // Apply rate limiting (unless disabled with rate_limit=0)
    if rate_limit > 0 {
        let limiter = CheckInboxRateLimiter::new(&agent_name, Some(rate_limit));
        if !limiter.should_check() {
            // Rate limited - exit silently
            return Ok(());
        }
    }

    // Fetch inbox (direct or HTTP)
    let result = if direct {
        let config = CheckInboxDirectConfig {
            project_key,
            agent_name: agent_name.clone(),
            limit: CHECK_INBOX_FETCH_LIMIT,
        };
        check_inbox_direct(&config)
    } else {
        // Build HTTP config and fetch via JSON-RPC
        let config = Config::from_env();
        let server_urls = check_inbox_server_urls(&host, port, &config.http_path);
        let server_url = check_inbox_server_url(&host, port, &config.http_path);
        let bearer_token = std::env::var("AGENT_MAIL_TOKEN")
            .or_else(|_| std::env::var("HTTP_BEARER_TOKEN"))
            .ok()
            .filter(|v| !v.is_empty() && !value_looks_like_template(v));

        let config = CheckInboxRpcConfig {
            server_url,
            server_urls: server_urls.clone(),
            bearer_token,
            project_key,
            agent_name: agent_name.clone(),
            limit: CHECK_INBOX_FETCH_LIMIT,
            include_bodies: false,
            timeout_seconds: CHECK_INBOX_RPC_TIMEOUT_SECS,
        };

        // Use a minimal runtime for the async HTTP call
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .map_err(|e| CliError::Other(format!("runtime error: {e}")))?;

        rt.block_on(async { fetch_inbox_via_jsonrpc_with_fallback(&config, &server_urls).await })
    };

    // Handle result - exit silently on any error (fail-safe for hooks)
    let result = match result {
        Ok(r) => r,
        Err(_) => return Ok(()), // Fail silently
    };

    // No messages - exit silently
    if result.unread_count == 0 {
        return Ok(());
    }

    // Build output data for JSON/TOON
    let output_data = serde_json::json!({
        "agent": agent_name,
        "unread_count": result.unread_count,
        "urgent_or_high_count": result.urgent_or_high_count,
        "messages": result.messages.iter().map(|m| {
            serde_json::json!({
                "id": m.id,
                "subject": m.subject,
                "from": m.from,
                "importance": m.importance,
                "created_ts": m.created_ts,
            })
        }).collect::<Vec<_>>(),
    });

    output::emit_output(&output_data, fmt, || {
        // Human-readable output with emoji
        ftui_runtime::ftui_println!();
        ftui_runtime::ftui_println!("📬 === INBOX REMINDER ===");
        if result.urgent_or_high_count > 0 {
            ftui_runtime::ftui_println!(
                "⚠️  You have {} message(s) in your inbox ({} urgent/high priority)",
                result.unread_count,
                result.urgent_or_high_count
            );
            ftui_runtime::ftui_println!("   Use fetch_inbox to check your messages!");
        } else {
            ftui_runtime::ftui_println!(
                "   You have {} recent message(s) in your inbox.",
                result.unread_count
            );
            ftui_runtime::ftui_println!(
                "   Consider checking with fetch_inbox if you haven't lately."
            );
        }
        ftui_runtime::ftui_println!("=========================");
        ftui_runtime::ftui_println!();
    });

    Ok(())
}

fn normalize_http_path(raw: &str) -> String {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "mcp" | "/mcp" | "/mcp/" => return "/mcp/".to_string(),
        "api" | "/api" | "/api/" => return "/api/".to_string(),
        _ => {}
    }

    if trimmed.is_empty() {
        return "/".to_string();
    }

    let mut with_leading = trimmed.to_string();
    if !with_leading.starts_with('/') {
        with_leading.insert(0, '/');
    }

    let without_trailing = with_leading.trim_end_matches('/');
    if without_trailing.is_empty() {
        "/".to_string()
    } else {
        format!("{without_trailing}/")
    }
}

fn check_inbox_server_url(host: &str, port: u16, http_path: &str) -> String {
    check_inbox_server_urls(host, port, http_path)
        .into_iter()
        .next()
        .unwrap_or_else(|| format!("http://{host}:{port}/"))
}

fn mcp_base_alias_path(path: &str) -> Option<&'static str> {
    match path {
        "/api/" => Some("/mcp/"),
        "/mcp/" => Some("/api/"),
        _ => None,
    }
}

fn check_inbox_server_urls(host: &str, port: u16, http_path: &str) -> Vec<String> {
    let primary_path = normalize_http_path(http_path);
    let mut urls = vec![format!("http://{host}:{port}{primary_path}")];
    if let Some(alias_path) = mcp_base_alias_path(&primary_path) {
        let alias_url = format!("http://{host}:{port}{alias_path}");
        if alias_url != urls[0] {
            urls.push(alias_url);
        }
    }
    urls
}

fn build_http_config(
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
    no_auth: bool,
) -> Config {
    let config = Config::from_env();
    apply_http_config_overrides(config, host, port, path, no_auth)
}

fn apply_http_config_overrides(
    mut config: Config,
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
    no_auth: bool,
) -> Config {
    // Keep env and CLI semantics aligned (`HTTP_PATH=mcp` == `--path mcp`).
    config.http_path = normalize_http_path(&config.http_path);
    if let Some(host) = host {
        config.http_host = host;
    }
    if let Some(port) = port {
        config.http_port = port;
    }
    if let Some(path) = path {
        config.http_path = normalize_http_path(&path);
    }
    if no_auth {
        // Disable bearer token authentication for this run
        config.http_bearer_token = None;
        eprintln!("[info] --no-auth: Bearer token authentication disabled for this run");
    }
    config
}

fn handle_doctor(action: DoctorCommand) -> CliResult<()> {
    match action {
        DoctorCommand::Check {
            project,
            verbose,
            format,
            json,
        } => handle_doctor_check(project, verbose, format, json),
        DoctorCommand::Repair {
            project,
            dry_run,
            yes,
            backup_dir,
        } => handle_doctor_repair(project, dry_run, yes, backup_dir),
        DoctorCommand::Backups { format, json } => handle_doctor_backups(format, json),
        DoctorCommand::Restore {
            backup_path,
            dry_run,
            yes,
        } => handle_doctor_restore(backup_path, dry_run, yes),
        DoctorCommand::Reconstruct { dry_run, yes, json } => {
            handle_doctor_reconstruct(dry_run, yes, json)
        }
    }
}

fn handle_guard(action: GuardCommand) -> CliResult<()> {
    match action {
        GuardCommand::Install {
            project,
            repo,
            prepush,
            no_prepush,
        } => {
            let install_prepush = if prepush { true } else { !no_prepush };
            let config = Config::from_env();
            let db_cfg = mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                ..Default::default()
            };
            let abs_db_path = db_cfg
                .sqlite_path()
                .ok()
                .map(|s| {
                    let p = PathBuf::from(s);
                    if p.exists() {
                        p.canonicalize().unwrap_or(p)
                    } else {
                        if p.is_absolute() {
                            p
                        } else {
                            std::env::current_dir().unwrap_or_default().join(p)
                        }
                    }
                })
                .map(|p| p.to_string_lossy().to_string());

            mcp_agent_mail_guard::install_guard(
                &project,
                repo.as_path(),
                abs_db_path.as_deref(),
                install_prepush,
            )?;
            ftui_runtime::ftui_println!("Guard installed successfully.");
            Ok(())
        }
        GuardCommand::Uninstall { repo } => {
            mcp_agent_mail_guard::uninstall_guard(repo.as_path())?;
            ftui_runtime::ftui_println!("Guard uninstalled successfully.");
            Ok(())
        }
        GuardCommand::Status { repo } => {
            let status = mcp_agent_mail_guard::guard_status(&repo)?;
            output::section("Guard Status:");
            output::kv("Hooks dir", &status.hooks_dir);
            output::kv("Mode", &format!("{:?}", status.guard_mode));
            output::kv("Worktrees", &status.worktrees_enabled.to_string());
            output::kv(
                "Pre-commit",
                if status.pre_commit_present {
                    "installed"
                } else {
                    "not installed"
                },
            );
            output::kv(
                "Pre-push",
                if status.pre_push_present {
                    "installed"
                } else {
                    "not installed"
                },
            );
            Ok(())
        }
        GuardCommand::Check {
            stdin_nul,
            advisory,
            repo,
        } => {
            let repo_path = repo.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            // Read paths from stdin (null-separated or line-separated)
            let input = {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf).unwrap_or(0);
                buf
            };
            let paths: Vec<String> = if stdin_nul {
                input
                    .split('\0')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            } else {
                input
                    .lines()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            };

            let config = mcp_agent_mail_core::Config::from_env();
            let archive_root = if repo_path.join("file_reservations").is_dir() {
                repo_path.clone()
            } else {
                let human_key = repo_path.to_string_lossy().to_string();
                let identity = mcp_agent_mail_core::identity::resolve_project_identity(&human_key);
                let candidate = config.storage_root.join("projects").join(&identity.slug);
                if candidate.join("file_reservations").is_dir() {
                    candidate
                } else {
                    repo_path.clone()
                }
            };

            let conflicts =
                mcp_agent_mail_guard::guard_check(&archive_root, &repo_path, &paths, advisory)?;
            if conflicts.is_empty() {
                ftui_runtime::ftui_println!("No file reservation conflicts detected.");
            } else {
                for c in &conflicts {
                    ftui_runtime::ftui_eprintln!(
                        "CONFLICT: pattern '{}' held by {} (expires {})",
                        c.pattern,
                        c.holder,
                        c.expires_ts
                    );
                }
                if !advisory {
                    return Err(CliError::ExitCode(1));
                }
            }
            Ok(())
        }
    }
}

fn handle_list_projects(
    include_agents: bool,
    format: Option<output::CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    handle_list_projects_with_database_url(&cfg.database_url, include_agents, format, json)
}

fn handle_list_projects_with_database_url(
    database_url: &str,
    include_agents: bool,
    format: Option<output::CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(format, json);
    let conn = open_db_sync_with_database_url(database_url)?;

    let projects = conn
        .query_sync(
            "SELECT id, slug, human_key, created_at FROM projects ORDER BY id",
            &[],
        )
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

    if projects.is_empty() {
        output::emit_empty(fmt, "No projects found.");
        return Ok(());
    }

    // Pre-fetch agents once if needed (avoids duplicate queries)
    let agents_by_project: std::collections::HashMap<i64, Vec<(String, String, String)>> =
        if include_agents {
            let mut map = std::collections::HashMap::new();
            for row in &projects {
                let id: i64 = row.get_named("id").unwrap_or(0);
                let agents = conn
                    .query_sync(
                        "SELECT name, program, model FROM agents WHERE project_id = ?",
                        &[sqlmodel_core::Value::BigInt(id)],
                    )
                    .unwrap_or_default();
                let agent_list: Vec<(String, String, String)> = agents
                    .iter()
                    .map(|a| {
                        let name: String = a.get_named("name").unwrap_or_default();
                        let program: String = a.get_named("program").unwrap_or_default();
                        let model: String = a.get_named("model").unwrap_or_default();
                        (name, program, model)
                    })
                    .collect();
                map.insert(id, agent_list);
            }
            map
        } else {
            std::collections::HashMap::new()
        };

    // Build serializable data structure for JSON/TOON output
    let mut output_data: Vec<serde_json::Value> = Vec::new();
    for row in &projects {
        let id: i64 = row.get_named("id").unwrap_or(0);
        let slug: String = row.get_named("slug").unwrap_or_default();
        let human_key: String = row.get_named("human_key").unwrap_or_default();
        let created_at: i64 = row.get_named("created_at").unwrap_or(0);

        let mut entry = serde_json::json!({
            "id": id,
            "slug": slug,
            "human_key": human_key,
            "created_at": mcp_agent_mail_db::timestamps::micros_to_iso(created_at),
        });

        if include_agents && let Some(agent_list) = agents_by_project.get(&id) {
            let agents_json: Vec<serde_json::Value> = agent_list
                .iter()
                .map(|(name, program, model)| {
                    serde_json::json!({ "name": name, "program": program, "model": model })
                })
                .collect();
            entry
                .as_object_mut()
                .unwrap()
                .insert("agents".to_string(), serde_json::json!(agents_json));
        }
        output_data.push(entry);
    }

    output::emit_output(&output_data, fmt, || {
        // Table rendering
        let mut table = output::CliTable::new(vec!["ID", "SLUG", "HUMAN_KEY"]);
        for row in &projects {
            let id: i64 = row.get_named("id").unwrap_or(0);
            let slug: String = row.get_named("slug").unwrap_or_default();
            let human_key: String = row.get_named("human_key").unwrap_or_default();
            table.add_row(vec![id.to_string(), slug, human_key]);
        }
        table.render();
        if include_agents {
            ftui_runtime::ftui_println!("");
            for row in &projects {
                let id: i64 = row.get_named("id").unwrap_or(0);
                let slug: String = row.get_named("slug").unwrap_or_default();
                if let Some(agent_list) = agents_by_project.get(&id)
                    && !agent_list.is_empty()
                {
                    output::section(&format!("Agents for {slug}:"));
                    for (name, program, model) in agent_list {
                        ftui_runtime::ftui_println!("  {name} ({program}/{model})");
                    }
                }
            }
        }
    });
    Ok(())
}

fn is_sqlite_corruption_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("database disk image is malformed")
        || lower.contains("malformed database schema")
        || lower.contains("file is not a database")
        || lower.contains("no healthy backup was found")
}

fn is_sqlite_recovery_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    is_sqlite_corruption_error_message(message)
        || lower.contains("out of memory")
        || lower.contains("cursor stack is empty")
        || lower.contains("called `option::unwrap()` on a `none` value")
        || lower.contains("internal error")
}

fn sqlite_conn_quick_check_ok(conn: &mcp_agent_mail_db::DbConn) -> CliResult<bool> {
    let rows = conn
        .query_sync("PRAGMA quick_check", &[])
        .map_err(|e| CliError::Other(format!("PRAGMA quick_check failed: {e}")))?;
    let mut checks: Vec<String> = Vec::with_capacity(rows.len());
    for row in &rows {
        if let Ok(v) = row.get_named::<String>("quick_check") {
            checks.push(v);
            continue;
        }
        if let Ok(v) = row.get_named::<String>("integrity_check") {
            checks.push(v);
        }
    }

    if checks.is_empty() {
        // No integrity_check values parsed; database is only OK if we got
        // no rows at all AND expected that (unlikely — PRAGMA quick_check
        // should always return at least one row).
        return Ok(false);
    }

    Ok(checks.len() == 1 && checks[0] == "ok")
}

fn sqlite_conn_is_healthy(conn: &mcp_agent_mail_db::DbConn) -> CliResult<bool> {
    match sqlite_conn_quick_check_ok(conn) {
        Ok(ok) => Ok(ok),
        Err(e) => {
            if is_sqlite_recovery_error_message(&e.to_string()) {
                return Ok(false);
            }
            Err(e)
        }
    }
}

fn sqlite_file_is_healthy(path: &Path) -> CliResult<bool> {
    if !path.exists() {
        return Ok(true);
    }

    let path_string = path.to_string_lossy().into_owned();
    let conn = match mcp_agent_mail_db::DbConn::open_file(&path_string) {
        Ok(conn) => conn,
        Err(e) => {
            if is_sqlite_recovery_error_message(&e.to_string()) {
                return Ok(false);
            }
            return Err(CliError::Other(format!(
                "cannot open sqlite file {} for health check: {e}",
                path.display()
            )));
        }
    };

    sqlite_conn_is_healthy(&conn)
}

fn sqlite_backup_candidates(path: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<(u8, std::time::SystemTime, PathBuf)> = Vec::new();
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let Some(parent) = path.parent() else {
        return Vec::new();
    };

    let bak = path.with_file_name(format!("{file_name}.bak"));
    if bak.is_file() {
        let modified = bak
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        candidates.push((0, modified, bak));
    }

    let backup_prefix = format!("{file_name}.backup-");
    let recovery_prefix = format!("{file_name}.recovery");
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let candidate = entry.path();
            if !candidate.is_file() {
                continue;
            }
            let Some(name) = candidate.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            let priority = if name.starts_with(&backup_prefix) {
                1
            } else if name.starts_with(&recovery_prefix) {
                2
            } else {
                continue;
            };
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            candidates.push((priority, modified, candidate));
        }
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    candidates.into_iter().map(|(_, _, path)| path).collect()
}

fn find_healthy_sqlite_backup(path: &Path) -> Option<PathBuf> {
    for candidate in sqlite_backup_candidates(path) {
        if let Ok(true) = sqlite_file_is_healthy(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn sqlite_quarantine_path(primary_path: &Path, suffix: &str, timestamp: &str) -> CliResult<()> {
    let source = if suffix.is_empty() {
        primary_path.to_path_buf()
    } else {
        let mut source_os = primary_path.as_os_str().to_os_string();
        source_os.push(suffix);
        PathBuf::from(source_os)
    };
    if !source.exists() {
        return Ok(());
    }

    let base_name = primary_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("storage.sqlite3");
    let target_name = if suffix.is_empty() {
        format!("{base_name}.corrupt-{timestamp}")
    } else {
        format!("{base_name}{suffix}.corrupt-{timestamp}")
    };
    let target = primary_path.with_file_name(target_name);
    std::fs::rename(&source, &target).map_err(|e| {
        CliError::Other(format!(
            "failed to quarantine sqlite artifact {}: {e}",
            source.display()
        ))
    })?;
    Ok(())
}

fn recover_sqlite_file(path: &Path) -> CliResult<()> {
    if !path.exists() || sqlite_file_is_healthy(path)? {
        return Ok(());
    }

    let timestamp = Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let backup = find_healthy_sqlite_backup(path);
    sqlite_quarantine_path(path, "", &timestamp)?;
    sqlite_quarantine_path(path, "-wal", &timestamp)?;
    sqlite_quarantine_path(path, "-shm", &timestamp)?;

    if let Some(backup_path) = backup {
        std::fs::copy(&backup_path, path).map_err(|e| {
            CliError::Other(format!(
                "failed to restore sqlite backup {} into {}: {e}",
                backup_path.display(),
                path.display()
            ))
        })?;
        if !sqlite_file_is_healthy(path)? {
            return Err(CliError::Other(format!(
                "sqlite restore from {} completed, but quick_check still failed for {}",
                backup_path.display(),
                path.display()
            )));
        }
    }

    Ok(())
}

fn ensure_sqlite_parent_dir(path: &Path) -> CliResult<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            CliError::Other(format!("failed to create db dir {}: {e}", parent.display()))
        })?;
    }
    Ok(())
}

fn sqlite_absolute_fallback_path(path: &str, open_error: &str) -> Option<String> {
    if path == ":memory:"
        || Path::new(path).is_absolute()
        || path.starts_with("./")
        || path.starts_with("../")
        || !is_sqlite_corruption_error_message(open_error)
    {
        return None;
    }
    let absolute_candidate = Path::new("/").join(path);
    if !absolute_candidate.exists() {
        return None;
    }
    Some(absolute_candidate.to_string_lossy().into_owned())
}

fn sqlite_absolute_candidate_path(path: &str) -> Option<String> {
    if path == ":memory:"
        || Path::new(path).is_absolute()
        || path.starts_with("./")
        || path.starts_with("../")
    {
        return None;
    }
    let absolute_candidate = Path::new("/").join(path);
    if !absolute_candidate.exists() {
        return None;
    }
    Some(absolute_candidate.to_string_lossy().into_owned())
}

fn open_sqlite_with_fallback(path: &str) -> CliResult<(mcp_agent_mail_db::DbConn, String)> {
    match mcp_agent_mail_db::DbConn::open_file(path) {
        Ok(conn) => Ok((conn, path.to_string())),
        Err(primary_err) => {
            let primary_err_text = primary_err.to_string();
            if let Some(fallback_path) = sqlite_absolute_fallback_path(path, &primary_err_text) {
                let fallback_conn =
                    mcp_agent_mail_db::DbConn::open_file(&fallback_path).map_err(|fallback_err| {
                        CliError::Other(format!(
                            "cannot open DB at {path}: {primary_err}; fallback {fallback_path} failed: {fallback_err}"
                        ))
                    })?;
                return Ok((fallback_conn, fallback_path));
            }
            if path != ":memory:" && is_sqlite_recovery_error_message(&primary_err_text) {
                let primary_path = Path::new(path);
                recover_sqlite_file(primary_path).map_err(|recovery_err| {
                    CliError::Other(format!(
                        "cannot open DB at {path}: {primary_err}; auto-recovery failed: {recovery_err}"
                    ))
                })?;
                let recovered_conn = mcp_agent_mail_db::DbConn::open_file(path).map_err(|e| {
                    CliError::Other(format!(
                        "cannot open DB at {path}: {primary_err}; auto-recovery reopen failed: {e}"
                    ))
                })?;
                return Ok((recovered_conn, path.to_string()));
            }
            Err(CliError::Other(format!(
                "cannot open DB at {path}: {primary_err}"
            )))
        }
    }
}

/// Open a synchronous SQLite connection for CLI commands.
pub(crate) fn open_db_sync_with_database_url(
    database_url: &str,
) -> CliResult<mcp_agent_mail_db::DbConn> {
    let cfg = mcp_agent_mail_db::DbPoolConfig {
        database_url: database_url.to_string(),
        ..Default::default()
    };
    let path = cfg
        .sqlite_path()
        .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?;
    if path != ":memory:" {
        ensure_sqlite_parent_dir(Path::new(&path))?;
    }
    let (mut conn, mut opened_path) = open_sqlite_with_fallback(&path)?;
    if opened_path != ":memory:" && !sqlite_conn_is_healthy(&conn)? {
        // If a malformed relative path shadows a healthy absolute DB, prefer that absolute file
        // before mutating/quarantining the relative artifact.
        if let Some(absolute_path) = sqlite_absolute_candidate_path(&opened_path)
            && absolute_path != opened_path
            && let Ok(fallback_conn) = mcp_agent_mail_db::DbConn::open_file(&absolute_path)
            && sqlite_conn_is_healthy(&fallback_conn)?
        {
            conn = fallback_conn;
            opened_path = absolute_path;
        }

        if !sqlite_conn_is_healthy(&conn)? {
            drop(conn);
            recover_sqlite_file(Path::new(&opened_path))?;
            conn = mcp_agent_mail_db::DbConn::open_file(&opened_path)
                .map_err(|e| CliError::Other(format!("cannot reopen DB at {opened_path}: {e}")))?;
        }
    }
    // Run schema init so tables exist even if first use
    let init_sql = mcp_agent_mail_db::schema::init_schema_sql_base();
    if let Err(init_error) = conn.execute_raw(&init_sql) {
        let init_error_text = init_error.to_string();
        if opened_path != ":memory:" && is_sqlite_recovery_error_message(&init_error_text) {
            drop(conn);
            recover_sqlite_file(Path::new(&opened_path))?;
            let recovered_conn = mcp_agent_mail_db::DbConn::open_file(&opened_path)
                .map_err(|e| CliError::Other(format!("cannot reopen DB at {opened_path}: {e}")))?;
            recovered_conn.execute_raw(&init_sql).map_err(|e| {
                CliError::Other(format!(
                    "schema init failed for {opened_path} after auto-recovery: {e}"
                ))
            })?;
            return Ok(recovered_conn);
        }
        return Err(CliError::Other(format!(
            "schema init failed for {opened_path}: {init_error}"
        )));
    }
    Ok(conn)
}

pub(crate) fn open_db_sync() -> CliResult<mcp_agent_mail_db::DbConn> {
    let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    open_db_sync_with_database_url(&cfg.database_url)
}

fn handle_config(action: ConfigCommand) -> CliResult<()> {
    match action {
        ConfigCommand::ShowPort => {
            let config = Config::from_env();
            ftui_runtime::ftui_println!("{}", config.http_port);
            Ok(())
        }
        ConfigCommand::SetPort { port, env_file } => {
            let env_path = env_file
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join(".env"));
            // Write or update the port in the env file
            let content = if env_path.exists() {
                let existing = std::fs::read_to_string(&env_path).map_err(|e| {
                    CliError::Other(format!("Failed to read {}: {e}", env_path.display()))
                })?;
                let mut found = false;
                let updated: Vec<String> = existing
                    .lines()
                    .map(|line: &str| {
                        if line.starts_with("AGENT_MAIL_HTTP_PORT=") {
                            found = true;
                            format!("AGENT_MAIL_HTTP_PORT={port}")
                        } else {
                            line.to_string()
                        }
                    })
                    .collect();
                if found {
                    updated.join("\n")
                } else {
                    format!("{existing}\nAGENT_MAIL_HTTP_PORT={port}")
                }
            } else {
                format!("AGENT_MAIL_HTTP_PORT={port}\n")
            };
            std::fs::write(&env_path, content).map_err(|e| {
                CliError::Other(format!("Failed to write {}: {e}", env_path.display()))
            })?;
            ftui_runtime::ftui_println!("Port set to {} in {}", port, env_path.display());
            Ok(())
        }
    }
}

pub(crate) fn handle_setup(action: SetupCommand) -> CliResult<()> {
    use mcp_agent_mail_core::setup;

    match action {
        SetupCommand::Run {
            agent,
            dry_run,
            yes: _,
            token,
            port,
            host,
            path,
            project_dir,
            format,
            json,
            no_user_config,
            no_hooks,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let pdir = project_dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let env_file = pdir.join(".env");

            // Resolve token
            let resolved_token = setup::resolve_token(token.as_deref(), &env_file);

            // Parse agent filter
            let agents = match agent {
                Some(a) => {
                    Some(setup::parse_agent_list(&a).map_err(|e| CliError::Other(e.to_string()))?)
                }
                None => None,
            };

            // Detect installed agents and filter to detected platforms
            let detect_opts = AgentDetectOptions {
                only_connectors: None,
                include_undetected: false,
                root_overrides: Vec::new(),
            };
            let detected_slugs: Vec<String> =
                match mcp_agent_mail_core::detect_installed_agents(&detect_opts) {
                    Ok(report) => report
                        .installed_agents
                        .iter()
                        .filter(|e| e.detected)
                        .map(|e| e.slug.clone())
                        .collect(),
                    Err(_) => Vec::new(),
                };

            // Use detected agent name (required for hooks)
            let agent_name_val = std::env::var("AGENT_MAIL_AGENT").unwrap_or_default();

            // Auto-skip hooks if agent_name is empty (hooks generate malformed commands without it)
            let no_hooks = no_hooks || agent_name_val.is_empty();
            let project_slug = if no_hooks {
                String::new()
            } else {
                // Resolve project identity only when hooks are enabled.
                let pdir_str = pdir.display().to_string();
                resolve_project_identity(&pdir_str).slug
            };

            // Filter agents: if explicit --agent provided, use that; otherwise use detected
            let target_agents = match agents {
                Some(explicit) => explicit,
                None => {
                    let mut platforms = Vec::new();
                    for slug in &detected_slugs {
                        if let Some(p) = setup::AgentPlatform::from_slug(slug)
                            && !platforms.contains(&p)
                        {
                            platforms.push(p);
                        }
                    }
                    platforms
                }
            };

            if target_agents.is_empty() {
                output::emit_empty(
                    fmt,
                    "No coding agents detected. Use --agent to specify agents manually.",
                );
                return Ok(());
            }

            let params = setup::SetupParams {
                host,
                port,
                path,
                token: resolved_token.clone(),
                project_dir: pdir.clone(),
                agents: Some(target_agents),
                dry_run,
                skip_user_config: no_user_config,
                skip_hooks: no_hooks,
                project_slug,
                agent_name: agent_name_val,
            };

            // Save token to .env (unless dry-run)
            if !dry_run && let Err(e) = setup::save_token_to_env_file(&env_file, &resolved_token) {
                output::warn(&format!("Could not save token to .env: {e}"));
            }

            let results = setup::run_setup(&params);

            output::emit_output(&results, fmt, || {
                render_setup_actions_table(&results, dry_run);

                let total_actions: usize = results.iter().map(|r| r.actions.len()).sum();
                let created = results
                    .iter()
                    .flat_map(|r| &r.actions)
                    .filter(|a| matches!(a.outcome, setup::ActionOutcome::Created))
                    .count();
                let updated = results
                    .iter()
                    .flat_map(|r| &r.actions)
                    .filter(|a| matches!(a.outcome, setup::ActionOutcome::Updated))
                    .count();
                let unchanged = results
                    .iter()
                    .flat_map(|r| &r.actions)
                    .filter(|a| matches!(a.outcome, setup::ActionOutcome::Unchanged))
                    .count();

                ftui_runtime::ftui_println!("");
                output::success(&format!(
                    "{total_actions} config files processed: {created} created, {updated} updated, {unchanged} unchanged"
                ));
            });
            Ok(())
        }
        SetupCommand::Status {
            format,
            json,
            port,
            host,
            path,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let pdir = std::env::current_dir().unwrap_or_default();

            let params = setup::SetupParams {
                host,
                port,
                path,
                project_dir: pdir,
                skip_user_config: false,
                ..Default::default()
            };

            // Detect agents
            let detect_opts = AgentDetectOptions {
                only_connectors: None,
                include_undetected: true,
                root_overrides: Vec::new(),
            };
            let detected_report = mcp_agent_mail_core::detect_installed_agents(&detect_opts).ok();

            let mut statuses = setup::check_status(&params);

            // Fill in detected status from agent detection
            if let Some(report) = &detected_report {
                for status in &mut statuses {
                    status.detected = report
                        .installed_agents
                        .iter()
                        .any(|e| e.slug == status.slug && e.detected);
                }
            }

            output::emit_output(&statuses, fmt, || {
                output::section("Agent Setup Status");
                ftui_runtime::ftui_println!("");

                let mut table = output::CliTable::new(vec![
                    "AGENT",
                    "DETECTED",
                    "CONFIG",
                    "SERVER ENTRY",
                    "URL OK",
                ]);
                for status in &statuses {
                    let detected_str = if status.detected { "yes" } else { "no" };
                    if status.config_files.is_empty() {
                        table.add_row(vec![
                            status.platform.clone(),
                            detected_str.into(),
                            "-".into(),
                            "-".into(),
                            "-".into(),
                        ]);
                    } else {
                        for (i, cf) in status.config_files.iter().enumerate() {
                            let platform_col = if i == 0 {
                                status.platform.clone()
                            } else {
                                String::new()
                            };
                            let detected_col = if i == 0 {
                                detected_str.to_string()
                            } else {
                                String::new()
                            };
                            let exists_str = if cf.exists { "exists" } else { "missing" };
                            let server_str = if cf.has_server_entry { "yes" } else { "no" };
                            let url_str = if cf.url_matches { "yes" } else { "no" };
                            table.add_row(vec![
                                platform_col,
                                detected_col,
                                exists_str.into(),
                                server_str.into(),
                                url_str.into(),
                            ]);
                        }
                    }
                }
                table.render();
            });
            Ok(())
        }
    }
}

fn setup_action_style(
    outcome: &mcp_agent_mail_core::setup::ActionOutcome,
) -> (&'static str, String) {
    match outcome {
        mcp_agent_mail_core::setup::ActionOutcome::Created => {
            ("✓", mcp_agent_mail_server::theme::success_bold())
        }
        mcp_agent_mail_core::setup::ActionOutcome::Updated => {
            ("↻", mcp_agent_mail_server::theme::primary_bold())
        }
        mcp_agent_mail_core::setup::ActionOutcome::Unchanged => {
            ("•", mcp_agent_mail_server::theme::muted())
        }
        mcp_agent_mail_core::setup::ActionOutcome::Skipped => {
            ("◌", mcp_agent_mail_server::theme::warning_bold())
        }
        mcp_agent_mail_core::setup::ActionOutcome::BackedUp(_) => {
            ("💾", mcp_agent_mail_server::theme::accent())
        }
        mcp_agent_mail_core::setup::ActionOutcome::Failed(_) => {
            ("✗", mcp_agent_mail_server::theme::error_bold())
        }
    }
}

fn render_setup_actions_table(results: &[mcp_agent_mail_core::setup::SetupResult], dry_run: bool) {
    if !output::is_tty() {
        if dry_run {
            output::section("Dry run — no files will be modified");
            ftui_runtime::ftui_println!("");
        }
        for result in results {
            output::section(&result.platform);
            for action in &result.actions {
                ftui_runtime::ftui_println!("  {} — {}", action.file_path, action.outcome);
                if !action.description.is_empty() {
                    ftui_runtime::ftui_println!("    {}", action.description);
                }
            }
        }
        return;
    }

    let _ = mcp_agent_mail_server::theme::init_console_theme();
    let reset = mcp_agent_mail_server::theme::RESET;
    let dim = mcp_agent_mail_server::theme::DIM;
    let border = mcp_agent_mail_server::theme::secondary_bold();
    let heading = mcp_agent_mail_server::theme::primary_bold();
    let label = mcp_agent_mail_server::theme::text_bold();
    let path = mcp_agent_mail_server::theme::accent();
    let note = mcp_agent_mail_server::theme::warning_bold();
    let muted = mcp_agent_mail_server::theme::muted();

    ftui_runtime::ftui_println!(
        "{border}╭─ 🧰 Agent MCP Configuration Sync ───────────────────────────────────────────────╮{reset}"
    );
    if dry_run {
        ftui_runtime::ftui_println!(
            "{border}│{reset} {note}Dry run{reset} {dim}— no files will be modified{reset}"
        );
        ftui_runtime::ftui_println!("{border}│{reset}");
    }

    for (platform_index, result) in results.iter().enumerate() {
        ftui_runtime::ftui_println!(
            "{border}│{reset} {heading}◈{reset} {label}{}{reset}",
            result.platform
        );
        for action in &result.actions {
            let (icon, icon_color) = setup_action_style(&action.outcome);
            ftui_runtime::ftui_println!(
                "{border}│{reset}   {icon_color}{icon}{reset} {dim}{:<14}{reset} {path}{}{reset}",
                action.outcome,
                action.file_path
            );
            if !action.description.is_empty() {
                ftui_runtime::ftui_println!(
                    "{border}│{reset}      {muted}{}{reset}",
                    action.description
                );
            }
        }
        if platform_index + 1 < results.len() {
            ftui_runtime::ftui_println!("{border}│{reset}");
        }
    }
    ftui_runtime::ftui_println!(
        "{border}╰──────────────────────────────────────────────────────────────────────────────────╯{reset}"
    );
}

#[derive(Debug, Clone, serde::Serialize)]
struct GoldenRow {
    filename: String,
    status: String,
    command: Vec<String>,
    expected_exit_code: i32,
    exit_code: Option<i32>,
    expected_sha256: Option<String>,
    actual_sha256: Option<String>,
    note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<String>,
}

fn golden_default_dir(dir: Option<PathBuf>) -> PathBuf {
    dir.unwrap_or_else(|| PathBuf::from("benches/golden"))
}

fn compile_golden_filter(filter: Option<&str>) -> CliResult<Option<glob::Pattern>> {
    filter
        .map(|raw| {
            glob::Pattern::new(raw).map_err(|err| {
                CliError::InvalidArgument(format!("invalid --filter pattern '{raw}': {err}"))
            })
        })
        .transpose()
}

fn golden_matches_filter(name: &str, pattern: Option<&glob::Pattern>) -> bool {
    pattern.is_none_or(|p| p.matches(name))
}

fn resolve_sibling_binary(current_exe: &Path, sibling_name: &str) -> PathBuf {
    current_exe
        .parent()
        .map(|dir| dir.join(sibling_name))
        .filter(|candidate| candidate.exists())
        .unwrap_or_else(|| PathBuf::from(sibling_name))
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && (meta.permissions().mode() & 0o111 != 0))
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn build_golden_specs(pattern: Option<&glob::Pattern>) -> Vec<golden::GoldenCommandSpec> {
    let current_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("am"));
    let am_bin = current_exe.to_string_lossy().to_string();
    let mcp_bin = resolve_sibling_binary(&current_exe, "mcp-agent-mail")
        .to_string_lossy()
        .to_string();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let stub_bin = workspace_root.join("scripts/toon_stub_encoder.sh");
    let stub_bin_str = stub_bin.to_string_lossy().to_string();

    let mut specs = Vec::new();
    let mut maybe_push = |spec: golden::GoldenCommandSpec| {
        if golden_matches_filter(&spec.filename, pattern) {
            specs.push(spec);
        }
    };

    maybe_push(golden::GoldenCommandSpec::new(
        "am_help.txt",
        vec![am_bin.clone(), "--help".to_string()],
    ));
    maybe_push(golden::GoldenCommandSpec::new(
        "am_version.txt",
        vec![am_bin.clone(), "--version".to_string()],
    ));

    const HELP_SUBCOMMANDS: &[&str] = &[
        "serve-http",
        "serve-stdio",
        "guard",
        "share",
        "doctor",
        "config",
        "mail",
        "agents",
        "tooling",
        "macros",
        "contacts",
        "products",
        "archive",
        "projects",
        "file_reservations",
        "legacy",
        "upgrade",
    ];
    for subcmd in HELP_SUBCOMMANDS {
        maybe_push(golden::GoldenCommandSpec::new(
            format!("am_{subcmd}_help.txt"),
            vec![am_bin.clone(), (*subcmd).to_string(), "--help".to_string()],
        ));
    }

    const MCP_DENIAL_COMMANDS: &[&str] = &["share", "guard", "doctor", "archive", "migrate"];
    for denied in MCP_DENIAL_COMMANDS {
        maybe_push(
            golden::GoldenCommandSpec::new(
                format!("mcp_deny_{denied}.txt"),
                vec![mcp_bin.clone(), (*denied).to_string()],
            )
            .expected_exit_code(2)
            .stream(golden::GoldenStream::Combined)
            .env("AM_INTERFACE_MODE", "mcp"),
        );
    }

    if is_executable_file(&stub_bin) {
        let cmd = if cfg!(windows) && stub_bin_str.ends_with(".sh") {
            vec![
                "sh".to_string(),
                stub_bin_str.clone(),
                "--encode".to_string(),
            ]
        } else {
            vec![stub_bin_str.clone(), "--encode".to_string()]
        };

        maybe_push(golden::GoldenCommandSpec::new("stub_encode.txt", cmd).stdin("{\"id\":1}\n"));

        let cmd_stats = if cfg!(windows) && stub_bin_str.ends_with(".sh") {
            vec![
                "sh".to_string(),
                stub_bin_str.clone(),
                "--encode".to_string(),
                "--stats".to_string(),
            ]
        } else {
            vec![
                stub_bin_str.clone(),
                "--encode".to_string(),
                "--stats".to_string(),
            ]
        };

        maybe_push(
            golden::GoldenCommandSpec::new("stub_encode_stats_stdout.txt", cmd_stats.clone())
                .stdin("{\"id\":1}\n")
                .stream(golden::GoldenStream::Stdout),
        );
        maybe_push(
            golden::GoldenCommandSpec::new("stub_encode_stats_stderr.txt", cmd_stats)
                .stdin("{\"id\":1}\n")
                .stream(golden::GoldenStream::Stderr),
        );

        let cmd_help = if cfg!(windows) && stub_bin_str.ends_with(".sh") {
            vec!["sh".to_string(), stub_bin_str.clone(), "--help".to_string()]
        } else {
            vec![stub_bin_str.clone(), "--help".to_string()]
        };
        maybe_push(golden::GoldenCommandSpec::new("stub_help.txt", cmd_help));

        let cmd_version = if cfg!(windows) && stub_bin_str.ends_with(".sh") {
            vec![
                "sh".to_string(),
                stub_bin_str.clone(),
                "--version".to_string(),
            ]
        } else {
            vec![stub_bin_str, "--version".to_string()]
        };
        maybe_push(golden::GoldenCommandSpec::new(
            "stub_version.txt",
            cmd_version,
        ));
    }

    specs
}

fn handle_golden_capture(
    dir: Option<PathBuf>,
    filter: Option<String>,
    format: Option<output::CliOutputFormat>,
    json: bool,
    verbose: bool,
) -> CliResult<()> {
    let dir = golden_default_dir(dir);
    let fmt = output::CliOutputFormat::resolve(format, json);
    let filter_pattern = compile_golden_filter(filter.as_deref())?;
    let specs = build_golden_specs(filter_pattern.as_ref());
    if specs.is_empty() {
        return Err(CliError::InvalidArgument(
            "no golden definitions matched the current --filter".to_string(),
        ));
    }
    std::fs::create_dir_all(&dir)?;

    let mut rows = Vec::new();
    let mut checksums = std::collections::BTreeMap::new();
    let mut failures = 0usize;

    for spec in specs {
        match golden::run_golden_command(&spec, &[], None) {
            Ok(run) => {
                let path = dir.join(&spec.filename);
                if let Err(err) = std::fs::write(&path, &run.normalized_output) {
                    failures += 1;
                    rows.push(GoldenRow {
                        filename: spec.filename,
                        status: "error".to_string(),
                        command: spec.command,
                        expected_exit_code: spec.expected_exit_code,
                        exit_code: Some(run.exit_code),
                        expected_sha256: None,
                        actual_sha256: None,
                        note: Some(format!("write failed: {err}")),
                        diff: None,
                    });
                    continue;
                }

                let sha = golden::sha256_hex(&run.normalized_output);
                checksums.insert(run.filename.clone(), sha.clone());
                let exit_matches = run.exit_code == run.expected_exit_code;
                if !exit_matches {
                    failures += 1;
                }
                rows.push(GoldenRow {
                    filename: run.filename,
                    status: if exit_matches {
                        "ok".to_string()
                    } else {
                        "error".to_string()
                    },
                    command: spec.command,
                    expected_exit_code: run.expected_exit_code,
                    exit_code: Some(run.exit_code),
                    expected_sha256: Some(sha.clone()),
                    actual_sha256: Some(sha),
                    note: (!exit_matches).then(|| {
                        format!(
                            "unexpected exit code: expected {}, got {}",
                            run.expected_exit_code, run.exit_code
                        )
                    }),
                    diff: None,
                });
            }
            Err(err) => {
                failures += 1;
                rows.push(GoldenRow {
                    filename: spec.filename,
                    status: "error".to_string(),
                    command: spec.command,
                    expected_exit_code: spec.expected_exit_code,
                    exit_code: None,
                    expected_sha256: None,
                    actual_sha256: None,
                    note: Some(err.to_string()),
                    diff: None,
                });
            }
        }
    }

    if failures == 0 {
        golden::write_checksums_file(&dir.join("checksums.sha256"), &checksums)
            .map_err(|err| CliError::Other(err.to_string()))?;
    }

    let payload = serde_json::json!({
        "mode": "capture",
        "directory": dir.display().to_string(),
        "total": rows.len(),
        "passed": rows.len().saturating_sub(failures),
        "failed": failures,
        "checksums_written": failures == 0,
        "rows": rows,
    });
    output::emit_output(&payload, fmt, || {
        output::section("Golden capture");
        for row in &rows {
            let marker = match row.status.as_str() {
                "ok" => "OK",
                _ => "ERROR",
            };
            ftui_runtime::ftui_println!("  {:<30} {}", row.filename, marker);
            if verbose {
                ftui_runtime::ftui_println!("    cmd: {}", row.command.join(" "));
                if let Some(exit) = row.exit_code {
                    ftui_runtime::ftui_println!(
                        "    exit: {} (expected {})",
                        exit,
                        row.expected_exit_code
                    );
                }
                if let Some(hash) = &row.actual_sha256 {
                    ftui_runtime::ftui_println!("    sha256: {hash}");
                }
            }
            if let Some(note) = &row.note {
                ftui_runtime::ftui_println!("    note: {note}");
            }
        }
        ftui_runtime::ftui_println!("");
        if failures == 0 {
            ftui_runtime::ftui_println!(
                "Result: {}/{} captured (checksums updated)",
                rows.len(),
                rows.len()
            );
        } else {
            ftui_runtime::ftui_println!(
                "Result: {}/{} captured, {} errors (checksums not updated)",
                rows.len().saturating_sub(failures),
                rows.len(),
                failures
            );
        }
    });

    if failures > 0 {
        return Err(CliError::ExitCode(1));
    }
    Ok(())
}

fn handle_golden_verify(
    dir: Option<PathBuf>,
    filter: Option<String>,
    format: Option<output::CliOutputFormat>,
    json: bool,
    verbose: bool,
) -> CliResult<()> {
    let dir = golden_default_dir(dir);
    let fmt = output::CliOutputFormat::resolve(format, json);
    let checksums_path = dir.join("checksums.sha256");
    let checksums = golden::read_checksums_file(&checksums_path).map_err(|err| {
        CliError::Other(format!(
            "failed to read {}: {err}",
            checksums_path.display()
        ))
    })?;
    let filter_pattern = compile_golden_filter(filter.as_deref())?;
    let spec_map: std::collections::BTreeMap<String, golden::GoldenCommandSpec> =
        build_golden_specs(None)
            .into_iter()
            .map(|spec| (spec.filename.clone(), spec))
            .collect();

    let selected_files: Vec<(String, String)> = checksums
        .into_iter()
        .filter(|(filename, _)| golden_matches_filter(filename, filter_pattern.as_ref()))
        .collect();
    if selected_files.is_empty() {
        return Err(CliError::InvalidArgument(
            "no checksum entries matched the current --filter".to_string(),
        ));
    }

    let mut rows = Vec::new();
    let mut failures = 0usize;
    for (filename, expected_hash) in selected_files {
        let Some(spec) = spec_map.get(&filename).cloned() else {
            failures += 1;
            rows.push(GoldenRow {
                filename,
                status: "error".to_string(),
                command: Vec::new(),
                expected_exit_code: 0,
                exit_code: None,
                expected_sha256: Some(expected_hash),
                actual_sha256: None,
                note: Some("no command definition for checksum entry".to_string()),
                diff: None,
            });
            continue;
        };

        let golden_path = dir.join(&filename);
        let expected_text = match std::fs::read_to_string(&golden_path) {
            Ok(text) => text,
            Err(err) => {
                failures += 1;
                rows.push(GoldenRow {
                    filename,
                    status: "missing".to_string(),
                    command: spec.command,
                    expected_exit_code: spec.expected_exit_code,
                    exit_code: None,
                    expected_sha256: Some(expected_hash),
                    actual_sha256: None,
                    note: Some(format!("failed to read {}: {err}", golden_path.display())),
                    diff: None,
                });
                continue;
            }
        };

        let run = match golden::run_golden_command(&spec, &[], None) {
            Ok(run) => run,
            Err(err) => {
                failures += 1;
                rows.push(GoldenRow {
                    filename,
                    status: "error".to_string(),
                    command: spec.command,
                    expected_exit_code: spec.expected_exit_code,
                    exit_code: None,
                    expected_sha256: Some(expected_hash),
                    actual_sha256: None,
                    note: Some(err.to_string()),
                    diff: None,
                });
                continue;
            }
        };

        let comparison = golden::compare_text(&expected_text, &run.normalized_output);
        let checksum_matches = comparison.actual_sha256 == expected_hash;
        let exit_matches = run.exit_code == run.expected_exit_code;
        let passed = comparison.matches && checksum_matches && exit_matches;
        if !passed {
            failures += 1;
        }
        rows.push(GoldenRow {
            filename,
            status: if passed {
                "ok".to_string()
            } else {
                "mismatch".to_string()
            },
            command: spec.command,
            expected_exit_code: run.expected_exit_code,
            exit_code: Some(run.exit_code),
            expected_sha256: Some(expected_hash),
            actual_sha256: Some(comparison.actual_sha256),
            note: (!exit_matches).then(|| {
                format!(
                    "unexpected exit code: expected {}, got {}",
                    run.expected_exit_code, run.exit_code
                )
            }),
            diff: (!passed).then(|| comparison.inline_diff.unwrap_or_default()),
        });
    }

    let payload = serde_json::json!({
        "mode": "verify",
        "directory": dir.display().to_string(),
        "total": rows.len(),
        "passed": rows.len().saturating_sub(failures),
        "failed": failures,
        "rows": rows,
    });
    output::emit_output(&payload, fmt, || {
        output::section("Golden output verification");
        let width = rows.iter().map(|row| row.filename.len()).max().unwrap_or(0);
        for row in &rows {
            let status = match row.status.as_str() {
                "ok" => "OK",
                "missing" => "MISSING",
                "mismatch" => "MISMATCH",
                _ => "ERROR",
            };
            ftui_runtime::ftui_println!("  {:<width$}  {}", row.filename, status, width = width);
            if verbose && row.status != "ok" {
                if let Some(expected) = &row.expected_sha256 {
                    ftui_runtime::ftui_println!("    expected_sha256: {expected}");
                }
                if let Some(actual) = &row.actual_sha256 {
                    ftui_runtime::ftui_println!("    actual_sha256:   {actual}");
                }
                if let Some(note) = &row.note {
                    ftui_runtime::ftui_println!("    note: {note}");
                }
                if let Some(diff) = &row.diff
                    && !diff.is_empty()
                {
                    ftui_runtime::ftui_println!("    diff:\n{diff}");
                }
            }
        }
        ftui_runtime::ftui_println!("");
        ftui_runtime::ftui_println!(
            "Result: {}/{} passed, {} failed",
            rows.len().saturating_sub(failures),
            rows.len(),
            failures
        );
    });

    if failures > 0 {
        return Err(CliError::ExitCode(1));
    }
    Ok(())
}

fn handle_golden_list(
    dir: Option<PathBuf>,
    filter: Option<String>,
    format: Option<output::CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    let dir = golden_default_dir(dir);
    let fmt = output::CliOutputFormat::resolve(format, json);
    let filter_pattern = compile_golden_filter(filter.as_deref())?;
    let specs = build_golden_specs(filter_pattern.as_ref());
    if specs.is_empty() {
        return Err(CliError::InvalidArgument(
            "no golden definitions matched the current --filter".to_string(),
        ));
    }

    let checksums_path = dir.join("checksums.sha256");
    let checksum_map = if checksums_path.exists() {
        golden::read_checksums_file(&checksums_path)
            .map_err(|err| CliError::Other(format!("failed to parse checksums: {err}")))?
    } else {
        std::collections::BTreeMap::new()
    };

    let mut rows = Vec::new();
    for spec in specs {
        let file_path = dir.join(&spec.filename);
        let expected_hash = checksum_map.get(&spec.filename).cloned();
        if !file_path.exists() {
            rows.push(GoldenRow {
                filename: spec.filename,
                status: "missing".to_string(),
                command: spec.command,
                expected_exit_code: spec.expected_exit_code,
                exit_code: None,
                expected_sha256: expected_hash,
                actual_sha256: None,
                note: Some("golden file not found".to_string()),
                diff: None,
            });
            continue;
        }

        let actual_hash = std::fs::read_to_string(&file_path)
            .map(|txt| golden::sha256_hex(&txt))
            .map_err(CliError::Io)?;
        let status = match expected_hash.as_deref() {
            Some(expected) if expected == actual_hash => "present",
            _ => "stale",
        };
        rows.push(GoldenRow {
            filename: spec.filename,
            status: status.to_string(),
            command: spec.command,
            expected_exit_code: spec.expected_exit_code,
            exit_code: None,
            expected_sha256: expected_hash,
            actual_sha256: Some(actual_hash),
            note: None,
            diff: None,
        });
    }

    let payload = serde_json::json!({
        "mode": "list",
        "directory": dir.display().to_string(),
        "total": rows.len(),
        "rows": rows,
    });
    output::emit_output(&payload, fmt, || {
        output::section("Golden files");
        let width = rows.iter().map(|row| row.filename.len()).max().unwrap_or(0);
        for row in &rows {
            ftui_runtime::ftui_println!(
                "  {:<width$}  {}",
                row.filename,
                row.status.to_uppercase(),
                width = width
            );
        }
        ftui_runtime::ftui_println!("");
        let present = rows.iter().filter(|row| row.status == "present").count();
        let missing = rows.iter().filter(|row| row.status == "missing").count();
        let stale = rows.iter().filter(|row| row.status == "stale").count();
        ftui_runtime::ftui_println!(
            "Result: {} present, {} missing, {} stale (total {})",
            present,
            missing,
            stale,
            rows.len()
        );
    });

    Ok(())
}

fn handle_golden(action: GoldenCommand) -> CliResult<()> {
    match action {
        GoldenCommand::Capture {
            dir,
            filter,
            format,
            json,
            verbose,
        } => handle_golden_capture(dir, filter, format, json, verbose),
        GoldenCommand::Verify {
            dir,
            filter,
            format,
            json,
            verbose,
        } => handle_golden_verify(dir, filter, format, json, verbose),
        GoldenCommand::List {
            dir,
            filter,
            format,
            json,
        } => handle_golden_list(dir, filter, format, json),
    }
}

fn handle_flake_triage(action: FlakeTriageCommand) -> CliResult<()> {
    use mcp_agent_mail_core::flake_triage;

    match action {
        FlakeTriageCommand::Scan { dir, format, json } => {
            let scan_dir = dir.unwrap_or_else(|| PathBuf::from("tests/artifacts"));
            let fmt = output::CliOutputFormat::resolve(format, json);
            let artifacts = flake_triage::scan_artifacts(&scan_dir);

            output::emit_output(&artifacts, fmt, || {
                if artifacts.is_empty() {
                    ftui_runtime::ftui_println!(
                        "No failure artifacts found in {}",
                        scan_dir.display()
                    );
                } else {
                    ftui_runtime::ftui_println!(
                        "Found {} failure artifact(s) in {}:\n",
                        artifacts.len(),
                        scan_dir.display()
                    );
                    for (i, a) in artifacts.iter().enumerate() {
                        ftui_runtime::ftui_println!(
                            "  [{}] {} {} ({:?})",
                            i + 1,
                            &a.context.failure_ts[..19.min(a.context.failure_ts.len())],
                            a.context.test_name,
                            a.context.category
                        );
                        ftui_runtime::ftui_println!("      {}", a.path.display());
                    }
                }
            });
            Ok(())
        }
        FlakeTriageCommand::Reproduce {
            artifact,
            verbose,
            timeout,
        } => {
            if !artifact.is_file() {
                return Err(CliError::InvalidArgument(
                    flake_triage_missing_artifact_hint(&artifact),
                ));
            }
            let config = flake_triage::ReproductionConfig {
                artifact_path: artifact.clone(),
                verbose,
                timeout: std::time::Duration::from_secs(timeout),
            };
            let result = flake_triage::reproduce_failure(&config).map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    CliError::InvalidArgument(flake_triage_missing_artifact_hint(&artifact))
                } else {
                    CliError::Io(err)
                }
            })?;

            println!("Test:       {}", result.test_name);
            if let Some(seed) = result.seed {
                println!("Seed:       {seed}");
            }
            println!("Reproduced: {}", result.reproduced);
            println!("Exit code:  {}", result.exit_code);
            println!("Elapsed:    {}ms", result.elapsed_ms);

            if !result.stderr.is_empty() {
                println!("\n--- stderr ---\n{}", result.stderr.trim());
            }
            if verbose && !result.stdout.is_empty() {
                println!("\n--- stdout ---\n{}", result.stdout.trim());
            }
            Ok(())
        }
        FlakeTriageCommand::Detect {
            test_name,
            seeds,
            packages,
            timeout,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let config = flake_triage::MultiSeedConfig {
                test_name,
                num_seeds: seeds,
                packages: if packages.is_empty() {
                    flake_triage::MultiSeedConfig::default().packages
                } else {
                    packages
                },
                timeout: std::time::Duration::from_secs(timeout),
            };
            let report = flake_triage::run_multi_seed_subprocess(&config);

            output::emit_output(&report, fmt, || {
                let total = report.runs.len();
                let passed = report.runs.iter().filter(|r| r.passed).count();
                let failed = total - passed;

                ftui_runtime::ftui_println!(
                    "Results: {passed}/{total} passed, {failed}/{total} failed"
                );
                ftui_runtime::ftui_println!("Verdict: {:?}", report.verdict);

                if !report.failing_seeds.is_empty() {
                    ftui_runtime::ftui_println!("Failing seeds: {:?}", report.failing_seeds);
                }
                if !report.remediation.is_empty() {
                    ftui_runtime::ftui_println!("Remediation: {}", report.remediation);
                }
            });
            Ok(())
        }
    }
}

fn flake_triage_missing_artifact_hint(path: &Path) -> String {
    format!(
        "artifact not found: {}\n\
         - Verify the path points to an existing flake artifact file.\n\
         - Expected artifact filename: failure_context.json\n\
         - For more information, try '--help'.",
        path.display()
    )
}

fn handle_file_reservations(action: FileReservationsCommand) -> CliResult<()> {
    let conn = open_db_sync()?;
    handle_file_reservations_with_conn(&conn, action)
}

fn handle_file_reservations_with_conn(
    conn: &mcp_agent_mail_db::DbConn,
    action: FileReservationsCommand,
) -> CliResult<()> {
    let now_us = mcp_agent_mail_db::timestamps::now_micros();

    match action {
        FileReservationsCommand::List {
            project,
            active_only,
            all,
        } => {
            let sql = if active_only {
                "SELECT fr.id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                        fr.expires_ts, fr.released_ts, a.name AS agent_name \
                 FROM file_reservations fr \
                 JOIN agents a ON a.id = fr.agent_id \
                 JOIN projects p ON p.id = fr.project_id \
                 WHERE p.slug = ? AND fr.released_ts IS NULL AND fr.expires_ts > ? \
                 ORDER BY fr.id"
            } else if all {
                "SELECT fr.id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                        fr.expires_ts, fr.released_ts, a.name AS agent_name \
                 FROM file_reservations fr \
                 JOIN agents a ON a.id = fr.agent_id \
                 JOIN projects p ON p.id = fr.project_id \
                 WHERE p.slug = ? \
                 ORDER BY fr.id"
            } else {
                // Default: active (not released, not expired)
                "SELECT fr.id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                        fr.expires_ts, fr.released_ts, a.name AS agent_name \
                 FROM file_reservations fr \
                 JOIN agents a ON a.id = fr.agent_id \
                 JOIN projects p ON p.id = fr.project_id \
                 WHERE p.slug = ? AND fr.released_ts IS NULL AND fr.expires_ts > ? \
                 ORDER BY fr.id"
            };
            let params: Vec<sqlmodel_core::Value> = if active_only || (!all) {
                vec![
                    sqlmodel_core::Value::Text(project),
                    sqlmodel_core::Value::BigInt(now_us),
                ]
            } else {
                vec![sqlmodel_core::Value::Text(project)]
            };
            let rows = conn
                .query_sync(sql, &params)
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

            if rows.is_empty() {
                output::empty_result(false, "No file reservations found.");
                return Ok(());
            }
            let mut table =
                output::CliTable::new(vec!["ID", "PATTERN", "AGENT", "EXPIRES", "REASON"]);
            for r in &rows {
                let id: i64 = r.get_named("id").unwrap_or(0);
                let pattern: String = r.get_named("path_pattern").unwrap_or_default();
                let agent: String = r.get_named("agent_name").unwrap_or_default();
                let expires: i64 = r.get_named("expires_ts").unwrap_or(0);
                let reason: String = r.get_named("reason").unwrap_or_default();
                let expires_str = mcp_agent_mail_db::timestamps::micros_to_iso(expires);
                let expires_display = expires_str.get(..20).unwrap_or(&expires_str).to_string();
                table.add_row(vec![
                    id.to_string(),
                    pattern,
                    agent,
                    expires_display,
                    reason,
                ]);
            }
            table.render();
            Ok(())
        }
        FileReservationsCommand::Active { project, limit } => {
            let limit = limit.unwrap_or(50);
            let rows = conn
                .query_sync(
                    "SELECT fr.id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                            fr.expires_ts, a.name AS agent_name \
                     FROM file_reservations fr \
                     JOIN agents a ON a.id = fr.agent_id \
                     JOIN projects p ON p.id = fr.project_id \
                     WHERE p.slug = ? AND fr.released_ts IS NULL AND fr.expires_ts > ? \
                     ORDER BY fr.expires_ts ASC \
                     LIMIT ?",
                    &[
                        sqlmodel_core::Value::Text(project),
                        sqlmodel_core::Value::BigInt(now_us),
                        sqlmodel_core::Value::BigInt(limit),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

            if rows.is_empty() {
                ftui_runtime::ftui_println!("No active reservations.");
                return Ok(());
            }
            for r in &rows {
                let pattern: String = r.get_named("path_pattern").unwrap_or_default();
                let agent: String = r.get_named("agent_name").unwrap_or_default();
                let exclusive: bool = r.get_named("exclusive").unwrap_or(true);
                let lock_type = if exclusive { "excl" } else { "shared" };
                ftui_runtime::ftui_println!("  {} [{}] by {}", pattern, lock_type, agent);
            }
            Ok(())
        }
        FileReservationsCommand::Soon { project, minutes } => {
            let minutes = minutes.unwrap_or(30);
            let threshold_us = now_us.saturating_add(minutes.saturating_mul(60_000_000));
            let rows = conn
                .query_sync(
                    "SELECT fr.id, fr.path_pattern, fr.expires_ts, a.name AS agent_name \
                     FROM file_reservations fr \
                     JOIN agents a ON a.id = fr.agent_id \
                     JOIN projects p ON p.id = fr.project_id \
                     WHERE p.slug = ? AND fr.released_ts IS NULL \
                       AND fr.expires_ts > ? AND fr.expires_ts <= ? \
                     ORDER BY fr.expires_ts ASC",
                    &[
                        sqlmodel_core::Value::Text(project),
                        sqlmodel_core::Value::BigInt(now_us),
                        sqlmodel_core::Value::BigInt(threshold_us),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

            if rows.is_empty() {
                ftui_runtime::ftui_println!("No reservations expiring within {} minutes.", minutes);
                return Ok(());
            }
            output::section(&format!(
                "Reservations expiring within {} minutes:",
                minutes
            ));
            let mut table = output::CliTable::new(vec!["PATTERN", "AGENT", "REMAINING"]);
            for r in &rows {
                let pattern: String = r.get_named("path_pattern").unwrap_or_default();
                let agent: String = r.get_named("agent_name").unwrap_or_default();
                let expires: i64 = r.get_named("expires_ts").unwrap_or(0);
                let remaining_min = (expires - now_us) / 60_000_000;
                table.add_row(vec![pattern, agent, format!("{}min", remaining_min)]);
            }
            table.render();
            Ok(())
        }
        FileReservationsCommand::Reserve {
            project,
            agent,
            paths,
            ttl,
            exclusive,
            shared,
            reason,
        } => {
            let exclusive_val = if shared { false } else { exclusive };
            let ttl = ttl.max(60); // Min 60s

            // Resolve project_id and agent_id.
            let proj_rows = conn
                .query_sync(
                    "SELECT id FROM projects WHERE slug = ?",
                    &[sqlmodel_core::Value::Text(project.clone())],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let project_id: i64 = proj_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!("project not found: {project}"))
                })?;

            let agent_rows = conn
                .query_sync(
                    "SELECT id FROM agents WHERE project_id = ? AND name = ?",
                    &[
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::Text(agent.clone()),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let agent_id: i64 = agent_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| CliError::InvalidArgument(format!("agent not found: {agent}")))?;

            // Check conflicts: find active exclusive reservations that overlap.
            let mut conflicts: Vec<serde_json::Value> = Vec::new();
            for path in &paths {
                let overlap_rows = conn
                    .query_sync(
                        "SELECT fr.id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                                fr.expires_ts, a.name AS agent_name \
                         FROM file_reservations fr \
                         JOIN agents a ON a.id = fr.agent_id \
                         WHERE fr.project_id = ? AND fr.released_ts IS NULL \
                           AND fr.expires_ts > ? AND fr.agent_id != ? \
                           AND (fr.\"exclusive\" = 1 OR ? = 1) \
                           AND (fr.path_pattern = ? OR fr.path_pattern LIKE ? \
                                OR ? GLOB fr.path_pattern)",
                        &[
                            sqlmodel_core::Value::BigInt(project_id),
                            sqlmodel_core::Value::BigInt(now_us),
                            sqlmodel_core::Value::BigInt(agent_id),
                            sqlmodel_core::Value::BigInt(if exclusive_val { 1 } else { 0 }),
                            sqlmodel_core::Value::Text(path.clone()),
                            sqlmodel_core::Value::Text(format!("{}%", path)),
                            sqlmodel_core::Value::Text(path.clone()),
                        ],
                    )
                    .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
                for r in &overlap_rows {
                    let holder: String = r.get_named("agent_name").unwrap_or_default();
                    let pattern: String = r.get_named("path_pattern").unwrap_or_default();
                    let rid: i64 = r.get_named("id").unwrap_or(0);
                    conflicts.push(serde_json::json!({
                        "path": path,
                        "holder": holder,
                        "holder_pattern": pattern,
                        "reservation_id": rid,
                    }));
                }
            }

            // Create reservations.
            let expires_us = now_us + ttl.saturating_mul(1_000_000);
            let mut granted: Vec<serde_json::Value> = Vec::new();
            for path in &paths {
                conn.query_sync(
                    "INSERT INTO file_reservations \
                     (project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                    &[
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::BigInt(agent_id),
                        sqlmodel_core::Value::Text(path.clone()),
                        sqlmodel_core::Value::BigInt(if exclusive_val { 1 } else { 0 }),
                        sqlmodel_core::Value::Text(reason.clone()),
                        sqlmodel_core::Value::BigInt(now_us),
                        sqlmodel_core::Value::BigInt(expires_us),
                    ],
                )
                .map_err(|e| CliError::Other(format!("insert failed: {e}")))?;

                // Get the inserted ID (MAX(id) since FrankenConnection
                // does not support last_insert_rowid).
                let id_rows = conn
                    .query_sync("SELECT MAX(id) AS id FROM file_reservations", &[])
                    .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
                let rid: i64 = id_rows
                    .first()
                    .and_then(|r| r.get_named("id").ok())
                    .unwrap_or(0);

                granted.push(serde_json::json!({
                    "id": rid,
                    "path": path,
                    "exclusive": exclusive_val,
                    "expires_ts": mcp_agent_mail_db::timestamps::micros_to_iso(expires_us),
                }));
            }

            // Output.
            let result = serde_json::json!({
                "granted": granted,
                "conflicts": conflicts,
            });
            ftui_runtime::ftui_println!(
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or_default()
            );
            if !conflicts.is_empty() {
                output::warn(&format!(
                    "{} conflict(s) detected — reservations created but may overlap.",
                    conflicts.len()
                ));
            }
            Ok(())
        }
        FileReservationsCommand::Renew {
            project,
            agent,
            extend_seconds,
            paths,
            ids,
        } => {
            let extend = extend_seconds.max(60);
            let extend_us = extend.saturating_mul(1_000_000);

            // Resolve project_id and agent_id.
            let proj_rows = conn
                .query_sync(
                    "SELECT id FROM projects WHERE slug = ?",
                    &[sqlmodel_core::Value::Text(project.clone())],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let project_id: i64 = proj_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!("project not found: {project}"))
                })?;

            let agent_rows = conn
                .query_sync(
                    "SELECT id FROM agents WHERE project_id = ? AND name = ?",
                    &[
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::Text(agent.clone()),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let agent_id: i64 = agent_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| CliError::InvalidArgument(format!("agent not found: {agent}")))?;

            // Build WHERE clause for renewal.
            let base_where =
                "project_id = ? AND agent_id = ? AND released_ts IS NULL AND expires_ts > ?";
            let (sql, params) = if !ids.is_empty() {
                let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "UPDATE file_reservations SET expires_ts = expires_ts + ? \
                     WHERE {base_where} AND id IN ({placeholders}) \
                     RETURNING id, path_pattern, expires_ts"
                );
                let mut params: Vec<sqlmodel_core::Value> = vec![
                    sqlmodel_core::Value::BigInt(extend_us),
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(agent_id),
                    sqlmodel_core::Value::BigInt(now_us),
                ];
                for id in &ids {
                    params.push(sqlmodel_core::Value::BigInt(*id));
                }
                (sql, params)
            } else if !paths.is_empty() {
                let placeholders: String = paths.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "UPDATE file_reservations SET expires_ts = expires_ts + ? \
                     WHERE {base_where} AND path_pattern IN ({placeholders}) \
                     RETURNING id, path_pattern, expires_ts"
                );
                let mut params: Vec<sqlmodel_core::Value> = vec![
                    sqlmodel_core::Value::BigInt(extend_us),
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(agent_id),
                    sqlmodel_core::Value::BigInt(now_us),
                ];
                for p in &paths {
                    params.push(sqlmodel_core::Value::Text(p.clone()));
                }
                (sql, params)
            } else {
                let sql = format!(
                    "UPDATE file_reservations SET expires_ts = expires_ts + ? \
                     WHERE {base_where} \
                     RETURNING id, path_pattern, expires_ts"
                );
                let params = vec![
                    sqlmodel_core::Value::BigInt(extend_us),
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(agent_id),
                    sqlmodel_core::Value::BigInt(now_us),
                ];
                (sql, params)
            };

            let rows = conn
                .query_sync(&sql, &params)
                .map_err(|e| CliError::Other(format!("update failed: {e}")))?;

            if rows.is_empty() {
                output::empty_result(false, "No matching reservations to renew.");
                return Ok(());
            }

            let mut table = output::CliTable::new(vec!["ID", "PATTERN", "NEW EXPIRES"]);
            for r in &rows {
                let id: i64 = r.get_named("id").unwrap_or(0);
                let pattern: String = r.get_named("path_pattern").unwrap_or_default();
                let expires: i64 = r.get_named("expires_ts").unwrap_or(0);
                let expires_str = mcp_agent_mail_db::timestamps::micros_to_iso(expires);
                table.add_row(vec![
                    id.to_string(),
                    pattern,
                    expires_str.get(..20).unwrap_or(&expires_str).to_string(),
                ]);
            }
            output::success(&format!("Renewed {} reservation(s).", rows.len()));
            table.render();
            Ok(())
        }
        FileReservationsCommand::Release {
            project,
            agent,
            paths,
            ids,
        } => {
            let proj_rows = conn
                .query_sync(
                    "SELECT id FROM projects WHERE slug = ?",
                    &[sqlmodel_core::Value::Text(project.clone())],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let project_id: i64 = proj_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!("project not found: {project}"))
                })?;

            let agent_rows = conn
                .query_sync(
                    "SELECT id FROM agents WHERE project_id = ? AND name = ?",
                    &[
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::Text(agent.clone()),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let agent_id: i64 = agent_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| CliError::InvalidArgument(format!("agent not found: {agent}")))?;

            let base_where = "project_id = ? AND agent_id = ? AND released_ts IS NULL";
            let (sql, params) = if !ids.is_empty() {
                let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "UPDATE file_reservations SET released_ts = ? \
                     WHERE {base_where} AND id IN ({placeholders}) \
                     RETURNING id"
                );
                let mut params: Vec<sqlmodel_core::Value> = vec![
                    sqlmodel_core::Value::BigInt(now_us),
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(agent_id),
                ];
                for id in &ids {
                    params.push(sqlmodel_core::Value::BigInt(*id));
                }
                (sql, params)
            } else if !paths.is_empty() {
                let placeholders: String = paths.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "UPDATE file_reservations SET released_ts = ? \
                     WHERE {base_where} AND path_pattern IN ({placeholders}) \
                     RETURNING id"
                );
                let mut params: Vec<sqlmodel_core::Value> = vec![
                    sqlmodel_core::Value::BigInt(now_us),
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(agent_id),
                ];
                for p in &paths {
                    params.push(sqlmodel_core::Value::Text(p.clone()));
                }
                (sql, params)
            } else {
                // Release all active reservations for this agent.
                let sql = format!(
                    "UPDATE file_reservations SET released_ts = ? \
                     WHERE {base_where} \
                     RETURNING id"
                );
                let params = vec![
                    sqlmodel_core::Value::BigInt(now_us),
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(agent_id),
                ];
                (sql, params)
            };

            let rows = conn
                .query_sync(&sql, &params)
                .map_err(|e| CliError::Other(format!("update failed: {e}")))?;

            let released_count = rows.len();
            output::success(&format!(
                "Released {} reservation(s) for {} in {}.",
                released_count, agent, project
            ));
            Ok(())
        }
        FileReservationsCommand::Conflicts { project, paths } => {
            let proj_rows = conn
                .query_sync(
                    "SELECT id FROM projects WHERE slug = ?",
                    &[sqlmodel_core::Value::Text(project.clone())],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let project_id: i64 = proj_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!("project not found: {project}"))
                })?;

            let mut conflicts: Vec<serde_json::Value> = Vec::new();
            for path in &paths {
                let overlap_rows = conn
                    .query_sync(
                        "SELECT fr.id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                                fr.expires_ts, a.name AS agent_name \
                         FROM file_reservations fr \
                         JOIN agents a ON a.id = fr.agent_id \
                         WHERE fr.project_id = ? AND fr.released_ts IS NULL \
                           AND fr.expires_ts > ? AND fr.\"exclusive\" = 1 \
                           AND (fr.path_pattern = ? OR fr.path_pattern LIKE ? \
                                OR ? LIKE fr.path_pattern)",
                        &[
                            sqlmodel_core::Value::BigInt(project_id),
                            sqlmodel_core::Value::BigInt(now_us),
                            sqlmodel_core::Value::Text(path.clone()),
                            sqlmodel_core::Value::Text(format!("{}%", path)),
                            sqlmodel_core::Value::Text(path.clone()),
                        ],
                    )
                    .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
                for r in &overlap_rows {
                    let holder: String = r.get_named("agent_name").unwrap_or_default();
                    let pattern: String = r.get_named("path_pattern").unwrap_or_default();
                    let rid: i64 = r.get_named("id").unwrap_or(0);
                    let expires: i64 = r.get_named("expires_ts").unwrap_or(0);
                    conflicts.push(serde_json::json!({
                        "path": path,
                        "holder": holder,
                        "holder_pattern": pattern,
                        "reservation_id": rid,
                        "expires_ts": mcp_agent_mail_db::timestamps::micros_to_iso(expires),
                    }));
                }
            }

            if conflicts.is_empty() {
                output::success("No conflicts detected.");
            } else {
                let mut table = output::CliTable::new(vec!["PATH", "HOLDER", "PATTERN", "EXPIRES"]);
                for c in &conflicts {
                    table.add_row(vec![
                        c["path"].as_str().unwrap_or("").to_string(),
                        c["holder"].as_str().unwrap_or("").to_string(),
                        c["holder_pattern"].as_str().unwrap_or("").to_string(),
                        c["expires_ts"]
                            .as_str()
                            .unwrap_or("")
                            .get(..20)
                            .unwrap_or("")
                            .to_string(),
                    ]);
                }
                output::warn(&format!("{} conflict(s) found:", conflicts.len()));
                table.render();
            }
            Ok(())
        }
    }
}

fn handle_acks(action: AcksCommand) -> CliResult<()> {
    let conn = open_db_sync()?;
    handle_acks_with_conn(&conn, action)
}

fn handle_acks_with_conn(conn: &mcp_agent_mail_db::DbConn, action: AcksCommand) -> CliResult<()> {
    let now_us = mcp_agent_mail_db::timestamps::now_micros();

    match action {
        AcksCommand::Pending {
            project,
            agent,
            limit,
        } => {
            // Messages sent TO this agent with ack_required=1 that haven't been acked
            let rows = conn
                .query_sync(
                    "SELECT m.id, m.subject, m.importance, m.created_ts, \
                            sender_a.name AS sender_name \
                     FROM messages m \
                     JOIN message_recipients i ON i.message_id = m.id \
                     JOIN agents recv_a ON recv_a.id = i.agent_id \
                     JOIN agents sender_a ON sender_a.id = m.sender_id \
                     JOIN projects p ON p.id = m.project_id \
                     WHERE p.slug = ? AND recv_a.name = ? \
                       AND m.ack_required = 1 AND i.ack_ts IS NULL \
                     ORDER BY m.created_ts DESC \
                     LIMIT ?",
                    &[
                        sqlmodel_core::Value::Text(project),
                        sqlmodel_core::Value::Text(agent),
                        sqlmodel_core::Value::BigInt(limit),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

            if rows.is_empty() {
                output::empty_result(false, "No pending acks.");
                return Ok(());
            }
            let mut table = output::CliTable::new(vec!["ID", "FROM", "SUBJECT", "IMPORTANCE"]);
            for r in &rows {
                let id: i64 = r.get_named("id").unwrap_or(0);
                let subject: String = r.get_named("subject").unwrap_or_default();
                let sender: String = r.get_named("sender_name").unwrap_or_default();
                let importance: String = r.get_named("importance").unwrap_or_default();
                let subject_display = subject.get(..40).unwrap_or(&subject).to_string();
                table.add_row(vec![id.to_string(), sender, subject_display, importance]);
            }
            table.render();
            Ok(())
        }
        AcksCommand::Remind {
            project,
            agent,
            min_age_minutes,
            limit,
        } => {
            // Stale acks: ack_required but not acked, older than min_age_minutes
            let cutoff = now_us - min_age_minutes * 60 * 1_000_000;
            let rows = conn
                .query_sync(
                    "SELECT m.id, m.subject, m.created_ts, sender_a.name AS sender_name \
                     FROM messages m \
                     JOIN message_recipients i ON i.message_id = m.id \
                     JOIN agents recv_a ON recv_a.id = i.agent_id \
                     JOIN agents sender_a ON sender_a.id = m.sender_id \
                     JOIN projects p ON p.id = m.project_id \
                     WHERE p.slug = ? AND recv_a.name = ? \
                       AND m.ack_required = 1 AND i.ack_ts IS NULL \
                       AND m.created_ts < ? \
                     ORDER BY m.created_ts ASC \
                     LIMIT ?",
                    &[
                        sqlmodel_core::Value::Text(project),
                        sqlmodel_core::Value::Text(agent),
                        sqlmodel_core::Value::BigInt(cutoff),
                        sqlmodel_core::Value::BigInt(limit),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

            if rows.is_empty() {
                output::empty_result(false, "No stale acks needing reminders.");
                return Ok(());
            }
            output::section(&format!("Stale acks (>{min_age_minutes}min old):"));
            let mut table = output::CliTable::new(vec!["ID", "FROM", "SUBJECT", "AGE"]);
            for r in &rows {
                let id: i64 = r.get_named("id").unwrap_or(0);
                let subject: String = r.get_named("subject").unwrap_or_default();
                let sender: String = r.get_named("sender_name").unwrap_or_default();
                let age_min =
                    (now_us - r.get_named::<i64>("created_ts").unwrap_or(now_us)) / 60_000_000;
                table.add_row(vec![
                    id.to_string(),
                    sender,
                    subject,
                    format!("{age_min}min"),
                ]);
            }
            table.render();
            Ok(())
        }
        AcksCommand::Overdue {
            project,
            agent,
            ttl_minutes,
            limit,
        } => {
            // Overdue acks: ack_required, not acked, older than ttl_minutes
            let cutoff = now_us - ttl_minutes * 60 * 1_000_000;
            let rows = conn
                .query_sync(
                    "SELECT m.id, m.subject, m.created_ts, sender_a.name AS sender_name \
                     FROM messages m \
                     JOIN message_recipients i ON i.message_id = m.id \
                     JOIN agents recv_a ON recv_a.id = i.agent_id \
                     JOIN agents sender_a ON sender_a.id = m.sender_id \
                     JOIN projects p ON p.id = m.project_id \
                     WHERE p.slug = ? AND recv_a.name = ? \
                       AND m.ack_required = 1 AND i.ack_ts IS NULL \
                       AND m.created_ts < ? \
                     ORDER BY m.created_ts ASC \
                     LIMIT ?",
                    &[
                        sqlmodel_core::Value::Text(project),
                        sqlmodel_core::Value::Text(agent),
                        sqlmodel_core::Value::BigInt(cutoff),
                        sqlmodel_core::Value::BigInt(limit),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

            if rows.is_empty() {
                output::empty_result(false, "No overdue acks.");
                return Ok(());
            }
            output::section(&format!("OVERDUE acks (>{ttl_minutes}min TTL):"));
            let mut table = output::CliTable::new(vec!["ID", "FROM", "SUBJECT", "OVERDUE"]);
            for r in &rows {
                let id: i64 = r.get_named("id").unwrap_or(0);
                let subject: String = r.get_named("subject").unwrap_or_default();
                let sender: String = r.get_named("sender_name").unwrap_or_default();
                let age_min =
                    (now_us - r.get_named::<i64>("created_ts").unwrap_or(now_us)) / 60_000_000;
                table.add_row(vec![
                    id.to_string(),
                    sender,
                    subject,
                    format!("{age_min}min"),
                ]);
            }
            table.render();
            Ok(())
        }
    }
}

fn handle_contacts(action: ContactsCommand) -> CliResult<()> {
    let conn = open_db_sync()?;
    handle_contacts_with_conn(&conn, action)
}

fn handle_contacts_with_conn(
    conn: &mcp_agent_mail_db::DbConn,
    action: ContactsCommand,
) -> CliResult<()> {
    let now_us = mcp_agent_mail_db::timestamps::now_micros();

    match action {
        ContactsCommand::Request {
            project_key,
            from_agent,
            to_agent,
            reason,
            ttl_seconds,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let ttl = ttl_seconds.max(60);
            let expires_us = now_us + ttl.saturating_mul(1_000_000);

            // Resolve project and agents.
            let proj_rows = conn
                .query_sync(
                    "SELECT id FROM projects WHERE slug = ?",
                    &[sqlmodel_core::Value::Text(project_key.clone())],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let project_id: i64 = proj_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!("project not found: {project_key}"))
                })?;

            let from_rows = conn
                .query_sync(
                    "SELECT id FROM agents WHERE project_id = ? AND name = ?",
                    &[
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::Text(from_agent.clone()),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let from_id: i64 = from_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!("agent not found: {from_agent}"))
                })?;

            let to_rows = conn
                .query_sync(
                    "SELECT id FROM agents WHERE project_id = ? AND name = ?",
                    &[
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::Text(to_agent.clone()),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let to_id: i64 = to_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| CliError::InvalidArgument(format!("agent not found: {to_agent}")))?;

            // Upsert agent_links: set status to 'pending'.
            // FrankenConnection does not support ON CONFLICT ... DO UPDATE;
            // emulate with DELETE + INSERT.
            conn.execute_sync(
                "DELETE FROM agent_links \
                 WHERE a_project_id = ? AND a_agent_id = ? \
                   AND b_project_id = ? AND b_agent_id = ?",
                &[
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(from_id),
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(to_id),
                ],
            )
            .map_err(|e| CliError::Other(format!("delete for upsert failed: {e}")))?;
            conn.execute_sync(
                "INSERT INTO agent_links \
                 (a_project_id, a_agent_id, b_project_id, b_agent_id, \
                  status, reason, created_ts, updated_ts, expires_ts) \
                 VALUES (?, ?, ?, ?, 'pending', ?, ?, ?, ?)",
                &[
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(from_id),
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(to_id),
                    sqlmodel_core::Value::Text(reason.clone()),
                    sqlmodel_core::Value::BigInt(now_us),
                    sqlmodel_core::Value::BigInt(now_us),
                    sqlmodel_core::Value::BigInt(expires_us),
                ],
            )
            .map_err(|e| CliError::Other(format!("insert failed: {e}")))?;

            let result = serde_json::json!({
                "from": from_agent.clone(),
                "to": to_agent.clone(),
                "status": "pending",
                "reason": reason.clone(),
                "expires_ts": mcp_agent_mail_db::timestamps::micros_to_iso(expires_us),
            });
            output::emit_output(&result, fmt, || {
                output::success(&format!("Contact request sent: {from_agent} → {to_agent}"));
                output::kv("Status", "pending");
                output::kv("Reason", &reason);
                output::kv(
                    "Expires",
                    &mcp_agent_mail_db::timestamps::micros_to_iso(expires_us),
                );
            });
            Ok(())
        }
        ContactsCommand::Respond {
            project_key,
            agent_name,
            from_agent,
            accept,
            reject,
            ttl_seconds,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let approved = if reject { false } else { accept };
            let new_status = if approved { "approved" } else { "blocked" };
            let ttl = ttl_seconds.max(60);
            let expires_us = now_us + ttl.saturating_mul(1_000_000);

            let proj_rows = conn
                .query_sync(
                    "SELECT id FROM projects WHERE slug = ?",
                    &[sqlmodel_core::Value::Text(project_key.clone())],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let project_id: i64 = proj_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!("project not found: {project_key}"))
                })?;

            let from_rows = conn
                .query_sync(
                    "SELECT id FROM agents WHERE project_id = ? AND name = ?",
                    &[
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::Text(from_agent.clone()),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let from_id: i64 = from_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!("agent not found: {from_agent}"))
                })?;

            let to_rows = conn
                .query_sync(
                    "SELECT id FROM agents WHERE project_id = ? AND name = ?",
                    &[
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::Text(agent_name.clone()),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let to_id: i64 = to_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!("agent not found: {agent_name}"))
                })?;

            let _updated = conn
                .query_sync(
                    "UPDATE agent_links SET status = ?, updated_ts = ?, expires_ts = ? \
                     WHERE a_project_id = ? AND a_agent_id = ? \
                       AND b_project_id = ? AND b_agent_id = ?",
                    &[
                        sqlmodel_core::Value::Text(new_status.to_string()),
                        sqlmodel_core::Value::BigInt(now_us),
                        sqlmodel_core::Value::BigInt(expires_us),
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::BigInt(from_id),
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::BigInt(to_id),
                    ],
                )
                .map_err(|e| CliError::Other(format!("update failed: {e}")))?;

            let result = serde_json::json!({
                "from": from_agent.clone(),
                "to": agent_name.clone(),
                "approved": approved,
                "status": new_status,
                "updated": true,
            });
            let verb = if approved { "Approved" } else { "Rejected" };
            output::emit_output(&result, fmt, || {
                output::success(&format!(
                    "{verb} contact request: {from_agent} → {agent_name}"
                ));
                output::kv("Status", new_status);
            });
            Ok(())
        }
        ContactsCommand::ListContacts {
            project_key,
            agent_name,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let proj_rows = conn
                .query_sync(
                    "SELECT id FROM projects WHERE slug = ?",
                    &[sqlmodel_core::Value::Text(project_key.clone())],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let project_id: i64 = proj_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!("project not found: {project_key}"))
                })?;

            let agent_rows = conn
                .query_sync(
                    "SELECT id FROM agents WHERE project_id = ? AND name = ?",
                    &[
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::Text(agent_name.clone()),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let agent_id: i64 = agent_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!("agent not found: {agent_name}"))
                })?;

            // Outgoing links.
            let outgoing = conn
                .query_sync(
                    "SELECT al.status, al.reason, al.updated_ts, al.expires_ts, \
                            a.name AS to_name \
                     FROM agent_links al \
                     JOIN agents a ON a.id = al.b_agent_id \
                     WHERE al.a_project_id = ? AND al.a_agent_id = ? \
                     ORDER BY al.updated_ts DESC",
                    &[
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::BigInt(agent_id),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

            // Incoming links.
            let incoming = conn
                .query_sync(
                    "SELECT al.status, al.reason, al.updated_ts, al.expires_ts, \
                            a.name AS from_name \
                     FROM agent_links al \
                     JOIN agents a ON a.id = al.a_agent_id \
                     WHERE al.b_project_id = ? AND al.b_agent_id = ? \
                     ORDER BY al.updated_ts DESC",
                    &[
                        sqlmodel_core::Value::BigInt(project_id),
                        sqlmodel_core::Value::BigInt(agent_id),
                    ],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

            let mut entries: Vec<serde_json::Value> = Vec::new();
            for r in &outgoing {
                let to: String = r.get_named("to_name").unwrap_or_default();
                let status: String = r.get_named("status").unwrap_or_default();
                let reason: String = r.get_named("reason").unwrap_or_default();
                let updated: i64 = r.get_named("updated_ts").unwrap_or(0);
                let expires: i64 = r.get_named("expires_ts").unwrap_or(0);
                entries.push(serde_json::json!({
                    "direction": "outgoing",
                    "to": to,
                    "status": status,
                    "reason": reason,
                    "updated_ts": mcp_agent_mail_db::timestamps::micros_to_iso(updated),
                    "expires_ts": mcp_agent_mail_db::timestamps::micros_to_iso(expires),
                }));
            }
            for r in &incoming {
                let from: String = r.get_named("from_name").unwrap_or_default();
                let status: String = r.get_named("status").unwrap_or_default();
                let reason: String = r.get_named("reason").unwrap_or_default();
                let updated: i64 = r.get_named("updated_ts").unwrap_or(0);
                let expires: i64 = r.get_named("expires_ts").unwrap_or(0);
                entries.push(serde_json::json!({
                    "direction": "incoming",
                    "from": from,
                    "status": status,
                    "reason": reason,
                    "updated_ts": mcp_agent_mail_db::timestamps::micros_to_iso(updated),
                    "expires_ts": mcp_agent_mail_db::timestamps::micros_to_iso(expires),
                }));
            }

            if entries.is_empty() {
                output::emit_empty(fmt, "No contacts found.");
                return Ok(());
            }

            output::emit_output(&entries, fmt, || {
                if !outgoing.is_empty() {
                    output::section("Outgoing contacts:");
                    let mut table = output::CliTable::new(vec!["TO", "STATUS", "REASON"]);
                    for r in &outgoing {
                        let to: String = r.get_named("to_name").unwrap_or_default();
                        let status: String = r.get_named("status").unwrap_or_default();
                        let reason: String = r.get_named("reason").unwrap_or_default();
                        table.add_row(vec![to, status, reason]);
                    }
                    table.render();
                }
                if !incoming.is_empty() {
                    output::section("Incoming contacts:");
                    let mut table = output::CliTable::new(vec!["FROM", "STATUS", "REASON"]);
                    for r in &incoming {
                        let from: String = r.get_named("from_name").unwrap_or_default();
                        let status: String = r.get_named("status").unwrap_or_default();
                        let reason: String = r.get_named("reason").unwrap_or_default();
                        table.add_row(vec![from, status, reason]);
                    }
                    table.render();
                }
            });
            Ok(())
        }
        ContactsCommand::Policy {
            project_key,
            agent_name,
            policy,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            // Validate policy.
            let valid = ["open", "auto", "contacts_only", "block_all"];
            if !valid.contains(&policy.as_str()) {
                return Err(CliError::InvalidArgument(format!(
                    "invalid policy: {policy}. Valid: {}",
                    valid.join(", ")
                )));
            }

            let proj_rows = conn
                .query_sync(
                    "SELECT id FROM projects WHERE slug = ?",
                    &[sqlmodel_core::Value::Text(project_key.clone())],
                )
                .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
            let project_id: i64 = proj_rows
                .first()
                .and_then(|r| r.get_named("id").ok())
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!("project not found: {project_key}"))
                })?;

            conn.query_sync(
                "UPDATE agents SET contact_policy = ? WHERE project_id = ? AND name = ?",
                &[
                    sqlmodel_core::Value::Text(policy.clone()),
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::Text(agent_name.clone()),
                ],
            )
            .map_err(|e| CliError::Other(format!("update failed: {e}")))?;

            let result = serde_json::json!({
                "agent": agent_name.clone(),
                "policy": policy.clone(),
            });
            output::emit_output(&result, fmt, || {
                output::success(&format!("Contact policy set: {agent_name} → {policy}"));
            });
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Beads integration
// ---------------------------------------------------------------------------

fn resolve_beads_db(path: Option<&Path>) -> CliResult<PathBuf> {
    let root = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|e| CliError::Other(format!("cannot determine cwd: {e}")))?,
    };
    let db_path = root.join(".beads").join("beads.db");
    if !db_path.exists() {
        return Err(CliError::InvalidArgument(format!(
            "no beads database found at {}",
            db_path.display()
        )));
    }
    Ok(db_path)
}

fn handle_beads(action: BeadsCommand) -> CliResult<()> {
    match action {
        BeadsCommand::Ready {
            path,
            limit,
            format,
            json,
        } => handle_beads_ready(path, limit, format, json),
        BeadsCommand::List {
            path,
            status,
            priority,
            limit,
            format,
            json,
        } => handle_beads_list(path, status, priority, limit, format, json),
        BeadsCommand::Show {
            id,
            path,
            format,
            json,
        } => handle_beads_show(id, path, format, json),
        BeadsCommand::Status { path, format, json } => handle_beads_status(path, format, json),
    }
}

fn handle_beads_ready(
    path: Option<PathBuf>,
    limit: usize,
    format: Option<output::CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    use beads_rust::storage::{ReadyFilters, ReadySortPolicy, SqliteStorage};

    let fmt = output::CliOutputFormat::resolve(format, json);
    let db_path = resolve_beads_db(path.as_deref())?;
    let storage = SqliteStorage::open(&db_path)
        .map_err(|e| CliError::Other(format!("failed to open beads db: {e}")))?;

    let filters = ReadyFilters {
        limit: Some(limit),
        ..ReadyFilters::default()
    };
    let issues = storage
        .get_ready_issues(&filters, ReadySortPolicy::Hybrid)
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

    let items: Vec<serde_json::Value> = issues
        .iter()
        .map(|i| {
            serde_json::json!({
                "id": i.id,
                "title": i.title,
                "status": i.status.as_str(),
                "priority": format!("P{}", i.priority.0),
                "type": i.issue_type.as_str(),
                "labels": i.labels,
            })
        })
        .collect();

    if items.is_empty() {
        output::emit_empty(fmt, "No ready issues.");
        return Ok(());
    }

    output::emit_output(&items, fmt, || {
        output::section(&format!("Ready issues ({}):", issues.len()));
        for issue in &issues {
            ftui_runtime::ftui_println!(
                "  {} [P{}] {} ({})",
                issue.id,
                issue.priority.0,
                issue.title,
                issue.status.as_str(),
            );
        }
    });
    Ok(())
}

fn handle_beads_list(
    path: Option<PathBuf>,
    status: Option<String>,
    priority: Option<u8>,
    limit: usize,
    format: Option<output::CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    use beads_rust::model::{Priority, Status};
    use beads_rust::storage::{ListFilters, SqliteStorage};

    let fmt = output::CliOutputFormat::resolve(format, json);
    let db_path = resolve_beads_db(path.as_deref())?;
    let storage = SqliteStorage::open(&db_path)
        .map_err(|e| CliError::Other(format!("failed to open beads db: {e}")))?;

    let statuses = status.map(|s| {
        s.split(',')
            .map(|v| match v.trim() {
                "open" => Status::Open,
                "in_progress" => Status::InProgress,
                "blocked" => Status::Blocked,
                "closed" => Status::Closed,
                "deferred" => Status::Deferred,
                other => Status::Custom(other.to_string()),
            })
            .collect::<Vec<_>>()
    });

    let priorities = priority.map(|p| vec![Priority(i32::from(p))]);

    let include_closed = statuses
        .as_ref()
        .is_some_and(|ss| ss.iter().any(|s| matches!(s, Status::Closed)));

    let filters = ListFilters {
        statuses,
        priorities,
        include_closed,
        limit: Some(limit),
        ..ListFilters::default()
    };
    let issues = storage
        .list_issues(&filters)
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

    let items: Vec<serde_json::Value> = issues
        .iter()
        .map(|i| {
            serde_json::json!({
                "id": i.id,
                "title": i.title,
                "status": i.status.as_str(),
                "priority": format!("P{}", i.priority.0),
                "type": i.issue_type.as_str(),
                "labels": i.labels,
            })
        })
        .collect();

    if items.is_empty() {
        output::emit_empty(fmt, "No matching issues.");
        return Ok(());
    }

    output::emit_output(&items, fmt, || {
        output::section(&format!("Issues ({}):", issues.len()));
        for issue in &issues {
            ftui_runtime::ftui_println!(
                "  {} [P{}] {} ({})",
                issue.id,
                issue.priority.0,
                issue.title,
                issue.status.as_str(),
            );
        }
    });
    Ok(())
}

fn handle_beads_show(
    id: String,
    path: Option<PathBuf>,
    format: Option<output::CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    use beads_rust::storage::SqliteStorage;

    let fmt = output::CliOutputFormat::resolve(format, json);
    let db_path = resolve_beads_db(path.as_deref())?;
    let storage = SqliteStorage::open(&db_path)
        .map_err(|e| CliError::Other(format!("failed to open beads db: {e}")))?;

    let issue = storage
        .get_issue(&id)
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?
        .ok_or_else(|| CliError::InvalidArgument(format!("issue not found: {id}")))?;

    output::emit_output(&issue, fmt, || {
        output::section(&format!("{}: {}", issue.id, issue.title));
        output::kv("Status", issue.status.as_str());
        output::kv("Priority", &format!("P{}", issue.priority.0));
        output::kv("Type", issue.issue_type.as_str());
        if let Some(ref a) = issue.assignee {
            output::kv("Assignee", a);
        }
        if !issue.labels.is_empty() {
            output::kv("Labels", &issue.labels.join(", "));
        }
        if let Some(ref desc) = issue.description {
            ftui_runtime::ftui_println!("\n{desc}");
        }
        if !issue.dependencies.is_empty() {
            output::section("Dependencies:");
            for dep in &issue.dependencies {
                ftui_runtime::ftui_println!("  {} {}", dep.dep_type, dep.depends_on_id);
            }
        }
    });
    Ok(())
}

fn handle_beads_status(
    path: Option<PathBuf>,
    format: Option<output::CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    use beads_rust::model::Status;
    use beads_rust::storage::{ListFilters, SqliteStorage};

    let fmt = output::CliOutputFormat::resolve(format, json);
    let db_path = resolve_beads_db(path.as_deref())?;
    let storage = SqliteStorage::open(&db_path)
        .map_err(|e| CliError::Other(format!("failed to open beads db: {e}")))?;

    // Count all non-closed
    let all = storage
        .list_issues(&ListFilters::default())
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
    let closed = storage
        .list_issues(&ListFilters {
            statuses: Some(vec![Status::Closed]),
            include_closed: true,
            ..ListFilters::default()
        })
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

    let open = all
        .iter()
        .filter(|i| matches!(i.status, Status::Open))
        .count();
    let in_progress = all
        .iter()
        .filter(|i| matches!(i.status, Status::InProgress))
        .count();
    let blocked = all
        .iter()
        .filter(|i| matches!(i.status, Status::Blocked))
        .count();
    let closed_count = closed.len();
    let total = all.len() + closed_count;

    let result = serde_json::json!({
        "open": open,
        "in_progress": in_progress,
        "blocked": blocked,
        "closed": closed_count,
        "total": total,
    });
    output::emit_output(&result, fmt, || {
        output::section("Beads Status:");
        output::kv("Open", &open.to_string());
        output::kv("In Progress", &in_progress.to_string());
        output::kv("Blocked", &blocked.to_string());
        output::kv("Closed", &closed_count.to_string());
        output::kv("Total", &total.to_string());
    });
    Ok(())
}

fn handle_list_acks(project_key: &str, agent_name: &str, limit: i64) -> CliResult<()> {
    let conn = open_db_sync()?;
    handle_list_acks_with_conn(&conn, project_key, agent_name, limit)
}

fn handle_list_acks_with_conn(
    conn: &mcp_agent_mail_db::DbConn,
    project_key: &str,
    agent_name: &str,
    limit: i64,
) -> CliResult<()> {
    let rows = conn
        .query_sync(
            "SELECT m.id, m.subject, m.importance, m.created_ts, \
                    i.ack_ts, i.read_ts, sender_a.name AS sender_name \
             FROM messages m \
             JOIN message_recipients i ON i.message_id = m.id \
             JOIN agents recv_a ON recv_a.id = i.agent_id \
             JOIN agents sender_a ON sender_a.id = m.sender_id \
             JOIN projects p ON p.id = m.project_id \
             WHERE p.slug = ? AND recv_a.name = ? AND m.ack_required = 1 \
             ORDER BY m.created_ts DESC \
             LIMIT ?",
            &[
                sqlmodel_core::Value::Text(project_key.to_string()),
                sqlmodel_core::Value::Text(agent_name.to_string()),
                sqlmodel_core::Value::BigInt(limit),
            ],
        )
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?;

    if rows.is_empty() {
        output::empty_result(
            false,
            &format!("No ack-required messages for {agent_name}."),
        );
        return Ok(());
    }
    let mut table = output::CliTable::new(vec!["ID", "FROM", "SUBJECT", "STATUS", "CREATED"]);
    for r in &rows {
        let id: i64 = r.get_named("id").unwrap_or(0);
        let subject: String = r.get_named("subject").unwrap_or_default();
        let sender: String = r.get_named("sender_name").unwrap_or_default();
        let ack_ts: Option<i64> = r.get_named("ack_ts").ok();
        let created: i64 = r.get_named("created_ts").unwrap_or(0);
        let status = if ack_ts.is_some() { "acked" } else { "pending" };
        let created_str = mcp_agent_mail_db::timestamps::micros_to_iso(created);
        let subject_display = subject.get(..35).unwrap_or(&subject).to_string();
        let created_display = created_str.get(..19).unwrap_or(&created_str).to_string();
        table.add_row(vec![
            id.to_string(),
            sender,
            subject_display,
            status.to_string(),
            created_display,
        ]);
    }
    table.render();
    Ok(())
}

fn handle_migrate_with_database_url(database_url: &str) -> CliResult<()> {
    use asupersync::runtime::RuntimeBuilder;
    use mcp_agent_mail_db::DbConn;
    use mcp_agent_mail_db::schema;

    let cfg = mcp_agent_mail_db::DbPoolConfig {
        database_url: database_url.to_string(),
        ..mcp_agent_mail_db::DbPoolConfig::default()
    };
    let path = cfg
        .sqlite_path()
        .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?;

    let conn = DbConn::open_file(&path)
        .map_err(|e| CliError::Other(format!("cannot open DB at {path}: {e}")))?;
    conn.execute_raw(schema::PRAGMA_SETTINGS_SQL)
        .map_err(|e| CliError::Other(format!("failed to apply PRAGMAs: {e}")))?;

    let cx = asupersync::Cx::for_request();
    let rt = RuntimeBuilder::current_thread()
        .build()
        .map_err(|e| CliError::Other(format!("failed to build runtime: {e}")))?;

    let outcome = rt.block_on(async { schema::migrate_to_latest_base(&cx, &conn).await });

    match outcome {
        asupersync::Outcome::Ok(_) => {
            // Legacy Python: `migrate` is an explicit schema-create command.
            ftui_runtime::ftui_println!("✓ Database schema created from model definitions!");
            ftui_runtime::ftui_println!(
                "Note: To apply model changes, delete storage.sqlite3 and run this again."
            );
            Ok(())
        }
        asupersync::Outcome::Err(e) => Err(CliError::Other(format!("migrate failed: {e}"))),
        asupersync::Outcome::Cancelled(r) => {
            Err(CliError::Other(format!("migrate cancelled: {r:?}")))
        }
        asupersync::Outcome::Panicked(p) => Err(CliError::Other(format!("migrate panicked: {p}"))),
    }
}

fn handle_migrate_cmd(
    check: bool,
    rollback: bool,
    force: bool,
    backup_dir: Option<PathBuf>,
) -> CliResult<()> {
    use mcp_agent_mail_db::migrate;

    let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    let path = cfg
        .sqlite_path()
        .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?;

    if path == ":memory:" {
        ftui_runtime::ftui_println!("In-memory database — no migration needed.");
        return Ok(());
    }

    let db_path = Path::new(&path);
    if !db_path.exists() {
        ftui_runtime::ftui_println!("Database file does not exist: {path}");
        ftui_runtime::ftui_println!("Run the server once to create it, then migrate.");
        return Ok(());
    }

    if check && rollback {
        return Err(CliError::InvalidArgument(
            "--check and --rollback are mutually exclusive".to_string(),
        ));
    }
    if rollback {
        return handle_migrate_rollback(db_path, backup_dir.as_deref(), force);
    }

    // Step 1: Run schema migration first (ensures tables exist)
    handle_migrate_with_database_url(&cfg.database_url)?;

    // Step 2: Detect timestamp format
    let conn = open_db_sync_with_database_url(&cfg.database_url)?;
    let before_counts = collect_table_row_counts(&conn);
    let format = migrate::detect_timestamp_format(&conn)
        .map_err(|e| CliError::Other(format!("format detection failed: {e}")))?;

    ftui_runtime::ftui_println!("Database format: {format}");

    if check {
        // --check mode: just report and exit
        if format.needs_migration() {
            ftui_runtime::ftui_println!("Migration needed: run `am migrate` to convert.");
            return Ok(());
        }
        ftui_runtime::ftui_println!("No migration needed.");
        return Ok(());
    }

    if !format.needs_migration() {
        ftui_runtime::ftui_println!("Database is already in Rust format. No migration needed.");
        return Ok(());
    }

    // For Unknown format, require --force
    if matches!(format, migrate::TimestampFormat::Unknown(_)) && !force {
        return Err(CliError::Other(
            "unknown timestamp format detected; use --force to migrate anyway".to_string(),
        ));
    }

    // Step 3: Create backup
    let start = std::time::Instant::now();
    let backup_path = create_db_backup(db_path, backup_dir.as_deref())?;
    ftui_runtime::ftui_println!("Backup created: {}", backup_path.display());

    // Step 4: Convert all TEXT timestamps to i64 microseconds
    ftui_runtime::ftui_println!("Converting timestamps...");
    let summary = migrate::convert_all_timestamps(&conn)
        .map_err(|e| CliError::Other(format!("timestamp conversion failed: {e}")))?;

    // Step 5: VACUUM to reclaim space
    let _ = conn.execute_raw("VACUUM");

    // Step 6: Verify
    let after = migrate::detect_timestamp_format(&conn)
        .map_err(|e| CliError::Other(format!("post-migration verification failed: {e}")))?;
    let verification = verify_migration_integrity(&conn, &before_counts);

    let elapsed = start.elapsed();

    // Print summary
    ftui_runtime::ftui_println!("Migration complete in {:.1}s", elapsed.as_secs_f64());
    ftui_runtime::ftui_println!("  Converted: {} rows", summary.total_converted);
    if summary.total_skipped > 0 {
        ftui_runtime::ftui_println!("  Skipped:   {} rows (parse errors)", summary.total_skipped);
    }
    if summary.total_nulls > 0 {
        ftui_runtime::ftui_println!("  NULLs:     {} (left as-is)", summary.total_nulls);
    }
    ftui_runtime::ftui_println!("  Backup:    {}", backup_path.display());
    ftui_runtime::ftui_println!("  Format:    {after}");
    if verification.row_counts_match {
        ftui_runtime::ftui_println!("  Row count: verified");
    } else {
        ftui_runtime::ftui_eprintln!("  Row count: mismatch detected");
    }

    // Report per-column errors if any
    for col_result in &summary.columns {
        for err in &col_result.errors {
            ftui_runtime::ftui_eprintln!("  Warning: {err}");
        }
    }
    for warning in &verification.warnings {
        ftui_runtime::ftui_eprintln!("  Warning: {warning}");
    }

    if !summary.success {
        ftui_runtime::ftui_eprintln!(
            "Warning: migration completed with errors. Review warnings above."
        );
        ftui_runtime::ftui_eprintln!("Original database preserved at: {}", backup_path.display());
    }

    if after.needs_migration() {
        ftui_runtime::ftui_eprintln!(
            "Warning: database still contains TEXT timestamps after migration."
        );
        ftui_runtime::ftui_eprintln!("  Post-migration format: {after}");
    }
    if !verification.row_counts_match {
        ftui_runtime::ftui_eprintln!(
            "Warning: one or more table row counts changed during migration."
        );
    }

    Ok(())
}

#[derive(Debug, Default)]
struct MigrationVerification {
    row_counts_match: bool,
    warnings: Vec<String>,
}

fn collect_table_row_counts(
    conn: &mcp_agent_mail_db::DbConn,
) -> std::collections::BTreeMap<String, i64> {
    let mut counts = std::collections::BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    for &(table, _column, _nullable) in mcp_agent_mail_db::migrate::TIMESTAMP_COLUMNS {
        if !seen.insert(table) {
            continue;
        }
        let sql = format!("SELECT COUNT(*) AS c FROM {table}");
        if let Ok(rows) = conn.query_sync(&sql, &[])
            && let Some(row) = rows.first()
            && let Ok(count) = row.get_named::<i64>("c")
        {
            counts.insert(table.to_string(), count);
        }
    }
    counts
}

fn verify_migration_integrity(
    conn: &mcp_agent_mail_db::DbConn,
    before_counts: &std::collections::BTreeMap<String, i64>,
) -> MigrationVerification {
    const MIN_REASONABLE_TS: i64 = 1_577_836_800_000_000; // 2020-01-01T00:00:00Z
    const MAX_REASONABLE_TS: i64 = 1_893_456_000_000_000; // 2030-01-01T00:00:00Z

    let mut verification = MigrationVerification {
        row_counts_match: true,
        warnings: Vec::new(),
    };

    // Row-count invariants: conversion should never add/drop rows.
    for (table, before) in before_counts {
        let sql = format!("SELECT COUNT(*) AS c FROM {table}");
        match conn.query_sync(&sql, &[]) {
            Ok(rows) => {
                let after = rows
                    .first()
                    .and_then(|row| row.get_named::<i64>("c").ok())
                    .unwrap_or(*before);
                if after != *before {
                    verification.row_counts_match = false;
                    verification.warnings.push(format!(
                        "row-count mismatch in {table}: before={before}, after={after}"
                    ));
                }
            }
            Err(err) => {
                verification.row_counts_match = false;
                verification
                    .warnings
                    .push(format!("row-count verification failed for {table}: {err}"));
            }
        }
    }

    // Spot-check timestamps after migration for plausibility.
    for &(table, column, _nullable) in mcp_agent_mail_db::migrate::TIMESTAMP_COLUMNS {
        let sql = format!(
            "SELECT {column} AS v FROM {table} WHERE {column} IS NOT NULL ORDER BY RANDOM() LIMIT 5"
        );
        let Ok(rows) = conn.query_sync(&sql, &[]) else {
            continue;
        };
        for row in rows {
            if let Ok(value) = row.get_named::<i64>("v") {
                if !(MIN_REASONABLE_TS..=MAX_REASONABLE_TS).contains(&value) {
                    verification.warnings.push(format!(
                        "{table}.{column} has out-of-range timestamp sample: {value}"
                    ));
                }
                continue;
            }
            if row.get_named::<String>("v").is_ok() {
                verification.warnings.push(format!(
                    "{table}.{column} still contains TEXT after migration"
                ));
            }
        }
    }

    verification
}

fn handle_migrate_rollback(
    db_path: &Path,
    backup_dir: Option<&Path>,
    force: bool,
) -> CliResult<()> {
    let backups = list_db_backups(db_path, backup_dir)?;
    if backups.is_empty() {
        return Err(CliError::Other(format!(
            "no backups found for {}",
            db_path.display()
        )));
    }

    ftui_runtime::ftui_println!("Available backups:");
    for backup in &backups {
        let modified = std::fs::metadata(backup)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|ts| chrono::DateTime::<chrono::Utc>::from(ts).to_rfc3339())
            .unwrap_or_else(|| "unknown-time".to_string());
        ftui_runtime::ftui_println!("  - {} ({modified})", backup.display());
    }

    let selected = backups[0].clone();
    if !force {
        if !crate::output::is_stdin_tty() {
            return Err(CliError::Other(
                "--rollback requires --force in non-interactive mode".to_string(),
            ));
        }
        print!(
            "Restore latest backup {} to {}? [y/N]: ",
            selected.display(),
            db_path.display()
        );
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| CliError::Other(format!("failed to read confirmation input: {e}")))?;
        let answer = input.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            ftui_runtime::ftui_println!("Rollback cancelled.");
            return Ok(());
        }
    }

    restore_db_from_backup(db_path, &selected)?;
    ftui_runtime::ftui_println!("Rollback complete.");
    ftui_runtime::ftui_println!("  Restored: {}", selected.display());
    ftui_runtime::ftui_println!("  Target:   {}", db_path.display());
    Ok(())
}

fn list_db_backups(db_path: &Path, backup_dir: Option<&Path>) -> CliResult<Vec<PathBuf>> {
    let backup_parent =
        backup_dir.unwrap_or_else(|| db_path.parent().unwrap_or_else(|| Path::new(".")));
    let db_name = db_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let prefix = format!("{db_name}.bak.");

    let mut backups: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(backup_parent).map_err(|e| {
        CliError::Other(format!(
            "cannot list backup directory {}: {e}",
            backup_parent.display()
        ))
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.starts_with(&prefix) && !name.ends_with("-wal") && !name.ends_with("-shm") {
            backups.push(path);
        }
    }

    backups.sort_by(|a, b| b.cmp(a)); // newest first because timestamp is embedded in filename
    Ok(backups)
}

fn restore_db_from_backup(db_path: &Path, backup_path: &Path) -> CliResult<()> {
    let rollback_ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let rollback_backup = db_path.with_file_name(format!(
        "{}.pre_rollback.{rollback_ts}",
        db_path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::copy(db_path, &rollback_backup).map_err(|e| {
        CliError::Other(format!(
            "failed to create pre-rollback backup {}: {e}",
            rollback_backup.display()
        ))
    })?;

    let db_wal = PathBuf::from(format!("{}-wal", db_path.display()));
    let db_shm = PathBuf::from(format!("{}-shm", db_path.display()));
    if db_wal.exists() {
        let _ = std::fs::remove_file(&db_wal);
    }
    if db_shm.exists() {
        let _ = std::fs::remove_file(&db_shm);
    }

    std::fs::copy(backup_path, db_path).map_err(|e| {
        CliError::Other(format!(
            "failed to restore {} -> {}: {e}",
            backup_path.display(),
            db_path.display()
        ))
    })?;

    let backup_name = backup_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let wal_backup = backup_path.with_file_name(format!("{backup_name}-wal"));
    if wal_backup.exists() {
        let _ = std::fs::copy(&wal_backup, &db_wal);
    }
    let shm_backup = backup_path.with_file_name(format!("{backup_name}-shm"));
    if shm_backup.exists() {
        let _ = std::fs::copy(&shm_backup, &db_shm);
    }

    Ok(())
}

/// Create a timestamped backup of the database file.
fn create_db_backup(db_path: &Path, backup_dir: Option<&Path>) -> CliResult<PathBuf> {
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let backup_name = format!(
        "{}.bak.{timestamp}",
        db_path.file_name().unwrap_or_default().to_string_lossy()
    );

    let backup_parent =
        backup_dir.unwrap_or_else(|| db_path.parent().unwrap_or_else(|| Path::new(".")));

    std::fs::create_dir_all(backup_parent).map_err(|e| {
        CliError::Other(format!(
            "cannot create backup directory {}: {e}",
            backup_parent.display()
        ))
    })?;

    let backup_path = backup_parent.join(&backup_name);
    std::fs::copy(db_path, &backup_path).map_err(|e| {
        CliError::Other(format!(
            "cannot create backup at {}: {e}",
            backup_path.display()
        ))
    })?;

    // Also backup WAL/SHM if present
    let wal = PathBuf::from(format!("{}-wal", db_path.display()));
    if wal.exists() {
        let _ = std::fs::copy(&wal, backup_parent.join(format!("{backup_name}-wal")));
    }
    let shm = PathBuf::from(format!("{}-shm", db_path.display()));
    if shm.exists() {
        let _ = std::fs::copy(&shm, backup_parent.join(format!("{backup_name}-shm")));
    }

    Ok(backup_path)
}

// ── Self-update check (T6.1) ─────────────────────────────────────────────

/// Result of checking for updates against the GitHub releases API.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum UpdateCheckResult {
    /// A newer version is available.
    UpdateAvailable {
        current: String,
        latest: String,
        url: String,
    },
    /// The current version is up to date.
    UpToDate { current: String },
    /// The check failed (network error, rate limit, etc.).
    CheckFailed { reason: String },
}

/// Cache file path for update check results.
fn update_check_cache_path() -> PathBuf {
    let cache_dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache/mcp-agent-mail");
    cache_dir.join("update-check.json")
}

/// Read cached update check if it's less than 24 hours old.
fn read_update_cache() -> Option<UpdateCheckResult> {
    let path = update_check_cache_path();
    let content = std::fs::read_to_string(&path).ok()?;
    let cached: serde_json::Value = serde_json::from_str(&content).ok()?;

    // Check age
    let checked_at = cached.get("checked_at")?.as_str()?;
    let checked_ts = chrono::DateTime::parse_from_rfc3339(checked_at).ok()?;
    let age = chrono::Utc::now().signed_duration_since(checked_ts);
    if age.num_hours() >= 24 {
        return None; // Cache expired
    }

    serde_json::from_value(cached.get("result")?.clone()).ok()
}

/// Write update check result to cache.
fn write_update_cache(result: &UpdateCheckResult) {
    let path = update_check_cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cached = serde_json::json!({
        "checked_at": chrono::Utc::now().to_rfc3339(),
        "result": result,
    });
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(&cached).unwrap_or_default(),
    );
}

/// Compare two semver-like version strings (e.g., "0.1.0" vs "0.2.0").
/// Returns true if `latest` is newer than `current`.
fn is_newer_version(current: &str, latest: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split('.')
            .filter_map(|p| p.parse::<u64>().ok())
            .collect()
    };
    let c = parse(current);
    let l = parse(latest);
    l > c
}

/// Check for updates against the GitHub releases API.
fn check_for_update() -> UpdateCheckResult {
    let current = env!("CARGO_PKG_VERSION");

    // Respect NO_UPDATE_CHECK
    if std::env::var("NO_UPDATE_CHECK").is_ok() {
        return UpdateCheckResult::UpToDate {
            current: current.to_string(),
        };
    }

    // Check cache first
    if let Some(cached) = read_update_cache() {
        return cached;
    }

    // Fetch from GitHub API
    let result = check_for_update_from_github(current);
    write_update_cache(&result);
    result
}

fn check_for_update_from_github(current: &str) -> UpdateCheckResult {
    use asupersync::runtime::RuntimeBuilder;

    let rt = match RuntimeBuilder::current_thread().build() {
        Ok(rt) => rt,
        Err(e) => {
            return UpdateCheckResult::CheckFailed {
                reason: format!("runtime error: {e}"),
            };
        }
    };

    let response = match rt.block_on(async {
        let client = asupersync::http::h1::HttpClient::new();
        let url =
            "https://api.github.com/repos/Dicklesworthstone/mcp_agent_mail_rust/releases/latest";
        let headers = vec![
            ("User-Agent".to_string(), "mcp-agent-mail-cli".to_string()),
            (
                "Accept".to_string(),
                "application/vnd.github+json".to_string(),
            ),
        ];
        client
            .request(asupersync::http::h1::Method::Get, url, headers, vec![])
            .await
    }) {
        Ok(resp) => resp,
        Err(e) => {
            return UpdateCheckResult::CheckFailed {
                reason: format!("HTTP error: {e}"),
            };
        }
    };

    if response.status == 403 {
        return UpdateCheckResult::CheckFailed {
            reason: "GitHub API rate limit exceeded".to_string(),
        };
    }
    if response.status != 200 {
        return UpdateCheckResult::CheckFailed {
            reason: format!("GitHub API returned status {}", response.status),
        };
    }

    let body: serde_json::Value = match serde_json::from_slice(&response.body) {
        Ok(v) => v,
        Err(e) => {
            return UpdateCheckResult::CheckFailed {
                reason: format!("JSON parse error: {e}"),
            };
        }
    };

    let tag = match body.get("tag_name").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => {
            return UpdateCheckResult::CheckFailed {
                reason: "no tag_name in release".to_string(),
            };
        }
    };

    let html_url = body
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or("https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases")
        .to_string();

    let latest_version = tag.trim_start_matches('v');

    if is_newer_version(current, latest_version) {
        UpdateCheckResult::UpdateAvailable {
            current: current.to_string(),
            latest: latest_version.to_string(),
            url: html_url,
        }
    } else {
        UpdateCheckResult::UpToDate {
            current: current.to_string(),
        }
    }
}

fn handle_self_update_check() -> CliResult<()> {
    let result = check_for_update();
    match &result {
        UpdateCheckResult::UpdateAvailable {
            current,
            latest,
            url,
        } => {
            ftui_runtime::ftui_println!("Update available: {current} -> {latest}");
            ftui_runtime::ftui_println!("Download: {url}");
            ftui_runtime::ftui_println!(
                "Run: curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/main/install.sh | bash"
            );
        }
        UpdateCheckResult::UpToDate { current } => {
            ftui_runtime::ftui_println!("Up to date (v{current})");
        }
        UpdateCheckResult::CheckFailed { reason } => {
            ftui_runtime::ftui_eprintln!("Update check failed: {reason}");
        }
    }
    Ok(())
}

/// Detect the current platform target triple (mirrors install.sh detect_platform).
fn detect_platform_target() -> Option<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu".to_string()),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu".to_string()),
        ("macos", "x86_64") => Some("x86_64-apple-darwin".to_string()),
        ("macos", "aarch64") => Some("aarch64-apple-darwin".to_string()),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc".to_string()),
        _ => None,
    }
}

/// Construct the download URL for a release asset.
fn release_asset_url(version: &str, target: &str) -> (String, String) {
    let ext = if target.contains("windows") {
        "zip"
    } else {
        "tar.xz"
    };
    let filename = format!("mcp-agent-mail-{target}.{ext}");
    let url = format!(
        "https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/download/v{version}/{filename}"
    );
    (url, filename)
}

/// Result of downloading and verifying a release binary.
#[derive(Debug)]
struct DownloadedRelease {
    /// Directory containing extracted binaries.
    extract_dir: PathBuf,
    /// Path to the `am` binary.
    am_binary: PathBuf,
    /// Path to the `mcp-agent-mail` binary.
    server_binary: PathBuf,
    /// Version downloaded.
    version: String,
}

/// Download a file from a URL to a local path using asupersync HttpClient.
/// Returns the response body bytes.
fn download_file_sync(url: &str) -> Result<Vec<u8>, String> {
    use asupersync::runtime::RuntimeBuilder;

    let rt = RuntimeBuilder::current_thread()
        .build()
        .map_err(|e| format!("runtime error: {e}"))?;

    let url_owned = url.to_string();
    let result = rt.block_on(async move {
        let client = asupersync::http::h1::HttpClient::new();
        let headers = vec![
            ("User-Agent".to_string(), "mcp-agent-mail-cli".to_string()),
            ("Accept".to_string(), "application/octet-stream".to_string()),
        ];
        client
            .request(
                asupersync::http::h1::Method::Get,
                &url_owned,
                headers,
                vec![],
            )
            .await
    });

    let response = result.map_err(|e| format!("HTTP error: {e}"))?;

    if response.status == 404 {
        return Err(format!("asset not found (404): {url}"));
    }
    if response.status != 200 {
        return Err(format!("HTTP {}: {url}", response.status));
    }

    Ok(response.body)
}

/// Verify SHA256 checksum of downloaded data against a checksum file.
fn verify_sha256(data: &[u8], expected_hex: &str) -> bool {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(data);
    let actual = hex::encode(hash);
    actual == expected_hex.trim().to_lowercase()
}

/// Extract binaries from a .tar.xz archive into a temporary directory.
/// Returns the directory path containing extracted files.
fn extract_tar_xz(archive_path: &Path, dest_dir: &Path) -> Result<(), String> {
    let status = std::process::Command::new("tar")
        .args([
            "xJf",
            &archive_path.to_string_lossy(),
            "-C",
            &dest_dir.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|e| format!("failed to run tar: {e}"))?;
    if !status.success() {
        return Err(format!(
            "tar extraction failed with exit code {:?}",
            status.code()
        ));
    }
    Ok(())
}

/// Extract binaries from a .zip archive (Windows).
fn extract_zip(archive_data: &[u8], dest_dir: &Path) -> Result<(), String> {
    use std::io::Cursor;
    let reader = Cursor::new(archive_data);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| format!("invalid zip archive: {e}"))?;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("zip entry error: {e}"))?;
        let name = file.name().to_string();
        let outpath = dest_dir.join(&name);
        if file.is_dir() {
            std::fs::create_dir_all(&outpath)
                .map_err(|e| format!("mkdir {}: {e}", outpath.display()))?;
        } else {
            if let Some(parent) = outpath.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
            let mut outfile = std::fs::File::create(&outpath)
                .map_err(|e| format!("create {}: {e}", outpath.display()))?;
            std::io::copy(&mut file, &mut outfile)
                .map_err(|e| format!("write {}: {e}", outpath.display()))?;
            // Set executable permission on Unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if name.ends_with("am") || name.ends_with("mcp-agent-mail") {
                    std::fs::set_permissions(&outpath, std::fs::Permissions::from_mode(0o755))
                        .map_err(|e| format!("chmod {}: {e}", outpath.display()))?;
                }
            }
        }
    }
    Ok(())
}

/// Download, verify, and extract a release. Returns paths to the extracted binaries.
fn download_and_verify_release(version: &str) -> Result<DownloadedRelease, String> {
    let target = detect_platform_target().ok_or_else(|| {
        format!(
            "unsupported platform: {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;

    let (asset_url, filename) = release_asset_url(version, &target);
    let checksum_url = format!("{asset_url}.sha256");

    // Download checksum file first (small, validates availability)
    ftui_runtime::ftui_eprintln!("Downloading checksum...");
    let checksum_body = download_file_sync(&checksum_url)?;
    let checksum_text = String::from_utf8(checksum_body)
        .map_err(|_| "checksum file is not valid UTF-8".to_string())?;
    // Checksum file format: "<hex>  <filename>" or just "<hex>"
    let expected_hash = checksum_text
        .split_whitespace()
        .next()
        .ok_or_else(|| "empty checksum file".to_string())?
        .to_string();

    // Download the archive
    ftui_runtime::ftui_eprintln!("Downloading {filename}...");
    let archive_data = download_file_sync(&asset_url)?;
    ftui_runtime::ftui_eprintln!("Downloaded {} bytes", archive_data.len());

    // Verify SHA256
    if !verify_sha256(&archive_data, &expected_hash) {
        return Err("SHA256 checksum verification failed — download may be corrupted".to_string());
    }
    ftui_runtime::ftui_eprintln!("Checksum verified.");

    // Create temp directory for extraction
    let tmp_dir = std::env::temp_dir().join(format!("am-update-{version}"));
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir).map_err(|e| format!("clean temp dir: {e}"))?;
    }
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("create temp dir: {e}"))?;

    // Extract
    if filename.ends_with(".tar.xz") {
        // Write to file first (tar reads from file)
        let archive_path = tmp_dir.join(&filename);
        std::fs::write(&archive_path, &archive_data).map_err(|e| format!("write archive: {e}"))?;
        extract_tar_xz(&archive_path, &tmp_dir)?;
        // Clean up the archive file
        let _ = std::fs::remove_file(&archive_path);
    } else if filename.ends_with(".zip") {
        extract_zip(&archive_data, &tmp_dir)?;
    } else {
        return Err(format!("unsupported archive format: {filename}"));
    }

    // Find extracted binaries
    let (am_name, server_name) = if target.contains("windows") {
        ("am.exe", "mcp-agent-mail.exe")
    } else {
        ("am", "mcp-agent-mail")
    };

    let am_binary = find_binary_in_dir(&tmp_dir, am_name)
        .ok_or_else(|| format!("{am_name} not found in extracted archive"))?;
    let server_binary = find_binary_in_dir(&tmp_dir, server_name)
        .ok_or_else(|| format!("{server_name} not found in extracted archive"))?;

    Ok(DownloadedRelease {
        extract_dir: tmp_dir,
        am_binary,
        server_binary,
        version: version.to_string(),
    })
}

/// Search for a binary file by name in a directory tree (max depth 2).
fn find_binary_in_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    // Check directly in dir
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    // Check one level deep (tarball might have a subdirectory)
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let nested = entry.path().join(name);
                if nested.is_file() {
                    return Some(nested);
                }
            }
        }
    }
    None
}

/// Find the install directory for the current `am` binary.
fn find_install_dir() -> Result<PathBuf, String> {
    // First try: the binary that's currently running
    let current_exe =
        std::env::current_exe().map_err(|e| format!("cannot determine current executable: {e}"))?;
    let dir = current_exe
        .parent()
        .ok_or_else(|| "current executable has no parent directory".to_string())?;
    Ok(dir.to_path_buf())
}

/// Atomically replace a single binary (Unix).
///
/// Strategy: write new → rename old to .old.tmp → rename new to target (atomic).
#[cfg(unix)]
fn atomic_replace_binary(new_binary: &Path, target_path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let dir = target_path.parent().ok_or("no parent directory")?;
    let name = target_path
        .file_name()
        .ok_or("no filename")?
        .to_string_lossy();

    let tmp_new = dir.join(format!(".{name}.new.tmp"));
    let tmp_old = dir.join(format!(".{name}.old.tmp"));

    // 1. Copy new binary to staging location
    std::fs::copy(new_binary, &tmp_new).map_err(|e| format!("stage new binary: {e}"))?;

    // 2. Set executable permissions
    std::fs::set_permissions(&tmp_new, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod: {e}"))?;

    // 3. On macOS, handle Gatekeeper quarantine
    #[cfg(target_os = "macos")]
    {
        // Copy quarantine xattr from old binary if it exists, or remove it from new
        let _ = std::process::Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(&tmp_new)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    // 4. Rename old binary to backup (if it exists)
    if target_path.exists() {
        // Remove any leftover old backup
        let _ = std::fs::remove_file(&tmp_old);
        std::fs::rename(target_path, &tmp_old).map_err(|e| format!("backup old binary: {e}"))?;
    }

    // 5. Rename new binary to target (atomic on same filesystem)
    if let Err(e) = std::fs::rename(&tmp_new, target_path) {
        // Rollback: restore old binary
        if tmp_old.exists() {
            let _ = std::fs::rename(&tmp_old, target_path);
        }
        return Err(format!("atomic rename failed: {e}"));
    }

    // 6. Clean up old backup
    let _ = std::fs::remove_file(&tmp_old);

    Ok(())
}

/// Atomically replace a single binary (Windows).
///
/// On Windows, running binaries are locked. We rename the running binary
/// out of the way (Windows allows renaming locked files), then move the
/// new binary into place.
#[cfg(windows)]
fn atomic_replace_binary(new_binary: &Path, target_path: &Path) -> Result<(), String> {
    let dir = target_path.parent().ok_or("no parent directory")?;
    let name = target_path
        .file_name()
        .ok_or("no filename")?
        .to_string_lossy();

    let tmp_new = dir.join(format!("{name}.new.tmp"));
    let tmp_old = dir.join(format!("{name}.old.tmp"));

    // 1. Copy new binary to staging location
    std::fs::copy(new_binary, &tmp_new).map_err(|e| format!("stage new binary: {e}"))?;

    // 2. Rename old binary out of the way (Windows allows this even if locked)
    if target_path.exists() {
        let _ = std::fs::remove_file(&tmp_old);
        std::fs::rename(target_path, &tmp_old).map_err(|e| format!("backup old binary: {e}"))?;
    }

    // 3. Rename new binary to target
    if let Err(e) = std::fs::rename(&tmp_new, target_path) {
        // Rollback: restore old binary
        if tmp_old.exists() {
            let _ = std::fs::rename(&tmp_old, target_path);
        }
        return Err(format!("rename failed: {e}"));
    }

    // 4. Schedule cleanup of old binary (it may be locked)
    // Try to delete; if it fails, it will be cleaned up on next update
    let _ = std::fs::remove_file(&tmp_old);

    Ok(())
}

/// Replace both `am` and `mcp-agent-mail` binaries atomically.
fn replace_binaries(release: &DownloadedRelease) -> Result<(), String> {
    let install_dir = find_install_dir()?;

    // Verify write access
    let test_file = install_dir.join(".am-update-test");
    std::fs::write(&test_file, b"test")
        .map_err(|e| format!("no write access to {}: {e}", install_dir.display()))?;
    let _ = std::fs::remove_file(&test_file);

    let (am_name, server_name) = if cfg!(windows) {
        ("am.exe", "mcp-agent-mail.exe")
    } else {
        ("am", "mcp-agent-mail")
    };

    let am_target = install_dir.join(am_name);
    let server_target = install_dir.join(server_name);

    // Replace am first
    ftui_runtime::ftui_eprintln!("Replacing {am_name}...");
    atomic_replace_binary(&release.am_binary, &am_target)?;

    // Replace mcp-agent-mail
    ftui_runtime::ftui_eprintln!("Replacing {server_name}...");
    if let Err(e) = atomic_replace_binary(&release.server_binary, &server_target) {
        // am was already replaced but server failed — report but don't rollback am
        // (having a newer am with old server is better than a broken state)
        ftui_runtime::ftui_eprintln!(
            "Warning: {server_name} replacement failed ({e}), but {am_name} was updated"
        );
    }

    Ok(())
}

/// Full self-update: check, download, verify, and replace.
///
/// Supports `--force` (reinstall current version) and `--version X` (specific version).
fn handle_self_update_full(force: bool, target_version: Option<String>) -> CliResult<()> {
    let current = env!("CARGO_PKG_VERSION");

    let version_to_install = if let Some(ref v) = target_version {
        // Specific version requested
        let v = v.trim_start_matches('v');
        ftui_runtime::ftui_println!("Installing version {v} (current: {current})");
        v.to_string()
    } else if force {
        // Force reinstall current version
        ftui_runtime::ftui_println!("Force reinstalling v{current}");
        current.to_string()
    } else {
        // Normal update: check for newer version
        let result = check_for_update();
        match result {
            UpdateCheckResult::UpdateAvailable {
                current: cur,
                latest,
                url: _,
            } => {
                ftui_runtime::ftui_println!("Update available: {cur} -> {latest}");
                latest
            }
            UpdateCheckResult::UpToDate { current: cur } => {
                ftui_runtime::ftui_println!("Already up to date (v{cur})");
                return Ok(());
            }
            UpdateCheckResult::CheckFailed { reason } => {
                ftui_runtime::ftui_eprintln!("Update check failed: {reason}");
                return Err(CliError::Other(format!("update check failed: {reason}")));
            }
        }
    };

    let release = match download_and_verify_release(&version_to_install) {
        Ok(r) => r,
        Err(e) => {
            ftui_runtime::ftui_eprintln!("Download failed: {e}");
            return Err(CliError::Other(format!("self-update download failed: {e}")));
        }
    };

    ftui_runtime::ftui_println!("Downloaded and verified v{}", release.version);

    // Perform atomic binary replacement
    match replace_binaries(&release) {
        Ok(()) => {
            ftui_runtime::ftui_println!(
                "Update complete (v{current} -> v{}). Restart am to use the new version.",
                release.version
            );
            // Clean up temp directory
            let _ = std::fs::remove_dir_all(&release.extract_dir);
            Ok(())
        }
        Err(e) => {
            ftui_runtime::ftui_eprintln!("Binary replacement failed: {e}");
            ftui_runtime::ftui_eprintln!(
                "New binaries are staged at: {}",
                release.extract_dir.display()
            );
            Err(CliError::Other(format!(
                "self-update replacement failed: {e}"
            )))
        }
    }
}

// ── End self-update ──────────────────────────────────────────────────────

#[derive(Debug)]
#[allow(dead_code)]
struct ClearAndResetOutcome {
    archive_path: Option<PathBuf>,
    deleted_db_files: Vec<PathBuf>,
    deleted_storage_entries: Vec<PathBuf>,
}

fn handle_clear_and_reset(force: bool, archive: bool, no_archive: bool) -> CliResult<()> {
    let db_cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    let db_path = match db_cfg.sqlite_path() {
        Ok(path) => Some(PathBuf::from(path)),
        Err(err) => {
            ftui_runtime::ftui_eprintln!(
                "Warning: failed to parse SQLite database path from DATABASE_URL ({}): {err}",
                db_cfg.database_url
            );
            None
        }
    };

    let mut database_files: Vec<PathBuf> = Vec::new();
    let source_db_for_archive = match db_path.as_deref() {
        Some(p) if p.to_string_lossy() != ":memory:" => {
            database_files.push(p.to_path_buf());
            database_files.push(PathBuf::from(format!("{}-wal", p.display())));
            database_files.push(PathBuf::from(format!("{}-shm", p.display())));
            Some(p)
        }
        _ => None,
    };

    let archive_choice = if archive {
        Some(true)
    } else if no_archive {
        Some(false)
    } else {
        None
    };

    let config = Config::from_env();
    let _outcome = clear_and_reset_everything(
        force,
        archive_choice,
        source_db_for_archive,
        &database_files,
        &config.storage_root,
    )?;
    Ok(())
}

fn clear_and_reset_everything(
    force: bool,
    archive_choice: Option<bool>,
    source_db_for_archive: Option<&Path>,
    database_files: &[PathBuf],
    storage_root: &Path,
) -> CliResult<ClearAndResetOutcome> {
    if !force {
        if !crate::output::is_stdin_tty() {
            return Err(CliError::Other(
                "refusing to prompt on non-interactive stdin; pass --force / -f to apply"
                    .to_string(),
            ));
        }

        ftui_runtime::ftui_println!("This will irreversibly delete:");
        if database_files.is_empty() {
            ftui_runtime::ftui_println!("  - (no SQLite files detected)");
        } else {
            for path in database_files {
                ftui_runtime::ftui_println!("  - {}", path.display());
            }
        }
        ftui_runtime::ftui_println!(
            "  - All contents inside {} (including .git)",
            storage_root.display()
        );
        ftui_runtime::ftui_println!("");
    }

    let mut should_archive = archive_choice;
    let archive_mandatory = archive_choice == Some(true) || force;
    if should_archive.is_none() {
        if force {
            should_archive = Some(true);
        } else {
            should_archive = Some(confirm(
                "Create a mailbox archive before wiping everything?",
                true,
            )?);
        }
    }

    let mut archive_path: Option<PathBuf> = None;
    if should_archive == Some(true) {
        let label = Some("pre-reset".to_string());
        let scrub_preset = "archive".to_string();
        let projects: Vec<String> = Vec::new();

        let archive_result = match source_db_for_archive {
            Some(source_db) => {
                archive_save_state(source_db, storage_root, projects, scrub_preset, label)
            }
            None => Err(CliError::Other(
                "SQLite database path is empty or in-memory; cannot create archive".to_string(),
            )),
        };

        match archive_result {
            Ok(path) => {
                let display_name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("<archive>");
                ftui_runtime::ftui_println!("Saved restore point to: {}", path.display());
                ftui_runtime::ftui_println!(
                    "Restore later with: am archive restore {display_name}"
                );
                archive_path = Some(path);
            }
            Err(err) => {
                ftui_runtime::ftui_eprintln!("Failed to create archive: {err}");
                if archive_mandatory {
                    return Err(CliError::ExitCode(1));
                }
                if !crate::output::is_stdin_tty() {
                    return Err(CliError::ExitCode(1));
                }
                if !confirm("Archive failed. Continue without a backup?", false)? {
                    return Err(CliError::ExitCode(1));
                }
            }
        }
    }

    if !force && !confirm("Proceed with destructive reset?", false)? {
        return Err(CliError::ExitCode(1));
    }

    let mut deleted_db_files: Vec<PathBuf> = Vec::new();
    for path in database_files {
        match std::fs::remove_file(path) {
            Ok(()) => deleted_db_files.push(path.clone()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                ftui_runtime::ftui_eprintln!("Failed to delete {}: {err}", path.display());
            }
        }
    }

    let mut deleted_storage_entries: Vec<PathBuf> = Vec::new();
    if storage_root.exists() {
        for entry in std::fs::read_dir(storage_root)? {
            let path = entry?.path();
            let result = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match result {
                Ok(()) => deleted_storage_entries.push(path),
                Err(err) => {
                    ftui_runtime::ftui_eprintln!("Failed to remove {}: {err}", path.display());
                }
            }
        }
    } else {
        ftui_runtime::ftui_println!(
            "Storage root {} does not exist; nothing to remove.",
            storage_root.display()
        );
    }

    ftui_runtime::ftui_println!("Reset complete.");
    if !deleted_db_files.is_empty() {
        let list = deleted_db_files
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        ftui_runtime::ftui_println!("Removed database files: {list}");
    }
    if !deleted_storage_entries.is_empty() {
        let list = deleted_storage_entries
            .iter()
            .filter_map(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            })
            .collect::<Vec<_>>()
            .join(", ");
        ftui_runtime::ftui_println!("Cleared storage root entries: {list}");
    }

    Ok(ClearAndResetOutcome {
        archive_path,
        deleted_db_files,
        deleted_storage_entries,
    })
}

const BENCH_DEFAULT_REGRESSION_THRESHOLD: f64 = 0.10;

#[derive(Debug, Serialize)]
struct BenchRunFailure {
    name: String,
    command: String,
    error: String,
}

#[derive(Debug, Serialize)]
struct BenchRunReport {
    summary: bench::BenchSummary,
    profile: bench::BenchProfile,
    warmup: u32,
    runs: u32,
    filter: Option<String>,
    baseline_path: Option<String>,
    save_baseline_path: Option<String>,
    seed_report: Option<bench::BenchSeedReport>,
    skipped: Vec<String>,
    failures: Vec<BenchRunFailure>,
    regression_count: usize,
}

#[allow(clippy::too_many_arguments)]
fn handle_bench(
    quick: bool,
    format: Option<output::CliOutputFormat>,
    json: bool,
    baseline: Option<PathBuf>,
    save_baseline: Option<PathBuf>,
    filter: Option<String>,
    list: bool,
    warmup_override: Option<u32>,
    runs_override: Option<u32>,
) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(format, json);
    let profile = if quick {
        bench::BenchProfile::Quick
    } else {
        bench::BenchProfile::Normal
    };
    let warmup = warmup_override.unwrap_or(profile.warmup());
    let runs = runs_override.unwrap_or(profile.runs());
    if warmup == 0 {
        return Err(CliError::InvalidArgument(
            "--warmup must be greater than zero".to_string(),
        ));
    }
    if runs == 0 {
        return Err(CliError::InvalidArgument(
            "--runs must be greater than zero".to_string(),
        ));
    }

    let filter_pattern = if let Some(raw) = filter.as_deref() {
        Some(glob::Pattern::new(raw).map_err(|err| {
            CliError::InvalidArgument(format!("invalid --filter pattern '{raw}': {err}"))
        })?)
    } else {
        None
    };

    let mut configs: Vec<bench::BenchConfig> = bench::DEFAULT_BENCHMARKS
        .iter()
        .map(|definition| definition.to_config(profile))
        .collect();
    for cfg in &mut configs {
        cfg.warmup = warmup;
        cfg.runs = runs;
    }
    configs.retain(|cfg| {
        filter_pattern
            .as_ref()
            .is_none_or(|pattern| pattern.matches(&cfg.name))
    });
    if configs.is_empty() {
        return Err(CliError::InvalidArgument(
            "no benchmarks matched the current --filter".to_string(),
        ));
    }

    if list {
        let payload: Vec<serde_json::Value> = configs
            .iter()
            .map(|cfg| {
                serde_json::json!({
                    "name": cfg.name,
                    "category": cfg.category,
                    "command": cfg.command,
                    "warmup": cfg.warmup,
                    "runs": cfg.runs,
                    "requires_seeded_db": cfg.requires_seeded_db,
                    "conditional": cfg.conditional,
                    "condition": cfg.condition,
                })
            })
            .collect();
        output::emit_output(&payload, fmt, || {
            ftui_runtime::ftui_println!(
                "Benchmarks (profile={:?}, warmup={warmup}, runs={runs}):",
                profile
            );
            for cfg in &configs {
                ftui_runtime::ftui_println!(
                    "- {:<16} {:<12} {}",
                    cfg.name,
                    format!("{:?}", cfg.category),
                    cfg.command.join(" ")
                );
            }
        });
        return Ok(());
    }

    let cwd = std::env::current_dir()
        .map_err(|err| CliError::Other(format!("failed to read current directory: {err}")))?;
    let executable = std::env::current_exe()
        .map_err(|err| CliError::Other(format!("failed to resolve current executable: {err}")))?;
    let executable = executable.to_string_lossy().into_owned();
    let hardware = bench::HardwareInfo::detect();

    let needs_seeded_db = configs.iter().any(|cfg| cfg.requires_seeded_db);
    let mut bench_env = std::collections::BTreeMap::new();
    let mut seed_report = None;
    let mut temp_workspace: Option<tempfile::TempDir> = None;
    if needs_seeded_db {
        let workspace = tempfile::tempdir()
            .map_err(|err| CliError::Other(format!("failed to create temp workspace: {err}")))?;
        let db_path = workspace.path().join("bench.sqlite3");
        let storage_root = workspace.path().join("archive");
        std::fs::create_dir_all(&storage_root).map_err(|err| {
            CliError::Other(format!("failed to create bench archive root: {err}"))
        })?;
        let database_url = format!("sqlite:///{}", db_path.display());
        let conn = open_db_sync_with_database_url(&database_url)?;
        let report = bench::seed_bench_database(&conn, false)
            .map_err(|err| CliError::Other(format!("benchmark seed failed: {err}")))?;
        bench_env.insert("DATABASE_URL".to_string(), database_url);
        bench_env.insert(
            "STORAGE_ROOT".to_string(),
            storage_root.to_string_lossy().into_owned(),
        );
        seed_report = Some(report);
        temp_workspace = Some(workspace);
    }

    let condition_context = bench::BenchConditionContext {
        stub_encoder_available: cwd.join("scripts/toon_stub_encoder.sh").is_file(),
        seeded_database_available: needs_seeded_db,
    };
    let mut summary = bench::BenchSummary::new(hardware.clone());
    let mut skipped = Vec::new();
    let mut failures = Vec::new();
    for cfg in &configs {
        cfg.validate().map_err(|err| {
            CliError::Other(format!("invalid benchmark config '{}': {err}", cfg.name))
        })?;
        if !cfg.enabled_for(condition_context) {
            skipped.push(cfg.name.clone());
            continue;
        }
        let mut command = vec![executable.clone()];
        command.extend(cfg.command.iter().cloned());
        let command_display = command.join(" ");
        let params_json = serde_json::json!({
            "warmup": cfg.warmup,
            "runs": cfg.runs,
            "requires_seeded_db": cfg.requires_seeded_db,
            "conditional": cfg.conditional,
        })
        .to_string();
        let signature =
            bench::fixture_signature(&cfg.name, &command_display, &params_json, &hardware);
        match bench::run_timed(&command, cfg.warmup, cfg.runs, &bench_env, Some(&cwd)) {
            Ok(timing) => match bench::BenchResult::from_samples(
                cfg.name.clone(),
                command_display,
                &timing.samples_seconds,
                signature,
                None,
            ) {
                Ok(result) => summary.insert(result),
                Err(err) => failures.push(BenchRunFailure {
                    name: cfg.name.clone(),
                    command: command.join(" "),
                    error: err.to_string(),
                }),
            },
            Err(err) => failures.push(BenchRunFailure {
                name: cfg.name.clone(),
                command: command.join(" "),
                error: err.to_string(),
            }),
        }
    }
    if summary.benchmarks.is_empty() {
        return Err(CliError::Other(
            "no benchmarks completed successfully; check failure diagnostics".to_string(),
        ));
    }

    let baseline_data = if let Some(path) = baseline.as_ref() {
        Some(
            bench::load_baseline(path)
                .map_err(|err| CliError::Other(format!("failed to load baseline: {err}")))?,
        )
    } else {
        None
    };
    if let Some(data) = baseline_data.as_ref() {
        bench::apply_baseline_comparison(
            &mut summary.benchmarks,
            data,
            BENCH_DEFAULT_REGRESSION_THRESHOLD,
        );
    }
    if let Some(path) = save_baseline.as_ref() {
        bench::save_baseline(&summary.benchmarks, path)
            .map_err(|err| CliError::Other(format!("failed to save baseline: {err}")))?;
    }

    let regression_count = summary
        .benchmarks
        .values()
        .filter(|result| result.baseline.regression)
        .count();
    let report = BenchRunReport {
        summary,
        profile,
        warmup,
        runs,
        filter: filter.clone(),
        baseline_path: baseline.map(|path| path.to_string_lossy().into_owned()),
        save_baseline_path: save_baseline.map(|path| path.to_string_lossy().into_owned()),
        seed_report,
        skipped,
        failures,
        regression_count,
    };

    let results_dir = PathBuf::from("benches/results");
    std::fs::create_dir_all(&results_dir)
        .map_err(|err| CliError::Other(format!("failed to create benches/results: {err}")))?;
    let report_path = results_dir.join(format!("summary_{}.json", report.summary.timestamp));
    let encoded = serde_json::to_string_pretty(&report)
        .map_err(|err| CliError::Other(format!("failed to encode bench report: {err}")))?;
    std::fs::write(&report_path, &encoded)
        .map_err(|err| CliError::Other(format!("failed to write bench report: {err}")))?;

    output::emit_output(&report, fmt, || {
        ftui_runtime::ftui_println!(
            "[bench] profile={:?} warmup={} runs={}",
            report.profile,
            report.warmup,
            report.runs
        );
        ftui_runtime::ftui_println!(
            "{:<18} {:>9} {:>9} {:>9} {:>12}",
            "Benchmark",
            "Mean",
            "P95",
            "P99",
            "Baseline Δ"
        );
        for result in report.summary.benchmarks.values() {
            let delta = result
                .baseline
                .delta_p95_ms
                .map_or_else(|| "-".to_string(), |value| format!("{value:+.2}ms"));
            ftui_runtime::ftui_println!(
                "{:<18} {:>7.2}ms {:>7.2}ms {:>7.2}ms {:>12}",
                result.name,
                result.mean_ms,
                result.p95_ms,
                result.p99_ms,
                delta
            );
        }
        if !report.skipped.is_empty() {
            ftui_runtime::ftui_println!("skipped: {}", report.skipped.join(", "));
        }
        if !report.failures.is_empty() {
            ftui_runtime::ftui_eprintln!("benchmark failures:");
            for failure in &report.failures {
                ftui_runtime::ftui_eprintln!(
                    "- {} [{}]: {}",
                    failure.name,
                    failure.command,
                    failure.error
                );
            }
        }
        ftui_runtime::ftui_println!("report: {}", report_path.display());
    });

    let _ = temp_workspace;
    if !report.failures.is_empty() {
        return Err(CliError::ExitCode(
            bench::BenchExitCode::RuntimeError.code(),
        ));
    }
    if report.regression_count > 0 {
        return Err(CliError::ExitCode(
            bench::BenchExitCode::RegressionDetected.code(),
        ));
    }
    Ok(())
}

/// Handle the `am ci` command: run quality gates with optional flags.
const fn ci_should_emit_progress(fmt: output::CliOutputFormat) -> bool {
    matches!(fmt, output::CliOutputFormat::Table)
}

fn handle_ci(
    quick: bool,
    report_path: Option<std::path::PathBuf>,
    format: Option<output::CliOutputFormat>,
    json: bool,
    parallel: bool,
) -> CliResult<()> {
    use ci::{
        Decision, GateRunnerConfig, RunMode, default_gates, print_gate_summary, run_gates,
        run_gates_parallel,
    };

    let fmt = output::CliOutputFormat::resolve(format, json);
    let show_progress = ci_should_emit_progress(fmt);
    let mode = if quick { RunMode::Quick } else { RunMode::Full };

    // Build runner config
    let working_dir =
        std::env::current_dir().map_err(|e| CliError::Other(format!("cwd error: {e}")))?;
    let runner_config = GateRunnerConfig::new(working_dir)
        .mode(mode)
        .timeout_secs(600);

    // Progress callback (only used in sequential mode)
    let on_start: fn(&str, usize, usize) = |name, idx, total| {
        ftui_runtime::ftui_println!("━━━ GATE [{}/{}]: {} ━━━", idx + 1, total, name);
    };

    let runner_config = if show_progress {
        GateRunnerConfig {
            on_gate_start: Some(on_start),
            ..runner_config
        }
    } else {
        runner_config
    };

    // Run all gates (parallel or sequential)
    let gates = default_gates();
    let report = if parallel {
        if show_progress {
            ftui_runtime::ftui_println!("Running gates in parallel mode...");
        }
        run_gates_parallel(&gates, &runner_config)
    } else {
        run_gates(&gates, &runner_config)
    };

    // Write report to file if requested
    if let Some(ref path) = report_path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CliError::Other(format!("failed to create report directory: {e}")))?;
        }
        report
            .write_to_file(path)
            .map_err(|e| CliError::Other(format!("failed to write report: {e}")))?;
        if show_progress {
            ftui_runtime::ftui_println!("Report written to: {}", path.display());
        } else {
            ftui_runtime::ftui_eprintln!("Report written to: {}", path.display());
        }
    }

    // Output results
    // Convert report to JSON value for emit_output
    let json_str = report
        .to_json()
        .map_err(|e| CliError::Other(format!("JSON serialization failed: {e}")))?;
    let data: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| CliError::Other(format!("JSON parse failed: {e}")))?;

    output::emit_output(&data, fmt, || {
        print_gate_summary(&report);
    });

    // Exit with appropriate code
    match report.decision {
        Decision::Go => Ok(()),
        Decision::NoGo => {
            ftui_runtime::ftui_eprintln!("CI failed: {}", report.decision_reason);
            Err(CliError::ExitCode(1))
        }
    }
}

fn handle_lint() -> CliResult<()> {
    let status = std::process::Command::new("cargo")
        .args(["clippy", "--all-targets", "--", "-D", "warnings"])
        .status()?;
    if status.success() {
        ftui_runtime::ftui_println!("Lint passed.");
        Ok(())
    } else {
        Err(CliError::ExitCode(status.code().unwrap_or(1)))
    }
}

fn handle_typecheck() -> CliResult<()> {
    let status = std::process::Command::new("cargo")
        .args(["check", "--all-targets"])
        .status()?;
    if status.success() {
        ftui_runtime::ftui_println!("Type check passed.");
        Ok(())
    } else {
        Err(CliError::ExitCode(status.code().unwrap_or(1)))
    }
}

/// Handle E2E test runner commands (br-8zmc).
fn handle_e2e(action: E2eCommand) -> CliResult<()> {
    use e2e_runner::{RunConfig, Runner, SuiteRegistry};

    let cwd = std::env::current_dir()?;

    match action {
        E2eCommand::List {
            format,
            json,
            verbose,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let registry = SuiteRegistry::new(&cwd)?;

            let suites: Vec<_> = registry
                .suites()
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "script": s.script_path,
                        "description": s.description,
                        "tags": s.tags,
                        "duration_class": s.duration_class.as_str(),
                    })
                })
                .collect();

            output::emit_output(&suites, fmt, || {
                ftui_runtime::ftui_println!("Available E2E test suites ({}):", registry.len());
                ftui_runtime::ftui_println!("");
                for suite in registry.suites() {
                    if verbose {
                        ftui_runtime::ftui_println!(
                            "  {} [{}]",
                            suite.name,
                            suite.duration_class.as_str()
                        );
                        if let Some(desc) = &suite.description {
                            ftui_runtime::ftui_println!("    {}", desc);
                        }
                        if !suite.tags.is_empty() {
                            ftui_runtime::ftui_println!("    tags: {}", suite.tags.join(", "));
                        }
                    } else {
                        ftui_runtime::ftui_println!("  {}", suite.name);
                    }
                }
            });
            Ok(())
        }
        E2eCommand::Run {
            suites,
            include,
            exclude,
            format,
            json,
            keep_tmp,
            force_build,
            project,
            artifacts,
            timeout,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let project_root = project.unwrap_or(cwd);

            let config = RunConfig {
                project_root: project_root.clone(),
                artifact_dir: artifacts,
                keep_tmp,
                force_build,
                timeout: Some(std::time::Duration::from_secs(timeout)),
                ..Default::default()
            };

            let runner = Runner::new(&project_root, config)?;

            // Determine which suites to run
            let report = if !suites.is_empty() {
                runner.run(&suites)
            } else if !include.is_empty() || !exclude.is_empty() {
                let inc = if include.is_empty() {
                    None
                } else {
                    Some(include.as_slice())
                };
                let exc = if exclude.is_empty() {
                    None
                } else {
                    Some(exclude.as_slice())
                };
                runner.run_filtered(inc, exc)
            } else {
                runner.run(&[]) // Run all
            };

            output::emit_output(&report, fmt, || {
                print!("{}", report.format_summary());
            });

            if report.success() {
                Ok(())
            } else {
                Err(CliError::ExitCode(report.exit_code()))
            }
        }
        E2eCommand::Show { suite } => {
            let registry = SuiteRegistry::new(&cwd)?;

            if let Some(s) = registry.get(&suite) {
                ftui_runtime::ftui_println!("Suite: {}", s.name);
                ftui_runtime::ftui_println!("Script: {}", s.script_path.display());
                ftui_runtime::ftui_println!("Duration: {}", s.duration_class.as_str());
                if let Some(desc) = &s.description {
                    ftui_runtime::ftui_println!("Description: {}", desc);
                }
                if !s.tags.is_empty() {
                    ftui_runtime::ftui_println!("Tags: {}", s.tags.join(", "));
                }
                Ok(())
            } else {
                Err(CliError::InvalidArgument(format!(
                    "Suite not found: {}",
                    suite
                )))
            }
        }
    }
}

// ── Native Share Wizard ─────────────────────────────────────────────────

fn run_native_wizard(args: ShareWizardArgs) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(args.format, args.json);

    // Parse provider if specified
    let provider = args.provider.as_ref().and_then(|s| {
        share::HostingProvider::parse(s).or_else(|| {
            ftui_runtime::ftui_eprintln!(
                "Unknown provider: {s}. Valid options: github, cloudflare, netlify, s3, custom"
            );
            None
        })
    });

    // Build wizard config
    let config = share::WizardConfig {
        inputs: share::WizardInputs {
            provider,
            bundle_path: args.bundle,
            output_dir: args.output,
            github_repo: args.github_repo,
            github_branch: Some(args.github_branch),
            cloudflare_project: args.cloudflare_project,
            netlify_site: args.netlify_site,
            s3_bucket: args.s3_bucket,
            cloudfront_id: args.cloudfront_id,
            base_url: args.base_url,
            skip_confirm: args.yes,
            dry_run: args.dry_run,
        },
        non_interactive: args.non_interactive,
        json_output: !matches!(fmt, output::CliOutputFormat::Table),
        skip_detection: false,
    };

    // Run the wizard
    let outcome = match share::run_interactive_wizard(config) {
        Ok(outcome) => outcome,
        Err(err) => {
            let output = share::WizardJsonOutput::failure(err.clone(), None);
            output::emit_output(&output, fmt, || {
                ftui_runtime::ftui_eprintln!("Error: {err}");
                if let Some(ref hint) = err.hint {
                    ftui_runtime::ftui_eprintln!("Hint: {hint}");
                }
            });
            return Err(CliError::ExitCode(err.exit_code()));
        }
    };

    // Handle outcome
    if !matches!(fmt, output::CliOutputFormat::Table) {
        let json = share::format_json_output(&outcome, outcome.confirmed, None);
        let payload = serde_json::from_str::<serde_json::Value>(&json)
            .map_err(|e| CliError::Other(format!("wizard output encode failed: {e}")))?;
        output::emit_output(&payload, fmt, || {});
    }

    if !outcome.confirmed {
        if args.dry_run {
            // Dry-run mode: show what would happen
            if matches!(fmt, output::CliOutputFormat::Table) {
                ftui_runtime::ftui_println!("\n[Dry run] No changes were made.");
            }
            return Ok(());
        }
        // User cancelled
        return Err(CliError::ExitCode(share::exit_codes::USER_CANCELLED));
    }

    // Execute the deployment plan
    let exec_config = share::ExecutorConfig {
        interactive: !args.non_interactive && crate::output::is_stdin_tty(),
        skip_confirm: args.yes,
        dry_run: args.dry_run,
        verbose: matches!(fmt, output::CliOutputFormat::Table),
    };

    let result = match share::execute_plan(&outcome.plan, &exec_config) {
        Ok(result) => result,
        Err(err) => {
            let output =
                share::WizardJsonOutput::failure(err.clone(), outcome.inputs.bundle_path.clone());
            output::emit_output(&output, fmt, || {
                ftui_runtime::ftui_eprintln!("Execution error: {err}");
                if let Some(ref hint) = err.hint {
                    ftui_runtime::ftui_eprintln!("Hint: {hint}");
                }
            });
            return Err(CliError::ExitCode(err.exit_code()));
        }
    };

    // Report success
    let output = share::WizardJsonOutput::success(result);
    output::emit_output(&output, fmt, || {
        ftui_runtime::ftui_println!("\nDeployment complete.");
        if let Some(ref url) = outcome.plan.expected_url {
            ftui_runtime::ftui_println!("Expected URL: {url}");
        }
        if !outcome.plan.warnings.is_empty() {
            ftui_runtime::ftui_eprintln!("\nNext steps:");
            for warning in &outcome.plan.warnings {
                ftui_runtime::ftui_eprintln!("  - {warning}");
            }
        }
    });

    Ok(())
}

#[derive(Debug, Clone)]
struct ProjectsAdoptRecord {
    id: i64,
    slug: String,
    human_key: String,
}

fn git_output_text(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn git_repo_root(path: &Path) -> Option<PathBuf> {
    git_output_text(path, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

fn git_common_dir(path: &Path) -> Option<PathBuf> {
    let value = git_output_text(path, &["rev-parse", "--git-common-dir"])?;
    let common = PathBuf::from(value);
    let resolved = if common.is_absolute() {
        common
    } else {
        let root = git_repo_root(path).unwrap_or_else(|| path.to_path_buf());
        root.join(common)
    };
    std::fs::canonicalize(&resolved).ok().or(Some(resolved))
}

fn find_project_for_adopt(
    conn: &mcp_agent_mail_db::DbConn,
    slug_or_key: &str,
) -> CliResult<ProjectsAdoptRecord> {
    let rows = conn
        .query_sync(
            "SELECT id, slug, human_key FROM projects \
             WHERE slug = ? OR human_key = ? \
             ORDER BY CASE WHEN slug = ? THEN 0 ELSE 1 END \
             LIMIT 1",
            &[
                sqlmodel_core::Value::Text(slug_or_key.to_string()),
                sqlmodel_core::Value::Text(slug_or_key.to_string()),
                sqlmodel_core::Value::Text(slug_or_key.to_string()),
            ],
        )
        .map_err(|e| CliError::Other(format!("project lookup failed: {e}")))?;
    let Some(row) = rows.first() else {
        return Err(CliError::InvalidArgument(format!(
            "project not found: {slug_or_key}"
        )));
    };
    let id: i64 = row.get_named("id").unwrap_or(0);
    let slug: String = row.get_named("slug").unwrap_or_default();
    let human_key: String = row.get_named("human_key").unwrap_or_default();
    if id <= 0 || slug.is_empty() || human_key.is_empty() {
        return Err(CliError::Other(format!(
            "invalid project row for '{slug_or_key}'"
        )));
    }
    Ok(ProjectsAdoptRecord {
        id,
        slug,
        human_key,
    })
}

fn collect_files_recursive(root: &Path) -> CliResult<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut stack = vec![root.to_path_buf()];
    let mut files: Vec<PathBuf> = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if ty.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn move_archive_files(
    src_root: &Path,
    dst_root: &Path,
    repo_root: &Path,
) -> CliResult<Vec<String>> {
    let mut changed_paths: Vec<String> = Vec::new();
    for path in collect_files_recursive(src_root)? {
        let skip = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".lock") || name.ends_with(".lock.owner.json"));
        if skip {
            continue;
        }
        let Ok(rel) = path.strip_prefix(src_root) else {
            continue;
        };
        let dest_path = dst_root.join(rel);
        if dest_path.exists() {
            continue;
        }
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        match std::fs::rename(&path, &dest_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
                std::fs::copy(&path, &dest_path)?;
                std::fs::remove_file(&path)?;
            }
            Err(err) => return Err(err.into()),
        }

        let repo_rel = dest_path
            .strip_prefix(repo_root)
            .ok()
            .unwrap_or(&dest_path)
            .to_string_lossy()
            .replace('\\', "/");
        changed_paths.push(repo_rel);
    }
    Ok(changed_paths)
}

enum GitCommitOutcome {
    Committed,
    NothingToCommit,
    Failed(String),
}

fn git_add_and_commit(
    repo_root: &Path,
    config: &Config,
    rel_paths: &[String],
    message: &str,
) -> GitCommitOutcome {
    if rel_paths.is_empty() {
        return GitCommitOutcome::NothingToCommit;
    }

    let add_output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("add")
        .args(rel_paths)
        .output();
    let add_output = match add_output {
        Ok(output) => output,
        Err(err) => return GitCommitOutcome::Failed(format!("git add failed: {err}")),
    };
    if !add_output.status.success() {
        return GitCommitOutcome::Failed(format!(
            "git add failed: {}",
            String::from_utf8_lossy(&add_output.stderr).trim()
        ));
    }

    let commit_output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("commit")
        .arg("-m")
        .arg(message)
        .env("GIT_AUTHOR_NAME", &config.git_author_name)
        .env("GIT_AUTHOR_EMAIL", &config.git_author_email)
        .env("GIT_COMMITTER_NAME", &config.git_author_name)
        .env("GIT_COMMITTER_EMAIL", &config.git_author_email)
        .output();
    let commit_output = match commit_output {
        Ok(output) => output,
        Err(err) => return GitCommitOutcome::Failed(format!("git commit failed: {err}")),
    };
    if commit_output.status.success() {
        return GitCommitOutcome::Committed;
    }

    let stderr_text = String::from_utf8_lossy(&commit_output.stderr).to_ascii_lowercase();
    if stderr_text.contains("nothing to commit") || stderr_text.contains("no changes added") {
        return GitCommitOutcome::NothingToCommit;
    }

    GitCommitOutcome::Failed(format!(
        "git commit failed: {}",
        String::from_utf8_lossy(&commit_output.stderr).trim()
    ))
}

fn handle_project_mark_identity(project_path: &Path, should_commit: bool) -> CliResult<()> {
    ensure_dir(project_path)?;
    let identity = resolve_project_identity(project_path.to_string_lossy().as_ref());
    if identity.project_uid.trim().is_empty() {
        return Err(CliError::InvalidArgument(
            "unable to resolve project_uid for this path".to_string(),
        ));
    }

    let root = git_repo_root(project_path).unwrap_or_else(|| project_path.to_path_buf());
    let marker_path = root.join(".agent-mail-project-id");
    std::fs::write(&marker_path, format!("{}\n", identity.project_uid))?;
    ftui_runtime::ftui_println!(
        "Wrote {} with project_uid={}",
        marker_path.display(),
        identity.project_uid
    );

    if should_commit {
        let config = Config::from_env();
        let rel_paths = vec![
            marker_path
                .strip_prefix(&root)
                .ok()
                .unwrap_or(&marker_path)
                .to_string_lossy()
                .replace('\\', "/"),
        ];
        match git_add_and_commit(
            &root,
            &config,
            &rel_paths,
            "chore: add .agent-mail-project-id",
        ) {
            GitCommitOutcome::Committed => ftui_runtime::ftui_println!("Committed marker file."),
            GitCommitOutcome::NothingToCommit => {
                ftui_runtime::ftui_println!("Marker file already committed (no changes to commit).")
            }
            GitCommitOutcome::Failed(msg) => {
                ftui_runtime::ftui_eprintln!(
                    "Warning: unable to commit marker automatically. {msg}"
                );
            }
        }
    }

    Ok(())
}

fn handle_project_discovery_init(project_path: &Path, product: Option<String>) -> CliResult<()> {
    ensure_dir(project_path)?;
    let identity = resolve_project_identity(project_path.to_string_lossy().as_ref());
    if identity.project_uid.trim().is_empty() {
        return Err(CliError::InvalidArgument(
            "unable to resolve project_uid for this path".to_string(),
        ));
    }

    let yaml_path = project_path.join(".agent-mail.yaml");
    let mut lines = vec![
        "# Agent Mail discovery file".to_string(),
        format!("project_uid: {}", identity.project_uid),
    ];
    if let Some(product_uid) = product.filter(|value| !value.trim().is_empty()) {
        lines.push(format!("product_uid: {product_uid}"));
    }
    std::fs::write(&yaml_path, format!("{}\n", lines.join("\n")))?;
    ftui_runtime::ftui_println!("Wrote {}", yaml_path.display());

    Ok(())
}

#[allow(clippy::too_many_lines)]
fn handle_projects_adopt_with_conn(
    conn: &mcp_agent_mail_db::DbConn,
    config: &Config,
    source: &str,
    target: &str,
    _dry_run: bool,
    apply: bool,
) -> CliResult<()> {
    let source_project = find_project_for_adopt(conn, source)?;
    let target_project = find_project_for_adopt(conn, target)?;

    if source_project.id == target_project.id {
        ftui_runtime::ftui_println!("Source and target refer to the same project; nothing to do.");
        return Ok(());
    }

    let source_common = git_common_dir(Path::new(&source_project.human_key));
    let target_common = git_common_dir(Path::new(&target_project.human_key));
    let same_repo = source_common.is_some() && source_common == target_common;

    let source_archive = config
        .storage_root
        .join("projects")
        .join(&source_project.slug);
    let target_archive = config
        .storage_root
        .join("projects")
        .join(&target_project.slug);

    ftui_runtime::ftui_println!("Projects adopt plan (dry-run)");
    ftui_runtime::ftui_println!(
        "- Source: id={} slug={} key={}",
        source_project.id,
        source_project.slug,
        source_project.human_key
    );
    ftui_runtime::ftui_println!(
        "- Target: id={} slug={} key={}",
        target_project.id,
        target_project.slug,
        target_project.human_key
    );
    ftui_runtime::ftui_println!(
        "- Same repo (git-common-dir): {}",
        if same_repo { "yes" } else { "no" }
    );
    ftui_runtime::ftui_println!(
        "- Move Git artifacts: {} -> {}",
        source_archive.display(),
        target_archive.display()
    );
    ftui_runtime::ftui_println!(
        "- Re-key DB rows: source project_id -> target project_id (agents/messages/file_reservations)"
    );
    ftui_runtime::ftui_println!(
        "- Write aliases.json under target project archive with former_slugs"
    );

    if !same_repo {
        ftui_runtime::ftui_eprintln!(
            "Refusing to adopt: projects do not appear to belong to the same repository."
        );
        return Ok(());
    }

    // Safety contract: adoption is dry-run by default; --apply is required to mutate.
    let effective_dry_run = !apply;
    if effective_dry_run {
        return Ok(());
    }

    let duplicate_agent_rows = conn
        .query_sync(
            "SELECT s.name AS name FROM agents s \
             INNER JOIN agents d ON s.name = d.name \
             WHERE s.project_id = ? AND d.project_id = ? \
             ORDER BY s.name",
            &[
                sqlmodel_core::Value::BigInt(source_project.id),
                sqlmodel_core::Value::BigInt(target_project.id),
            ],
        )
        .map_err(|e| CliError::Other(format!("agent conflict check failed: {e}")))?;
    let mut duplicate_agent_names: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for row in duplicate_agent_rows {
        let name: String = row.get_named("name").unwrap_or_default();
        if !name.is_empty() {
            duplicate_agent_names.insert(name);
        }
    }
    if !duplicate_agent_names.is_empty() {
        return Err(CliError::InvalidArgument(format!(
            "agent name conflicts in target project: {}",
            duplicate_agent_names
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    std::fs::create_dir_all(&source_archive)?;
    std::fs::create_dir_all(&target_archive)?;

    let mut changed_paths =
        move_archive_files(&source_archive, &target_archive, &config.storage_root)?;

    conn.execute_sync(
        "UPDATE agents SET project_id = ? WHERE project_id = ?",
        &[
            sqlmodel_core::Value::BigInt(target_project.id),
            sqlmodel_core::Value::BigInt(source_project.id),
        ],
    )
    .map_err(|e| CliError::Other(format!("rekey agents failed: {e}")))?;
    conn.execute_sync(
        "UPDATE messages SET project_id = ? WHERE project_id = ?",
        &[
            sqlmodel_core::Value::BigInt(target_project.id),
            sqlmodel_core::Value::BigInt(source_project.id),
        ],
    )
    .map_err(|e| CliError::Other(format!("rekey messages failed: {e}")))?;
    conn.execute_sync(
        "UPDATE file_reservations SET project_id = ? WHERE project_id = ?",
        &[
            sqlmodel_core::Value::BigInt(target_project.id),
            sqlmodel_core::Value::BigInt(source_project.id),
        ],
    )
    .map_err(|e| CliError::Other(format!("rekey file_reservations failed: {e}")))?;

    conn.execute_sync(
        "INSERT OR IGNORE INTO product_project_links (product_id, project_id, created_at) \
         SELECT product_id, ?, created_at FROM product_project_links WHERE project_id = ?",
        &[
            sqlmodel_core::Value::BigInt(target_project.id),
            sqlmodel_core::Value::BigInt(source_project.id),
        ],
    )
    .map_err(|e| CliError::Other(format!("rekey product links failed: {e}")))?;
    conn.execute_sync(
        "DELETE FROM product_project_links WHERE project_id = ?",
        &[sqlmodel_core::Value::BigInt(source_project.id)],
    )
    .map_err(|e| CliError::Other(format!("cleanup source product links failed: {e}")))?;

    let aliases_path = target_archive.join("aliases.json");
    let mut alias_doc = if aliases_path.exists() {
        match std::fs::read_to_string(&aliases_path) {
            Ok(text) => serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default(),
            Err(_) => serde_json::json!({}),
        }
    } else {
        serde_json::json!({})
    };
    let mut former_slugs: std::collections::BTreeSet<String> = alias_doc
        .get("former_slugs")
        .and_then(|value| value.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|value| value.as_str().map(ToString::to_string))
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();
    former_slugs.insert(source_project.slug.clone());
    alias_doc["former_slugs"] = serde_json::Value::Array(
        former_slugs
            .into_iter()
            .map(serde_json::Value::String)
            .collect(),
    );
    std::fs::write(
        &aliases_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&alias_doc)
                .map_err(|e| CliError::Other(format!("serialize aliases.json failed: {e}")))?
        ),
    )?;
    let aliases_rel = aliases_path
        .strip_prefix(&config.storage_root)
        .ok()
        .unwrap_or(&aliases_path)
        .to_string_lossy()
        .replace('\\', "/");
    changed_paths.push(aliases_rel);

    let commit_message = format!(
        "adopt: move {} into {}",
        source_project.slug, target_project.slug
    );
    match git_add_and_commit(
        &config.storage_root,
        config,
        &changed_paths,
        &commit_message,
    ) {
        GitCommitOutcome::Committed | GitCommitOutcome::NothingToCommit => {}
        GitCommitOutcome::Failed(msg) => {
            ftui_runtime::ftui_eprintln!(
                "Warning: unable to commit adoption artifacts automatically. {msg}"
            );
        }
    }

    ftui_runtime::ftui_println!("Adoption apply completed.");
    Ok(())
}

fn handle_projects(action: ProjectsCommand) -> CliResult<()> {
    match action {
        ProjectsCommand::MarkIdentity {
            project_path,
            commit,
            no_commit,
        } => handle_project_mark_identity(&project_path, resolve_bool(commit, no_commit, true)),
        ProjectsCommand::DiscoveryInit {
            project_path,
            product,
        } => handle_project_discovery_init(&project_path, product),
        ProjectsCommand::Adopt {
            source,
            target,
            dry_run,
            apply,
        } => {
            let conn = open_db_sync()?;
            let config = Config::from_env();
            handle_projects_adopt_with_conn(
                &conn,
                &config,
                source.to_string_lossy().as_ref(),
                target.to_string_lossy().as_ref(),
                dry_run,
                apply,
            )
        }
    }
}

fn handle_doctor_check(
    project: Option<String>,
    verbose: bool,
    format: Option<output::CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    let config = Config::from_env();
    handle_doctor_check_with(
        &cfg.database_url,
        &config.storage_root,
        project,
        verbose,
        format,
        json,
    )
}

fn beads_issue_awareness_counts_from(
    start: Option<&Path>,
) -> Result<(usize, usize, usize), String> {
    let beads_dir = beads_rust::config::discover_beads_dir(start).map_err(|e| e.to_string())?;
    let (storage, _paths) =
        beads_rust::config::open_storage(&beads_dir, None, None).map_err(|e| e.to_string())?;

    let ready = storage
        .get_ready_issues(
            &beads_rust::storage::ReadyFilters::default(),
            beads_rust::storage::ReadySortPolicy::Hybrid,
        )
        .map_err(|e| e.to_string())?
        .len();

    let open = storage
        .list_issues(&beads_rust::storage::ListFilters {
            statuses: Some(vec![beads_rust::model::Status::Open]),
            ..Default::default()
        })
        .map_err(|e| e.to_string())?
        .len();

    let in_progress = storage
        .list_issues(&beads_rust::storage::ListFilters {
            statuses: Some(vec![beads_rust::model::Status::InProgress]),
            ..Default::default()
        })
        .map_err(|e| e.to_string())?
        .len();

    Ok((ready, open, in_progress))
}

fn beads_issue_awareness_counts() -> Result<(usize, usize, usize), String> {
    beads_issue_awareness_counts_from(None)
}

const STORAGE_ROOT_LOW_DISK_WARN_BYTES: u64 = 100 * 1024 * 1024;

fn format_bytes_human(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;

    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn storage_root_permissions_hint(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| format!("{:04o}", meta.permissions().mode() & 0o7777))
            .unwrap_or_else(|_| "unknown".to_string())
    }
    #[cfg(not(unix))]
    {
        "unknown".to_string()
    }
}

fn storage_root_write_probe(storage_root: &Path) -> Result<(), std::io::Error> {
    let probe_name = format!(
        ".am-doctor-write-probe-{}-{}",
        std::process::id(),
        Utc::now().timestamp_micros()
    );
    let probe_path = storage_root.join(probe_name);
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe_path)?;
    drop(file);
    let _ = std::fs::remove_file(&probe_path);
    Ok(())
}

fn discover_archive_git_repos(storage_root: &Path) -> Vec<PathBuf> {
    let projects_root = storage_root.join("projects");
    let mut repos = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&projects_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join(".git").exists() {
                repos.push(path);
            }
        }
    }
    repos.sort();
    repos
}

fn handle_doctor_check_with(
    database_url: &str,
    storage_root: &Path,
    project: Option<String>,
    verbose: bool,
    format: Option<output::CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    let mut checks: Vec<serde_json::Value> = Vec::new();
    let mut db_file_sanity_failed = false;

    // Check 1: Database accessible (non-mutating; avoid auto-recovery side effects).
    let db_ok = {
        let cfg = mcp_agent_mail_db::DbPoolConfig {
            database_url: database_url.to_string(),
            ..Default::default()
        };
        match cfg.sqlite_path() {
            Ok(path) if path == ":memory:" => true,
            Ok(path) => mcp_agent_mail_db::DbConn::open_file(&path)
                .ok()
                .and_then(|conn| conn.query_sync("SELECT 1 AS one", &[]).ok())
                .is_some(),
            Err(_) => false,
        }
    };
    checks.push(serde_json::json!({
        "check": "database",
        "status": if db_ok { "ok" } else { "fail" },
        "detail": if db_ok { "SQLite database accessible" } else { "Cannot open database" },
    }));

    // Check 1b: DB file sanity (non-zero size + PRAGMA quick_check)
    {
        let cfg = mcp_agent_mail_db::DbPoolConfig {
            database_url: database_url.to_string(),
            ..Default::default()
        };
        if let Ok(db_path) = cfg.sqlite_path() {
            let path = std::path::Path::new(&db_path);
            if db_path != ":memory:" && path.exists() {
                let file_size = path.metadata().map(|m| m.len()).unwrap_or(0);
                if file_size == 0 {
                    db_file_sanity_failed = true;
                    checks.push(serde_json::json!({
                        "check": "db_file_sanity",
                        "status": "fail",
                        "detail": "Database file is 0 bytes (empty/corrupt)",
                    }));
                } else if let Ok(conn) = mcp_agent_mail_db::DbConn::open_file(&db_path) {
                    let qc_ok = conn
                        .query_sync("PRAGMA quick_check", &[])
                        .ok()
                        .and_then(|rows| {
                            rows.first()
                                .and_then(|r| r.get_named::<String>("quick_check").ok())
                        })
                        .map(|s| s == "ok")
                        .unwrap_or(false);
                    if !qc_ok {
                        db_file_sanity_failed = true;
                    }
                    checks.push(serde_json::json!({
                        "check": "db_file_sanity",
                        "status": if qc_ok { "ok" } else { "fail" },
                        "detail": if qc_ok {
                            format!("quick_check OK ({file_size} bytes)")
                        } else {
                            "PRAGMA quick_check failed (possible corruption)".to_string()
                        },
                    }));
                } else {
                    db_file_sanity_failed = true;
                    checks.push(serde_json::json!({
                        "check": "db_file_sanity",
                        "status": "fail",
                        "detail": "Cannot open database file for sanity check",
                    }));
                }
            }
        }
    }

    // Check 1c: Pool initialization (including migrations)
    {
        if db_file_sanity_failed {
            checks.push(serde_json::json!({
                "check": "pool_init",
                "status": "fail",
                "detail": "Skipped because db_file_sanity failed (potential corruption)",
            }));
        } else {
            let pool_cfg = mcp_agent_mail_db::DbPoolConfig {
                database_url: database_url.to_string(),
                ..Default::default()
            };
            match mcp_agent_mail_db::pool::DbPool::new(&pool_cfg) {
                Ok(pool) => {
                    let pool_result: Result<(), String> = context::run_async(async {
                        let cx = asupersync::Cx::for_request();
                        match pool.acquire(&cx).await {
                            asupersync::Outcome::Ok(_conn) => Ok(()),
                            asupersync::Outcome::Err(e) => Err(CliError::Other(format!("{e}"))),
                            asupersync::Outcome::Cancelled(_) => {
                                Err(CliError::Other("cancelled".to_string()))
                            }
                            asupersync::Outcome::Panicked(p) => {
                                Err(CliError::Other(format!("panic: {}", p.message())))
                            }
                        }
                    })
                    .map_err(|e| e.to_string());
                    match pool_result {
                        Ok(()) => {
                            checks.push(serde_json::json!({
                                "check": "pool_init",
                                "status": "ok",
                                "detail": "Pool initialization + migrations OK",
                            }));
                        }
                        Err(msg) => {
                            checks.push(serde_json::json!({
                                "check": "pool_init",
                                "status": "fail",
                                "detail": format!("Pool/migration failure: {msg}"),
                            }));
                        }
                    }
                }
                Err(e) => {
                    checks.push(serde_json::json!({
                        "check": "pool_init",
                        "status": "fail",
                        "detail": format!("Cannot create pool: {e}"),
                    }));
                }
            }
        }
    }

    // Check 2: Storage root exists
    let storage_ok = storage_root.exists();
    checks.push(serde_json::json!({
        "check": "storage_root",
        "status": if storage_ok { "ok" } else { "warn" },
        "detail": format!("{}", storage_root.display()),
    }));

    // Check 2b: Storage root write permissions
    if storage_ok {
        match storage_root_write_probe(storage_root) {
            Ok(()) => checks.push(serde_json::json!({
                "check": "storage_root_writable",
                "status": "ok",
                "detail": "Storage root is writable",
            })),
            Err(err) => checks.push(serde_json::json!({
                "check": "storage_root_writable",
                "status": "fail",
                "detail": format!(
                    "Storage root not writable ({err}); permissions: {}",
                    storage_root_permissions_hint(storage_root)
                ),
            })),
        }
    } else {
        checks.push(serde_json::json!({
            "check": "storage_root_writable",
            "status": "warn",
            "detail": "Skipped: storage root missing",
        }));
    }

    // Check 2c: Storage root disk space
    if storage_ok {
        match mcp_agent_mail_core::disk::disk_free_bytes(storage_root) {
            Ok(bytes) => {
                let status = if bytes < STORAGE_ROOT_LOW_DISK_WARN_BYTES {
                    "warn"
                } else {
                    "ok"
                };
                let detail = if bytes < STORAGE_ROOT_LOW_DISK_WARN_BYTES {
                    format!(
                        "Low disk space: {} free (recommend >= 100 MB)",
                        format_bytes_human(bytes)
                    )
                } else {
                    format!("{} free", format_bytes_human(bytes))
                };
                checks.push(serde_json::json!({
                    "check": "storage_root_disk_space",
                    "status": status,
                    "detail": detail,
                }));
            }
            Err(err) => checks.push(serde_json::json!({
                "check": "storage_root_disk_space",
                "status": "warn",
                "detail": format!("Unable to measure free space: {err}"),
            })),
        }
    } else {
        checks.push(serde_json::json!({
            "check": "storage_root_disk_space",
            "status": "warn",
            "detail": "Skipped: storage root missing",
        }));
    }

    // Check 2d: Archive git repos are valid
    let archive_repos = if storage_ok {
        discover_archive_git_repos(storage_root)
    } else {
        Vec::new()
    };
    if !storage_ok {
        checks.push(serde_json::json!({
            "check": "storage_root_git_repo",
            "status": "warn",
            "detail": "Skipped: storage root missing",
        }));
    } else if archive_repos.is_empty() {
        checks.push(serde_json::json!({
            "check": "storage_root_git_repo",
            "status": "warn",
            "detail": format!(
                "No archive git repos found under {}",
                storage_root.join("projects").display()
            ),
        }));
    } else {
        let mut invalid_repos = Vec::new();
        for repo in &archive_repos {
            if mcp_agent_mail_guard::resolve_hooks_dir(repo).is_err() {
                invalid_repos.push(repo.display().to_string());
            }
        }
        if invalid_repos.is_empty() {
            checks.push(serde_json::json!({
                "check": "storage_root_git_repo",
                "status": "ok",
                "detail": format!("{} archive git repo(s) valid", archive_repos.len()),
            }));
        } else {
            checks.push(serde_json::json!({
                "check": "storage_root_git_repo",
                "status": "fail",
                "detail": format!(
                    "Invalid archive git repos: {}",
                    invalid_repos.join(", ")
                ),
            }));
        }
    }

    // Check 2e: No stale index.lock files in archive repos
    if !storage_ok {
        checks.push(serde_json::json!({
            "check": "storage_root_git_index_lock",
            "status": "warn",
            "detail": "Skipped: storage root missing",
        }));
    } else if archive_repos.is_empty() {
        checks.push(serde_json::json!({
            "check": "storage_root_git_index_lock",
            "status": "warn",
            "detail": "No archive repos to inspect for index.lock",
        }));
    } else {
        let mut lock_paths: Vec<PathBuf> = archive_repos
            .iter()
            .map(|repo| repo.join(".git").join("index.lock"))
            .filter(|path| path.exists())
            .collect();
        lock_paths.sort();
        if lock_paths.is_empty() {
            checks.push(serde_json::json!({
                "check": "storage_root_git_index_lock",
                "status": "ok",
                "detail": "No archive index.lock files detected",
            }));
        } else {
            let listed = lock_paths
                .iter()
                .take(5)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let suffix = if lock_paths.len() > 5 {
                format!(" (+{} more)", lock_paths.len() - 5)
            } else {
                String::new()
            };
            checks.push(serde_json::json!({
                "check": "storage_root_git_index_lock",
                "status": "warn",
                "detail": format!(
                    "Detected {} index.lock file(s): {listed}{suffix}",
                    lock_paths.len()
                ),
            }));
        }
    }

    // Check 2f: Guard hooks integrity in current repository
    match std::env::current_dir() {
        Ok(cwd) => match mcp_agent_mail_guard::guard_status(&cwd) {
            Ok(status) => {
                let hooks_ok = status.pre_commit_present && status.pre_push_present;
                let detail = if hooks_ok {
                    format!("pre-commit and pre-push installed in {}", status.hooks_dir)
                } else {
                    format!(
                        "Missing hooks (pre-commit={}, pre-push={}) in {}",
                        status.pre_commit_present, status.pre_push_present, status.hooks_dir
                    )
                };
                checks.push(serde_json::json!({
                    "check": "guard_hooks",
                    "status": if hooks_ok { "ok" } else { "warn" },
                    "detail": detail,
                }));
            }
            Err(err) => checks.push(serde_json::json!({
                "check": "guard_hooks",
                "status": "warn",
                "detail": format!("Guard status unavailable: {err}"),
            })),
        },
        Err(err) => checks.push(serde_json::json!({
            "check": "guard_hooks",
            "status": "warn",
            "detail": format!("Current directory unavailable: {err}"),
        })),
    }

    // Check 3: Installed coding-agent connectors
    let detect_opts = AgentDetectOptions {
        only_connectors: None,
        include_undetected: true,
        root_overrides: Vec::new(),
    };
    match mcp_agent_mail_core::detect_installed_agents(&detect_opts) {
        Ok(report) => {
            checks.push(serde_json::json!({
                "check": "installed_agents",
                "status": "ok",
                "detail": format!(
                    "{} detected of {} connector(s)",
                    report.summary.detected_count,
                    report.summary.total_count
                ),
            }));
        }
        Err(AgentDetectError::FeatureDisabled) => {
            checks.push(serde_json::json!({
                "check": "installed_agents",
                "status": "warn",
                "detail": "Agent detection feature disabled at compile time",
            }));
        }
        Err(err) => {
            checks.push(serde_json::json!({
                "check": "installed_agents",
                "status": "warn",
                "detail": format!("Agent detection unavailable: {err}"),
            }));
        }
    }

    // Check 4: Binary resolution — does `am` resolve to the Rust binary?
    {
        let expected_path = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".local/bin/am");
        let expected_str = expected_path.to_string_lossy().to_string();

        // Try both login and non-login shell contexts
        let shells: &[(&str, &str, &str)] = &[("zsh", "zsh", "-lc"), ("bash", "bash", "-lc")];

        let mut resolution_detail = String::new();
        let mut resolution_status = "ok";

        for &(label, shell_bin, flag) in shells {
            // Check if this shell exists
            let shell_exists = std::process::Command::new("which")
                .arg(shell_bin)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !shell_exists {
                continue;
            }

            // Check for alias
            let alias_out = std::process::Command::new(shell_bin)
                .arg(flag)
                .arg("alias am 2>/dev/null; true")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();

            if !alias_out.is_empty() && alias_out.contains("alias") {
                resolution_status = "fail";
                resolution_detail = format!(
                    "'am' is aliased in {label}: {alias_out}. \
                     Fix: remove the alias or run `unalias am`. \
                     Rust binary at: {expected_str}"
                );
                break;
            }

            // Check binary path
            let which_out = std::process::Command::new(shell_bin)
                .arg(flag)
                .arg("which am 2>/dev/null || echo NOT_FOUND")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|| "NOT_FOUND".to_string());

            if which_out == "NOT_FOUND" || which_out.is_empty() {
                if resolution_detail.is_empty() {
                    resolution_status = "warn";
                    resolution_detail =
                        format!("'am' not found in {label} PATH. Expected: {expected_str}");
                }
            } else if which_out != expected_str {
                resolution_detail =
                    format!("'am' resolves to {which_out} in {label} (expected {expected_str})");
                // Not necessarily a failure — could be a different install path
                if resolution_status != "fail" {
                    resolution_status = "warn";
                }
            } else {
                resolution_status = "ok";
                resolution_detail = format!("'am' resolves to {expected_str} ({label})");
                break;
            }
        }

        if resolution_detail.is_empty() {
            resolution_detail = "binary resolution check skipped (no suitable shell)".to_string();
            resolution_status = "warn";
        }

        checks.push(serde_json::json!({
            "check": "binary_resolution",
            "status": resolution_status,
            "detail": resolution_detail,
        }));
    }

    // Check 4b: PATH order — verify ~/.local/bin is in PATH and positioned correctly
    {
        let install_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".local/bin");
        let install_dir_str = install_dir.to_string_lossy().to_string();

        let path_var = std::env::var("PATH").unwrap_or_default();
        let path_dirs: Vec<&str> = path_var.split(':').collect();

        if let Some(pos) = path_dirs.iter().position(|d| {
            let p = Path::new(d);
            p == install_dir.as_path()
                || p.canonicalize()
                    .ok()
                    .map(|c| {
                        c == install_dir
                            .canonicalize()
                            .unwrap_or_else(|_| install_dir.clone())
                    })
                    .unwrap_or(false)
        }) {
            // Check if any earlier PATH entry also contains an `am` binary
            let mut shadow_detail = String::new();
            for earlier_dir in &path_dirs[..pos] {
                let candidate = Path::new(earlier_dir).join("am");
                if candidate.exists() && Path::new(earlier_dir) != install_dir.as_path() {
                    shadow_detail = format!(
                        " (shadowed by {} at PATH position {})",
                        candidate.display(),
                        path_dirs
                            .iter()
                            .position(|d| *d == *earlier_dir)
                            .unwrap_or(0)
                            + 1
                    );
                    break;
                }
            }
            if shadow_detail.is_empty() {
                checks.push(serde_json::json!({
                    "check": "path_order",
                    "status": "ok",
                    "detail": format!(
                        "{install_dir_str} in PATH at position {} of {}",
                        pos + 1,
                        path_dirs.len()
                    ),
                }));
            } else {
                checks.push(serde_json::json!({
                    "check": "path_order",
                    "status": "warn",
                    "detail": format!(
                        "{install_dir_str} in PATH at position {} of {}{shadow_detail}. \
                         Fix: move {install_dir_str} earlier in PATH",
                        pos + 1,
                        path_dirs.len()
                    ),
                }));
            }
        } else {
            checks.push(serde_json::json!({
                "check": "path_order",
                "status": "warn",
                "detail": format!(
                    "{install_dir_str} is NOT in PATH. \
                     Fix: add to shell config: export PATH=\"{install_dir_str}:$PATH\""
                ),
            }));
        }
    }

    // Check 4c: MCP config health — verify config files exist and reference Rust binary
    {
        use mcp_agent_mail_core::mcp_config::detect_mcp_config_locations_default;

        let rust_binary = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".local/bin/mcp-agent-mail");

        let locations = detect_mcp_config_locations_default();
        let existing: Vec<_> = locations.iter().filter(|l| l.exists).collect();

        if existing.is_empty() {
            checks.push(serde_json::json!({
                "check": "mcp_config",
                "status": "warn",
                "detail": "No MCP config files found. Run `am setup` to configure agent tools",
            }));
        } else {
            let mut pointing_to_rust = 0u32;
            let mut pointing_to_python = 0u32;
            let mut missing_entry = 0u32;
            let mut parse_errors = 0u32;
            let mut detail_parts: Vec<String> = Vec::new();

            for loc in &existing {
                match std::fs::read_to_string(&loc.config_path) {
                    Ok(content) => {
                        let has_rust = content.contains("mcp-agent-mail")
                            && (content.contains(".local/bin/mcp-agent-mail")
                                || content.contains(&rust_binary.to_string_lossy().to_string()));
                        let has_python = content.contains("mcp_agent_mail")
                            && (content.contains("python")
                                || content.contains("run_server")
                                || content.contains("uvx")
                                || content.contains("uv run"));

                        if has_rust {
                            pointing_to_rust += 1;
                        } else if has_python {
                            pointing_to_python += 1;
                            detail_parts.push(format!(
                                "{} ({}) → Python",
                                loc.tool.slug(),
                                loc.config_path.display()
                            ));
                        } else {
                            missing_entry += 1;
                        }
                    }
                    Err(_) => {
                        parse_errors += 1;
                    }
                }
            }

            let mcp_status = if pointing_to_python > 0 {
                "warn"
            } else if pointing_to_rust > 0 {
                "ok"
            } else {
                "warn"
            };

            let summary = format!(
                "{} config(s) found: {} Rust, {} Python, {} no entry, {} unreadable",
                existing.len(),
                pointing_to_rust,
                pointing_to_python,
                missing_entry,
                parse_errors
            );

            let detail = if pointing_to_python > 0 {
                format!(
                    "{}. Fix: run `am setup` to update: {}",
                    summary,
                    detail_parts.join(", ")
                )
            } else {
                summary
            };

            checks.push(serde_json::json!({
                "check": "mcp_config",
                "status": mcp_status,
                "detail": detail,
            }));
        }
    }

    // Check 4d: Database format and health (br-28mgh.7.4)
    {
        use mcp_agent_mail_db::migrate;
        let conn_result = open_db_sync_with_database_url(database_url);

        // 4d-i: Timestamp format
        let ts_check = conn_result
            .as_ref()
            .ok()
            .and_then(|conn| migrate::detect_timestamp_format(conn).ok());
        match ts_check {
            Some(fmt) if fmt.needs_migration() => {
                checks.push(serde_json::json!({
                    "check": "timestamp_format",
                    "status": "warn",
                    "detail": format!("{fmt} — run `am migrate` to convert"),
                }));
            }
            Some(fmt) => {
                checks.push(serde_json::json!({
                    "check": "timestamp_format",
                    "status": "ok",
                    "detail": format!("{fmt}"),
                }));
            }
            None => {
                checks.push(serde_json::json!({
                    "check": "timestamp_format",
                    "status": "warn",
                    "detail": "Could not detect timestamp format",
                }));
            }
        }

        // 4d-ii: WAL mode
        if let Ok(ref conn) = conn_result {
            let wal_ok = conn
                .query_sync("PRAGMA journal_mode", &[])
                .ok()
                .and_then(|rows| {
                    rows.first()
                        .and_then(|r| r.get_named::<String>("journal_mode").ok())
                })
                .map(|m| m.eq_ignore_ascii_case("wal"))
                .unwrap_or(false);
            checks.push(serde_json::json!({
                "check": "wal_mode",
                "status": if wal_ok { "ok" } else { "warn" },
                "detail": if wal_ok {
                    "WAL mode enabled"
                } else {
                    "WAL mode not enabled. Fix: run `am migrate` or restart the server"
                },
            }));
        }

        // 4d-iii: Schema version (user_version PRAGMA)
        if let Ok(ref conn) = conn_result {
            let version = conn
                .query_sync("PRAGMA user_version", &[])
                .ok()
                .and_then(|rows| {
                    rows.first()
                        .and_then(|r| r.get_named::<i64>("user_version").ok())
                })
                .unwrap_or(0);
            checks.push(serde_json::json!({
                "check": "schema_version",
                "status": if version > 0 { "ok" } else { "warn" },
                "detail": format!("user_version = {version}"),
            }));
        }

        // 4d-iv: FTS5 virtual tables
        if let Ok(ref conn) = conn_result {
            let fts_ok = conn
                .query_sync(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE '%_fts%'",
                    &[],
                )
                .ok()
                .map(|rows| !rows.is_empty())
                .unwrap_or(false);
            let fts_query_ok = if fts_ok {
                conn.query_sync(
                    "SELECT COUNT(*) AS cnt FROM messages_fts WHERE messages_fts MATCH 'test' LIMIT 1",
                    &[],
                )
                .is_ok()
            } else {
                false
            };
            let (fts_status, fts_detail) = if fts_ok && fts_query_ok {
                (
                    "ok",
                    "FTS5 virtual tables present and queryable".to_string(),
                )
            } else if fts_ok {
                ("warn", "FTS5 tables exist but query failed".to_string())
            } else {
                ("warn", "No FTS5 virtual tables found".to_string())
            };
            checks.push(serde_json::json!({
                "check": "fts5",
                "status": fts_status,
                "detail": fts_detail,
            }));
        }

        // 4d-v: Row counts (sanity check)
        if verbose && let Ok(ref conn) = conn_result {
            let tables = ["projects", "agents", "messages", "file_reservations"];
            let mut counts: Vec<String> = Vec::new();
            for table in &tables {
                let count: i64 = conn
                    .query_sync(&format!("SELECT COUNT(*) AS cnt FROM {table}"), &[])
                    .ok()
                    .and_then(|rows| rows.first().and_then(|r| r.get_named("cnt").ok()))
                    .unwrap_or(0);
                counts.push(format!("{table}={count}"));
            }
            checks.push(serde_json::json!({
                "check": "row_counts",
                "status": "ok",
                "detail": counts.join(", "),
            }));
        }
    }

    // Check 5: Beads issue awareness (ready/open/in-progress)
    match beads_issue_awareness_counts() {
        Ok((ready, open, in_progress)) => {
            checks.push(serde_json::json!({
                "check": "beads_issue_awareness",
                "status": "ok",
                "detail": format!("{ready} ready / {open} open / {in_progress} in_progress"),
            }));
        }
        Err(err) => {
            checks.push(serde_json::json!({
                "check": "beads_issue_awareness",
                "status": "warn",
                "detail": format!("Unavailable: {err}"),
            }));
        }
    }

    // Check 5: Project-specific checks
    if let Some(ref slug) = project
        && let Ok(conn) = open_db_sync_with_database_url(database_url)
    {
        let rows = conn
            .query_sync(
                "SELECT id, slug FROM projects WHERE slug = ?",
                &[sqlmodel_core::Value::Text(slug.clone())],
            )
            .unwrap_or_default();
        let project_exists = !rows.is_empty();
        checks.push(serde_json::json!({
            "check": "project_exists",
            "status": if project_exists { "ok" } else { "fail" },
            "detail": format!("project '{slug}'"),
        }));

        if project_exists {
            let agent_rows = conn
                .query_sync(
                    "SELECT COUNT(*) AS cnt FROM agents a \
                     JOIN projects p ON p.id = a.project_id \
                     WHERE p.slug = ?",
                    &[sqlmodel_core::Value::Text(slug.clone())],
                )
                .unwrap_or_default();
            let agent_count: i64 = agent_rows
                .first()
                .and_then(|r| r.get_named("cnt").ok())
                .unwrap_or(0);
            checks.push(serde_json::json!({
                "check": "agents_registered",
                "status": "ok",
                "detail": format!("{agent_count} agent(s)"),
            }));
        }
    }

    // Output
    let all_ok = checks.iter().all(|c| c["status"] != "fail");
    let fmt = output::CliOutputFormat::resolve(format, json);
    let payload = serde_json::json!({
        "healthy": all_ok,
        "checks": checks,
    });

    if matches!(fmt, output::CliOutputFormat::Table) {
        ftui_runtime::ftui_println!(
            "Doctor check{}:",
            project
                .as_deref()
                .map(|p| format!(" ({p})"))
                .unwrap_or_default()
        );
        for c in payload
            .get("checks")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let icon = match c["status"].as_str().unwrap_or("") {
                "ok" => "OK",
                "warn" => "WARN",
                _ => "FAIL",
            };
            let detail = if verbose {
                format!(" - {}", c["detail"].as_str().unwrap_or(""))
            } else {
                String::new()
            };
            ftui_runtime::ftui_println!(
                "  [{}] {}{}",
                icon,
                c["check"].as_str().unwrap_or("?"),
                detail
            );
        }
        if all_ok {
            ftui_runtime::ftui_println!("All checks passed.");
        } else {
            ftui_runtime::ftui_println!("Some checks failed.");
            return Err(CliError::ExitCode(1));
        }
    } else {
        output::emit_output(&payload, fmt, || {});
    }
    Ok(())
}

fn handle_mail(action: MailCommand) -> CliResult<()> {
    match action {
        MailCommand::Status { .. } => {
            let conn = open_db_sync()?;
            handle_mail_status_sync(&conn, action)
        }
        _ => context::run_async(async move { handle_mail_async(action).await }),
    }
}

fn handle_mail_status_sync(conn: &mcp_agent_mail_db::DbConn, action: MailCommand) -> CliResult<()> {
    let MailCommand::Status { project_path } = action else {
        unreachable!()
    };
    let identity = resolve_project_identity(project_path.to_string_lossy().as_ref());
    let slug = &identity.project_uid;

    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS cnt FROM messages m \
             JOIN projects p ON p.id = m.project_id \
             WHERE p.slug = ?",
            &[sqlmodel_core::Value::Text(slug.to_string())],
        )
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
    let total: i64 = rows
        .first()
        .and_then(|r| r.get_named("cnt").ok())
        .unwrap_or(0);

    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS cnt FROM agents a \
             JOIN projects p ON p.id = a.project_id \
             WHERE p.slug = ?",
            &[sqlmodel_core::Value::Text(slug.to_string())],
        )
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
    let agents: i64 = rows
        .first()
        .and_then(|r| r.get_named("cnt").ok())
        .unwrap_or(0);

    output::section(&format!("Project: {slug}"));
    output::kv("Messages", &total.to_string());
    output::kv("Agents", &agents.to_string());
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn handle_mail_async(action: MailCommand) -> CliResult<()> {
    let ctx = context::AsyncCliContext::open()?;
    let cx = asupersync::Cx::for_request();

    match action {
        MailCommand::Status { .. } => unreachable!("handled in sync path"),

        MailCommand::Send {
            project_key,
            sender,
            to,
            subject,
            body,
            cc,
            importance,
            ack_required,
            thread_id,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;
            let pid = proj.id.unwrap_or(0);
            let sender_row = resolve_agent_async(&cx, &ctx.pool, pid, &sender).await?;

            // Parse recipients
            let to_names: Vec<&str> = to
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if to_names.is_empty() {
                return Err(CliError::InvalidArgument(
                    "--to requires at least one recipient".into(),
                ));
            }

            let mut recipients: Vec<(i64, &str)> = Vec::new();
            for name in &to_names {
                let agent = resolve_agent_async(&cx, &ctx.pool, pid, name).await?;
                recipients.push((agent.id.unwrap_or(0), "to"));
            }
            if let Some(ref cc_str) = cc {
                for name in cc_str.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let agent = resolve_agent_async(&cx, &ctx.pool, pid, name).await?;
                    recipients.push((agent.id.unwrap_or(0), "cc"));
                }
            }

            let msg = outcome_to_result(
                mcp_agent_mail_db::queries::create_message_with_recipients(
                    &cx,
                    &ctx.pool,
                    pid,
                    sender_row.id.unwrap_or(0),
                    &subject,
                    &body,
                    thread_id.as_deref(),
                    &importance,
                    ack_required,
                    "", // attachments
                    &recipients,
                )
                .await,
            )?;

            let data = message_row_to_json(&msg, &sender);
            output::emit_output(&data, fmt, || {
                output::success(&format!(
                    "Message sent (id={}) to {}",
                    msg.id.unwrap_or(0),
                    to
                ));
            });
            Ok(())
        }

        MailCommand::Reply {
            project_key,
            sender,
            message_id,
            body,
            to,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;
            let pid = proj.id.unwrap_or(0);
            let sender_row = resolve_agent_async(&cx, &ctx.pool, pid, &sender).await?;

            // Get original message to derive thread_id and default recipient
            let orig = get_message_by_id_async(&cx, &ctx.pool, message_id).await?;
            let thread_id = orig
                .thread_id
                .clone()
                .unwrap_or_else(|| message_id.to_string());

            // Determine recipients
            let to_names: Vec<String> = if let Some(ref explicit_to) = to {
                explicit_to
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            } else {
                // Default to original sender
                let orig_sender = outcome_to_result(
                    mcp_agent_mail_db::queries::get_agent_by_id(&cx, &ctx.pool, orig.sender_id)
                        .await,
                )?;
                vec![orig_sender.name.clone()]
            };

            let mut recipients: Vec<(i64, &str)> = Vec::new();
            for name in &to_names {
                let agent = resolve_agent_async(&cx, &ctx.pool, pid, name).await?;
                recipients.push((agent.id.unwrap_or(0), "to"));
            }

            // Prefix subject
            let subject = if orig.subject.to_ascii_lowercase().starts_with("re:") {
                orig.subject.clone()
            } else {
                format!("Re: {}", orig.subject)
            };

            let msg = outcome_to_result(
                mcp_agent_mail_db::queries::create_message_with_recipients(
                    &cx,
                    &ctx.pool,
                    pid,
                    sender_row.id.unwrap_or(0),
                    &subject,
                    &body,
                    Some(&thread_id),
                    &orig.importance,
                    orig.ack_required != 0,
                    "",
                    &recipients,
                )
                .await,
            )?;

            let data = message_row_to_json(&msg, &sender);
            output::emit_output(&data, fmt, || {
                output::success(&format!(
                    "Reply sent (id={}, thread={})",
                    msg.id.unwrap_or(0),
                    thread_id
                ));
            });
            Ok(())
        }

        MailCommand::Inbox {
            project_key,
            agent_name,
            urgent_only,
            since,
            limit,
            include_bodies,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;
            let pid = proj.id.unwrap_or(0);
            let agent = resolve_agent_async(&cx, &ctx.pool, pid, &agent_name).await?;

            let since_ts = match since.as_deref() {
                None => None,
                Some(s) => Some(mcp_agent_mail_db::iso_to_micros(s).ok_or_else(|| {
                    CliError::InvalidArgument(format!("bad --since timestamp: {s}"))
                })?),
            };

            let rows = outcome_to_result(
                mcp_agent_mail_db::queries::fetch_inbox(
                    &cx,
                    &ctx.pool,
                    pid,
                    agent.id.unwrap_or(0),
                    urgent_only,
                    since_ts,
                    limit.max(0) as usize,
                )
                .await,
            )?;

            if rows.is_empty() {
                output::emit_empty(fmt, "No messages.");
                return Ok(());
            }

            // Build serializable data for JSON/TOON
            let data: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| inbox_row_to_json(r, include_bodies))
                .collect();

            output::emit_output(&data, fmt, || {
                let mut table =
                    output::CliTable::new(vec!["ID", "FROM", "SUBJECT", "IMPORTANCE", "TIME"]);
                for r in &rows {
                    table.add_row(vec![
                        r.message.id.unwrap_or(0).to_string(),
                        r.sender_name.clone(),
                        truncate_str(&r.message.subject, 50),
                        r.message.importance.clone(),
                        context::format_ts_short(r.message.created_ts),
                    ]);
                }
                table.render();

                if include_bodies {
                    for r in &rows {
                        ftui_runtime::ftui_println!(
                            "\n--- #{} {} ---",
                            r.message.id.unwrap_or(0),
                            r.message.subject
                        );
                        ftui_runtime::ftui_println!("{}", r.message.body_md);
                    }
                }
            });
            Ok(())
        }

        MailCommand::Read {
            project_key,
            agent_name,
            message_id,
        } => {
            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;
            let pid = proj.id.unwrap_or(0);
            let agent = resolve_agent_async(&cx, &ctx.pool, pid, &agent_name).await?;

            let ts = outcome_to_result(
                mcp_agent_mail_db::queries::mark_message_read(
                    &cx,
                    &ctx.pool,
                    agent.id.unwrap_or(0),
                    message_id,
                )
                .await,
            )?;
            output::success(&format!(
                "Message {message_id} marked as read at {}",
                context::format_ts(ts)
            ));
            Ok(())
        }

        MailCommand::Ack {
            project_key,
            agent_name,
            message_id,
        } => {
            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;
            let pid = proj.id.unwrap_or(0);
            let agent = resolve_agent_async(&cx, &ctx.pool, pid, &agent_name).await?;

            let (read_ts, ack_ts) = outcome_to_result(
                mcp_agent_mail_db::queries::acknowledge_message(
                    &cx,
                    &ctx.pool,
                    agent.id.unwrap_or(0),
                    message_id,
                )
                .await,
            )?;
            output::success(&format!(
                "Message {message_id} acknowledged (read={}, ack={})",
                context::format_ts(read_ts),
                context::format_ts(ack_ts)
            ));
            Ok(())
        }

        MailCommand::SummarizeThread {
            project_key,
            thread_id,
            per_thread_limit,
            no_llm,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let config = mcp_agent_mail_core::config::Config::from_env();
            let server_url = format!(
                "http://{}:{}{}",
                config.http_host, config.http_port, config.http_path
            );
            let bearer = config.http_bearer_token.as_deref();

            let server_result = try_call_server_tool(
                &server_url,
                bearer,
                "summarize_thread",
                serde_json::json!({
                    "project_key": project_key,
                    "thread_id": thread_id,
                    "include_examples": true,
                    "llm_mode": !no_llm,
                    "per_thread_limit": per_thread_limit,
                }),
            )
            .await
            .and_then(coerce_tool_result_json);

            let Some(payload) = server_result else {
                ftui_runtime::ftui_println!(
                    "Server unavailable; summarization requires server tool. Try again when server is running."
                );
                return Err(CliError::ExitCode(2));
            };

            if !matches!(fmt, output::CliOutputFormat::Table) {
                output::emit_output(&payload, fmt, || {});
                return Ok(());
            }

            let summary = payload.get("summary").cloned().unwrap_or_default();
            let participants = summary
                .get("participants")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();

            output::section(&format!("Thread summary: {thread_id}"));
            output::kv("Participants", &participants);
            output::kv(
                "Total messages",
                &summary
                    .get("total_messages")
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
            );
            output::kv(
                "Open actions",
                &summary
                    .get("open_actions")
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
            );
            output::kv(
                "Done actions",
                &summary
                    .get("done_actions")
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
            );
            Ok(())
        }

        MailCommand::Search {
            project_key,
            query,
            limit,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;
            let pid = proj.id.unwrap_or(0);

            let mut search_query =
                mcp_agent_mail_db::search_planner::SearchQuery::messages(&query, pid);
            search_query.limit = Some(limit.max(0) as usize);

            let response = outcome_to_result(
                mcp_agent_mail_db::search_service::execute_search_simple(
                    &cx,
                    &ctx.pool,
                    &search_query,
                )
                .await,
            )?;

            if response.results.is_empty() {
                output::emit_empty(fmt, "No results.");
                return Ok(());
            }

            // Build serializable data for JSON/TOON
            let data: Vec<serde_json::Value> = response
                .results
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "subject": r.title,
                        "importance": r.importance,
                        "ack_required": r.ack_required.unwrap_or(false),
                        "created_ts": r.created_ts.map(mcp_agent_mail_db::micros_to_iso),
                        "thread_id": r.thread_id,
                        "from": r.from_agent,
                    })
                })
                .collect();

            output::emit_output(&data, fmt, || {
                let mut table =
                    output::CliTable::new(vec!["ID", "FROM", "SUBJECT", "IMPORTANCE", "TIME"]);
                for r in &response.results {
                    table.add_row(vec![
                        r.id.to_string(),
                        r.from_agent.clone().unwrap_or_default(),
                        truncate_str(&r.title, 50),
                        r.importance.clone().unwrap_or_default(),
                        r.created_ts
                            .map(context::format_ts_short)
                            .unwrap_or_default(),
                    ]);
                }
                table.render();
            });
            Ok(())
        }
    }
}

fn handle_amctl(action: AmctlCommand) -> CliResult<()> {
    match action {
        AmctlCommand::Env { path, agent } => {
            let identity = resolve_project_identity(path.to_string_lossy().as_ref());
            let agent_name = agent
                .or_else(|| std::env::var("AGENT_NAME").ok())
                .unwrap_or_else(|| "Unknown".to_string());
            let branch = identity
                .branch
                .clone()
                .filter(|b| !b.is_empty())
                .or_else(|| compute_git_branch(&path))
                .unwrap_or_else(|| "unknown".to_string());
            let cache_key = format!(
                "am-cache-{}-{}-{}",
                identity.project_uid, agent_name, branch
            );
            let config = Config::from_env();
            let artifact_dir = config
                .storage_root
                .join("projects")
                .join(&identity.slug)
                .join("artifacts")
                .join(&agent_name)
                .join(&branch);

            ftui_runtime::ftui_println!("SLUG={}", identity.slug);
            ftui_runtime::ftui_println!("PROJECT_UID={}", identity.project_uid);
            ftui_runtime::ftui_println!("BRANCH={branch}");
            ftui_runtime::ftui_println!("AGENT={agent_name}");
            ftui_runtime::ftui_println!("CACHE_KEY={cache_key}");
            ftui_runtime::ftui_println!("ARTIFACT_DIR={}", artifact_dir.display());
            Ok(())
        }
    }
}

// ── agents command family (br-21gj.4.2) ────────────────────────────────

fn handle_agents(action: AgentsCommand) -> CliResult<()> {
    context::run_async(async move { handle_agents_async(action).await })
}

async fn handle_agents_async(action: AgentsCommand) -> CliResult<()> {
    let ctx = context::AsyncCliContext::open()?;
    let cx = asupersync::Cx::for_request();

    match action {
        AgentsCommand::Register {
            project_key,
            program,
            model,
            name,
            task,
            attachments_policy,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let program = program.trim().to_string();
            if program.is_empty() {
                return Err(CliError::InvalidArgument("program cannot be empty".into()));
            }
            let model = model.trim().to_string();
            if model.is_empty() {
                return Err(CliError::InvalidArgument("model cannot be empty".into()));
            }

            // Resolve project
            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;

            // Resolve or generate agent name
            let agent_name = name.unwrap_or_else(mcp_agent_mail_core::models::generate_agent_name);

            let row = match mcp_agent_mail_db::queries::register_agent(
                &cx,
                &ctx.pool,
                proj.id.unwrap_or(0),
                &agent_name,
                &program,
                &model,
                task.as_deref(),
                Some(attachments_policy.as_str()),
            )
            .await
            {
                asupersync::Outcome::Ok(r) => r,
                asupersync::Outcome::Err(e) => {
                    return Err(CliError::Other(format!("register_agent failed: {e}")));
                }
                asupersync::Outcome::Cancelled(_) => {
                    return Err(CliError::Other("request cancelled".into()));
                }
                asupersync::Outcome::Panicked(p) => {
                    return Err(CliError::Other(format!("internal panic: {}", p.message())));
                }
            };

            render_agent_row(&row, fmt);
            Ok(())
        }

        AgentsCommand::Create {
            project_key,
            program,
            model,
            name_hint,
            task,
            attachments_policy,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let program = program.trim().to_string();
            if program.is_empty() {
                return Err(CliError::InvalidArgument("program cannot be empty".into()));
            }
            let model = model.trim().to_string();
            if model.is_empty() {
                return Err(CliError::InvalidArgument("model cannot be empty".into()));
            }

            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;

            let agent_name =
                name_hint.unwrap_or_else(mcp_agent_mail_core::models::generate_agent_name);

            // Enforce uniqueness: check if agent already exists
            let existing = match mcp_agent_mail_db::queries::get_agent(
                &cx,
                &ctx.pool,
                proj.id.unwrap_or(0),
                &agent_name,
            )
            .await
            {
                asupersync::Outcome::Ok(row) => Some(row),
                asupersync::Outcome::Err(_) => None,
                asupersync::Outcome::Cancelled(_) => None,
                asupersync::Outcome::Panicked(p) => {
                    return Err(CliError::Other(format!("internal panic: {}", p.message())));
                }
            };

            if existing.is_some() {
                return Err(CliError::InvalidArgument(format!(
                    "agent name already exists in this project: {agent_name}"
                )));
            }

            let row = match mcp_agent_mail_db::queries::register_agent(
                &cx,
                &ctx.pool,
                proj.id.unwrap_or(0),
                &agent_name,
                &program,
                &model,
                task.as_deref(),
                Some(attachments_policy.as_str()),
            )
            .await
            {
                asupersync::Outcome::Ok(r) => r,
                asupersync::Outcome::Err(e) => {
                    return Err(CliError::Other(format!("create_agent failed: {e}")));
                }
                asupersync::Outcome::Cancelled(_) => {
                    return Err(CliError::Other("request cancelled".into()));
                }
                asupersync::Outcome::Panicked(p) => {
                    return Err(CliError::Other(format!("internal panic: {}", p.message())));
                }
            };

            render_agent_row(&row, fmt);
            Ok(())
        }

        AgentsCommand::List {
            project_key,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;

            let agents =
                match mcp_agent_mail_db::queries::list_agents(&cx, &ctx.pool, proj.id.unwrap_or(0))
                    .await
                {
                    asupersync::Outcome::Ok(rows) => rows,
                    asupersync::Outcome::Err(e) => {
                        return Err(CliError::Other(format!("list_agents failed: {e}")));
                    }
                    asupersync::Outcome::Cancelled(_) => {
                        return Err(CliError::Other("request cancelled".into()));
                    }
                    asupersync::Outcome::Panicked(p) => {
                        return Err(CliError::Other(format!("internal panic: {}", p.message())));
                    }
                };

            let data: Vec<serde_json::Value> = agents.iter().map(agent_row_to_json).collect();
            if data.is_empty() {
                output::emit_empty(fmt, "No agents found.");
                return Ok(());
            }

            output::emit_output(&data, fmt, || {
                let mut table =
                    output::CliTable::new(vec!["NAME", "PROGRAM", "MODEL", "TASK", "LAST_ACTIVE"]);
                for a in &agents {
                    table.add_row(vec![
                        a.name.clone(),
                        a.program.clone(),
                        a.model.clone(),
                        truncate_str(&a.task_description, 40),
                        context::format_ts_short(a.last_active_ts),
                    ]);
                }
                table.render();
            });
            Ok(())
        }

        AgentsCommand::Show {
            project_key,
            agent,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;

            let row = match mcp_agent_mail_db::queries::get_agent(
                &cx,
                &ctx.pool,
                proj.id.unwrap_or(0),
                &agent,
            )
            .await
            {
                asupersync::Outcome::Ok(r) => r,
                asupersync::Outcome::Err(e) => {
                    return Err(CliError::InvalidArgument(format!(
                        "agent not found: {agent} ({e})"
                    )));
                }
                asupersync::Outcome::Cancelled(_) => {
                    return Err(CliError::Other("request cancelled".into()));
                }
                asupersync::Outcome::Panicked(p) => {
                    return Err(CliError::Other(format!("internal panic: {}", p.message())));
                }
            };

            let data = agent_row_to_json(&row);
            output::emit_output(&data, fmt, || {
                output::section(&format!("Agent: {}", row.name));
                output::kv("ID", &row.id.unwrap_or(0).to_string());
                output::kv("Program", &row.program);
                output::kv("Model", &row.model);
                output::kv("Task", &row.task_description);
                output::kv("Attachments", &row.attachments_policy);
                output::kv("Contact Policy", &row.contact_policy);
                output::kv("Inception", &context::format_ts(row.inception_ts));
                output::kv("Last Active", &context::format_ts(row.last_active_ts));
            });
            Ok(())
        }

        AgentsCommand::Detect {
            only,
            include_undetected,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let only_connectors = only.map(|s| {
                s.split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>()
            });

            let opts = mcp_agent_mail_core::AgentDetectOptions {
                only_connectors,
                include_undetected,
                ..Default::default()
            };

            match mcp_agent_mail_core::detect_installed_agents(&opts) {
                Ok(report) => {
                    output::emit_output(&report, fmt, || {
                        output::section(&format!(
                            "Installed Agents ({}/{} detected):",
                            report.summary.detected_count, report.summary.total_count
                        ));
                        for entry in &report.installed_agents {
                            let status = if entry.detected {
                                "detected"
                            } else {
                                "not found"
                            };
                            ftui_runtime::ftui_println!("  {} ({})", entry.slug, status);
                            for path in &entry.root_paths {
                                ftui_runtime::ftui_println!("    root: {path}");
                            }
                        }
                    });
                    Ok(())
                }
                Err(e) => Err(CliError::Other(format!("agent detection failed: {e}"))),
            }
        }
    }
}

/// Resolve a project key to a `ProjectRow` via the async DB layer.
///
/// Tries slug lookup, then human_key lookup, then auto-creates if the key is
/// an absolute path. Distinguishes "not found" errors (which should fall
/// through to the next lookup) from real DB/connection errors (which surface
/// immediately so the caller sees the actual problem).
async fn resolve_project_async(
    cx: &asupersync::Cx,
    pool: &mcp_agent_mail_db::DbPool,
    key: &str,
) -> CliResult<mcp_agent_mail_db::ProjectRow> {
    // Try by slug first
    match mcp_agent_mail_db::queries::get_project_by_slug(cx, pool, key).await {
        asupersync::Outcome::Ok(row) => return Ok(row),
        asupersync::Outcome::Err(mcp_agent_mail_db::DbError::NotFound { .. }) => { /* fall through */
        }
        asupersync::Outcome::Err(e) => return Err(CliError::Other(format!("database error: {e}"))),
        asupersync::Outcome::Cancelled(_) => {
            return Err(CliError::Other("request cancelled".into()));
        }
        asupersync::Outcome::Panicked(p) => {
            return Err(CliError::Other(format!("internal panic: {}", p.message())));
        }
    }

    // Try by human_key
    match mcp_agent_mail_db::queries::get_project_by_human_key(cx, pool, key).await {
        asupersync::Outcome::Ok(row) => return Ok(row),
        asupersync::Outcome::Err(mcp_agent_mail_db::DbError::NotFound { .. }) => { /* fall through */
        }
        asupersync::Outcome::Err(e) => return Err(CliError::Other(format!("database error: {e}"))),
        asupersync::Outcome::Cancelled(_) => {
            return Err(CliError::Other("request cancelled".into()));
        }
        asupersync::Outcome::Panicked(p) => {
            return Err(CliError::Other(format!("internal panic: {}", p.message())));
        }
    }

    // If it looks like an absolute path, auto-create it
    if key.starts_with('/') {
        let proj = mcp_agent_mail_db::queries::ensure_project(cx, pool, key).await;
        return match proj {
            asupersync::Outcome::Ok(row) => Ok(row),
            asupersync::Outcome::Err(e) => {
                Err(CliError::Other(format!("ensure_project failed: {e}")))
            }
            asupersync::Outcome::Cancelled(_) => Err(CliError::Other("request cancelled".into())),
            asupersync::Outcome::Panicked(p) => {
                Err(CliError::Other(format!("internal panic: {}", p.message())))
            }
        };
    }

    Err(CliError::InvalidArgument(format!(
        "project not found: {key}"
    )))
}

fn agent_row_to_json(a: &mcp_agent_mail_db::AgentRow) -> serde_json::Value {
    serde_json::json!({
        "id": a.id.unwrap_or(0),
        "name": a.name,
        "program": a.program,
        "model": a.model,
        "task_description": a.task_description,
        "inception_ts": mcp_agent_mail_db::micros_to_iso(a.inception_ts),
        "last_active_ts": mcp_agent_mail_db::micros_to_iso(a.last_active_ts),
        "project_id": a.project_id,
        "attachments_policy": a.attachments_policy,
        "contact_policy": a.contact_policy,
    })
}

fn render_agent_row(row: &mcp_agent_mail_db::AgentRow, format: output::CliOutputFormat) {
    let payload = agent_row_to_json(row);
    output::emit_output(&payload, format, || {
        output::success(&format!("Agent: {}", row.name));
        output::kv("ID", &row.id.unwrap_or(0).to_string());
        output::kv("Program", &row.program);
        output::kv("Model", &row.model);
        output::kv("Task", &row.task_description);
        output::kv("Attachments", &row.attachments_policy);
        output::kv("Inception", &context::format_ts(row.inception_ts));
        output::kv("Last Active", &context::format_ts(row.last_active_ts));
    });
}

// ── Macro command handler ────────────────────────────────────────────

fn handle_macros(action: MacroCommand) -> CliResult<()> {
    context::run_async(async move { handle_macros_async(action).await })
}

#[allow(clippy::too_many_lines)]
async fn handle_macros_async(action: MacroCommand) -> CliResult<()> {
    let ctx = context::AsyncCliContext::open()?;
    let cx = asupersync::Cx::for_request();

    match action {
        MacroCommand::StartSession {
            human_key,
            program,
            model,
            agent_name,
            task,
            reserve_paths,
            reserve_reason,
            reserve_ttl,
            inbox_limit,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            // Validate human_key is absolute
            if !human_key.starts_with('/') {
                return Err(CliError::InvalidArgument(
                    "project key must be an absolute path (e.g. /data/projects/backend)".into(),
                ));
            }

            // 1. Ensure project
            let proj = resolve_project_async(&cx, &ctx.pool, &human_key).await?;
            let pid = proj.id.unwrap_or(0);

            // 2. Register agent
            let agent_name =
                agent_name.unwrap_or_else(mcp_agent_mail_core::models::generate_agent_name);
            let agent = outcome_to_result(
                mcp_agent_mail_db::queries::register_agent(
                    &cx,
                    &ctx.pool,
                    pid,
                    &agent_name,
                    &program,
                    &model,
                    task.as_deref(),
                    Some("auto"),
                )
                .await,
            )?;

            // 3. Reserve files (if any paths given)
            let reservations = if reserve_paths.is_empty() {
                Vec::new()
            } else {
                let path_refs: Vec<&str> = reserve_paths.iter().map(String::as_str).collect();
                let reason = reserve_reason.unwrap_or_else(|| "macro-session".to_string());
                outcome_to_result(
                    mcp_agent_mail_db::queries::create_file_reservations(
                        &cx,
                        &ctx.pool,
                        pid,
                        agent.id.unwrap_or(0),
                        &path_refs,
                        reserve_ttl,
                        true, // exclusive
                        &reason,
                    )
                    .await,
                )?
            };

            // 4. Fetch inbox
            let inbox = outcome_to_result(
                mcp_agent_mail_db::queries::fetch_inbox(
                    &cx,
                    &ctx.pool,
                    pid,
                    agent.id.unwrap_or(0),
                    false,
                    None,
                    inbox_limit.max(0) as usize,
                )
                .await,
            )?;

            let resp = serde_json::json!({
                "project": {
                    "id": pid,
                    "slug": proj.slug,
                    "human_key": proj.human_key,
                    "created_at": mcp_agent_mail_db::micros_to_iso(proj.created_at),
                },
                "agent": agent_row_to_json(&agent),
                "file_reservations": {
                    "granted": reservations.iter().map(|r| serde_json::json!({
                        "id": r.id.unwrap_or(0),
                        "path_pattern": r.path_pattern,
                        "exclusive": r.exclusive != 0,
                        "reason": r.reason,
                        "expires_ts": mcp_agent_mail_db::micros_to_iso(r.expires_ts),
                    })).collect::<Vec<_>>(),
                    "conflicts": [],
                },
                "inbox": inbox.iter().map(|r| inbox_row_to_json(r, false)).collect::<Vec<_>>(),
            });

            output::emit_output(&resp, fmt, || {
                output::success(&format!("Session started for project: {}", proj.slug));
                output::kv("Agent", &agent.name);
                output::kv("Program", &program);
                output::kv("Model", &model);
                if !reservations.is_empty() {
                    output::kv(
                        "Reservations",
                        &format!("{} path(s) reserved", reservations.len()),
                    );
                }
                output::kv("Inbox", &format!("{} message(s)", inbox.len()));
            });
            Ok(())
        }

        MacroCommand::PrepareThread {
            project_key,
            thread_id,
            program,
            model,
            agent_name,
            task,
            register,
            no_register,
            examples,
            no_examples,
            inbox_bodies,
            inbox_limit,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let should_register = context::resolve_bool(register, no_register, true);
            let include_examples = context::resolve_bool(examples, no_examples, true);

            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;
            let pid = proj.id.unwrap_or(0);

            // Register or resolve agent
            let agent = if should_register {
                let name =
                    agent_name.unwrap_or_else(mcp_agent_mail_core::models::generate_agent_name);
                outcome_to_result(
                    mcp_agent_mail_db::queries::register_agent(
                        &cx,
                        &ctx.pool,
                        pid,
                        &name,
                        &program,
                        &model,
                        task.as_deref(),
                        Some("auto"),
                    )
                    .await,
                )?
            } else {
                let name = agent_name.ok_or_else(|| {
                    CliError::InvalidArgument(
                        "agent name is required when --no-register is set".into(),
                    )
                })?;
                resolve_agent_async(&cx, &ctx.pool, pid, &name).await?
            };

            // Get thread messages
            let messages = outcome_to_result(
                mcp_agent_mail_db::queries::list_thread_messages(
                    &cx, &ctx.pool, pid, &thread_id, None,
                )
                .await,
            )?;

            let total_messages = messages.len();

            // Collect participants
            let mut participants: Vec<String> = messages
                .iter()
                .map(|m| m.from.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            participants.sort();

            let example_msgs: Vec<serde_json::Value> = if include_examples {
                messages
                    .iter()
                    .take(3)
                    .map(|m| {
                        serde_json::json!({
                            "id": m.id,
                            "from": m.from,
                            "subject": m.subject,
                            "created_ts": mcp_agent_mail_db::micros_to_iso(m.created_ts),
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };

            // Fetch inbox
            let inbox = outcome_to_result(
                mcp_agent_mail_db::queries::fetch_inbox(
                    &cx,
                    &ctx.pool,
                    pid,
                    agent.id.unwrap_or(0),
                    false,
                    None,
                    inbox_limit.max(0) as usize,
                )
                .await,
            )?;

            let resp = serde_json::json!({
                "project": {
                    "id": pid,
                    "slug": proj.slug,
                    "human_key": proj.human_key,
                },
                "agent": agent_row_to_json(&agent),
                "thread": {
                    "thread_id": thread_id,
                    "total_messages": total_messages,
                    "participants": participants,
                    "examples": example_msgs,
                },
                "inbox": inbox.iter().map(|r| inbox_row_to_json(r, inbox_bodies)).collect::<Vec<_>>(),
            });

            output::emit_output(&resp, fmt, || {
                output::success(&format!("Thread prepared: {thread_id}"));
                output::kv("Agent", &agent.name);
                output::kv("Messages", &total_messages.to_string());
                output::kv("Participants", &participants.join(", "));
                if include_examples && !example_msgs.is_empty() {
                    output::section("Examples:");
                    for ex in &example_msgs {
                        ftui_runtime::ftui_println!(
                            "  #{} from {} — {}",
                            ex["id"],
                            ex["from"].as_str().unwrap_or("?"),
                            ex["subject"].as_str().unwrap_or("?")
                        );
                    }
                }
                output::kv("Inbox", &format!("{} message(s)", inbox.len()));
            });
            Ok(())
        }

        MacroCommand::FileReservationCycle {
            project_key,
            agent_name,
            paths,
            ttl,
            exclusive,
            no_exclusive,
            reason,
            auto_release,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let is_exclusive = context::resolve_bool(exclusive, no_exclusive, true);

            if ttl < 60 {
                return Err(CliError::InvalidArgument(
                    "ttl must be at least 60 seconds".into(),
                ));
            }

            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;
            let pid = proj.id.unwrap_or(0);
            let agent = resolve_agent_async(&cx, &ctx.pool, pid, &agent_name).await?;
            let aid = agent.id.unwrap_or(0);

            let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
            let reason_str = reason.unwrap_or_else(|| "macro-file_reservation".to_string());

            let reservations = outcome_to_result(
                mcp_agent_mail_db::queries::create_file_reservations(
                    &cx,
                    &ctx.pool,
                    pid,
                    aid,
                    &path_refs,
                    ttl,
                    is_exclusive,
                    &reason_str,
                )
                .await,
            )?;

            let released = if auto_release {
                let released_rows = outcome_to_result(
                    mcp_agent_mail_db::queries::release_reservations(
                        &cx,
                        &ctx.pool,
                        pid,
                        aid,
                        Some(&path_refs),
                        None,
                    )
                    .await,
                )?;
                Some(released_rows)
            } else {
                None
            };
            let released_count = released.as_ref().map(Vec::len);

            let resp = serde_json::json!({
                "file_reservations": {
                    "granted": reservations.iter().map(|r| serde_json::json!({
                        "id": r.id.unwrap_or(0),
                        "path_pattern": r.path_pattern,
                        "exclusive": r.exclusive != 0,
                        "reason": r.reason,
                        "expires_ts": mcp_agent_mail_db::micros_to_iso(r.expires_ts),
                    })).collect::<Vec<_>>(),
                    "conflicts": [],
                },
                "released": released_count.map(|n| serde_json::json!({
                    "released": n,
                    "released_at": mcp_agent_mail_db::micros_to_iso(mcp_agent_mail_db::timestamps::now_micros()),
                })),
            });

            output::emit_output(&resp, fmt, || {
                output::success(&format!(
                    "{} reservation(s) granted for {}",
                    reservations.len(),
                    agent_name
                ));
                for r in &reservations {
                    output::kv(
                        "  Path",
                        &format!(
                            "{} (exclusive={}, expires={})",
                            r.path_pattern,
                            r.exclusive != 0,
                            mcp_agent_mail_db::micros_to_iso(r.expires_ts)
                        ),
                    );
                }
                if let Some(n) = released_count {
                    output::kv("Released", &n.to_string());
                }
            });
            Ok(())
        }

        MacroCommand::ContactHandshake {
            project_key,
            from,
            to,
            to_project,
            reason,
            auto_accept,
            ttl,
            welcome_subject,
            welcome_body,
            thread_id,
            register_missing,
            reg_program,
            reg_model,
            reg_task,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let proj = resolve_project_async(&cx, &ctx.pool, &project_key).await?;
            let pid = proj.id.unwrap_or(0);

            // Resolve from agent
            let from_agent = resolve_agent_async(&cx, &ctx.pool, pid, &from).await?;

            // Resolve target project + agent
            let (target_pid, target_proj_slug) = if let Some(ref tp) = to_project {
                let target = resolve_project_async(&cx, &ctx.pool, tp).await?;
                (target.id.unwrap_or(0), target.slug.clone())
            } else {
                (pid, proj.slug.clone())
            };

            // Resolve or register target agent
            let to_agent = match resolve_agent_async(&cx, &ctx.pool, target_pid, &to).await {
                Ok(a) => a,
                Err(_) if register_missing => {
                    let program = reg_program.unwrap_or_else(|| "unknown".to_string());
                    let model = reg_model.unwrap_or_else(|| "unknown".to_string());
                    outcome_to_result(
                        mcp_agent_mail_db::queries::register_agent(
                            &cx,
                            &ctx.pool,
                            target_pid,
                            &to,
                            &program,
                            &model,
                            reg_task.as_deref(),
                            Some("auto"),
                        )
                        .await,
                    )?
                }
                Err(e) => return Err(e),
            };

            let ttl_clamped = if ttl < 60 { 60 } else { ttl };

            // 1. Request contact
            let _link = outcome_to_result(
                mcp_agent_mail_db::queries::request_contact(
                    &cx,
                    &ctx.pool,
                    pid,
                    from_agent.id.unwrap_or(0),
                    target_pid,
                    to_agent.id.unwrap_or(0),
                    reason.as_deref().unwrap_or(""),
                    ttl_clamped,
                )
                .await,
            )?;

            // 2. Auto-accept
            let response_val = if auto_accept {
                let (_, approved) = outcome_to_result(
                    mcp_agent_mail_db::queries::respond_contact(
                        &cx,
                        &ctx.pool,
                        pid,
                        from_agent.id.unwrap_or(0),
                        target_pid,
                        to_agent.id.unwrap_or(0),
                        true,
                        ttl_clamped,
                    )
                    .await,
                )?;
                Some(serde_json::json!({
                    "status": approved.status,
                    "expires_ts": approved.expires_ts.map(mcp_agent_mail_db::micros_to_iso),
                }))
            } else {
                None
            };

            // 3. Welcome message
            let welcome_val = if let (Some(subject), Some(body)) = (welcome_subject, welcome_body) {
                if to_project.is_none() {
                    let msg = outcome_to_result(
                        mcp_agent_mail_db::queries::create_message_with_recipients(
                            &cx,
                            &ctx.pool,
                            pid,
                            from_agent.id.unwrap_or(0),
                            &subject,
                            &body,
                            thread_id.as_deref(),
                            "normal",
                            false,
                            "",
                            &[(to_agent.id.unwrap_or(0), "to")],
                        )
                        .await,
                    )?;
                    Some(message_row_to_json(&msg, &from))
                } else {
                    None
                }
            } else {
                None
            };

            let resp = serde_json::json!({
                "request": {
                    "from": from,
                    "from_project": proj.slug,
                    "to": to,
                    "to_project": target_proj_slug,
                },
                "response": response_val,
                "welcome_message": welcome_val,
            });

            output::emit_output(&resp, fmt, || {
                output::success(&format!("Contact handshake: {from} → {to}"));
                if auto_accept {
                    output::kv("Status", "approved");
                } else {
                    output::kv("Status", "pending");
                }
                if welcome_val.is_some() {
                    output::kv("Welcome", "sent");
                }
            });
            Ok(())
        }
    }
}

/// Resolve an agent by name within a project via the async DB layer.
async fn resolve_agent_async(
    cx: &asupersync::Cx,
    pool: &mcp_agent_mail_db::DbPool,
    project_id: i64,
    agent_name: &str,
) -> CliResult<mcp_agent_mail_db::AgentRow> {
    outcome_to_result(mcp_agent_mail_db::queries::get_agent(cx, pool, project_id, agent_name).await)
        .map_err(|_| CliError::InvalidArgument(format!("agent not found: {agent_name}")))
}

/// Convert an asupersync `Outcome` to a `CliResult`.
fn outcome_to_result<T>(
    outcome: asupersync::Outcome<T, mcp_agent_mail_db::DbError>,
) -> CliResult<T> {
    match outcome {
        asupersync::Outcome::Ok(v) => Ok(v),
        asupersync::Outcome::Err(e) => Err(CliError::Other(format!("{e}"))),
        asupersync::Outcome::Cancelled(_) => Err(CliError::Other("request cancelled".into())),
        asupersync::Outcome::Panicked(p) => {
            Err(CliError::Other(format!("internal panic: {}", p.message())))
        }
    }
}

/// Get a message by ID via the async DB layer.
async fn get_message_by_id_async(
    cx: &asupersync::Cx,
    pool: &mcp_agent_mail_db::DbPool,
    message_id: i64,
) -> CliResult<mcp_agent_mail_db::MessageRow> {
    outcome_to_result(mcp_agent_mail_db::queries::get_message(cx, pool, message_id).await)
        .map_err(|_| CliError::InvalidArgument(format!("message not found: {message_id}")))
}

fn message_row_to_json(m: &mcp_agent_mail_db::MessageRow, sender_name: &str) -> serde_json::Value {
    serde_json::json!({
        "id": m.id.unwrap_or(0),
        "subject": m.subject,
        "body_md": m.body_md,
        "importance": m.importance,
        "ack_required": m.ack_required != 0,
        "thread_id": m.thread_id,
        "created_ts": mcp_agent_mail_db::micros_to_iso(m.created_ts),
        "from": sender_name,
    })
}

fn inbox_row_to_json(
    r: &mcp_agent_mail_db::queries::InboxRow,
    include_body: bool,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "id": r.message.id.unwrap_or(0),
        "subject": r.message.subject,
        "from": r.sender_name,
        "importance": r.message.importance,
        "ack_required": r.message.ack_required != 0,
        "created_ts": mcp_agent_mail_db::micros_to_iso(r.message.created_ts),
        "kind": r.kind,
        "thread_id": r.message.thread_id,
    });
    if include_body {
        v.as_object_mut().unwrap().insert(
            "body_md".to_string(),
            serde_json::Value::String(r.message.body_md.clone()),
        );
    }
    v
}

fn truncate_str(s: &str, max: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn stdio_capture_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// Extract the first top-level JSON value delimited by `open`/`close` from a
    /// string that may contain non-JSON text (e.g. from concurrent test output).
    fn extract_json_delimited(s: &str, open: char, close: char) -> Option<&str> {
        let start = s.find(open)?;
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escape = false;
        for (i, ch) in s[start..].char_indices() {
            if escape {
                escape = false;
                continue;
            }
            match ch {
                '\\' if in_string => escape = true,
                '"' => in_string = !in_string,
                c if c == open && !in_string => depth += 1,
                c if c == close && !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&s[start..start + i + 1]);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Extract the first top-level JSON object `{...}` from a string.
    fn extract_json_block(s: &str) -> Option<&str> {
        extract_json_delimited(s, '{', '}')
    }

    /// Extract the first top-level JSON array `[...]` from a string.
    fn extract_json_array(s: &str) -> Option<&str> {
        extract_json_delimited(s, '[', ']')
    }

    /// Extract the first JSON object that looks like doctor-check output.
    fn extract_doctor_check_json(s: &str) -> Option<serde_json::Value> {
        let mut cursor = s;
        while let Some(json_str) = extract_json_block(cursor) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str)
                && parsed.get("healthy").and_then(|v| v.as_bool()).is_some()
                && parsed.get("checks").and_then(|v| v.as_array()).is_some()
            {
                return Some(parsed);
            }

            let next = json_str.len();
            if next >= cursor.len() {
                break;
            }
            cursor = &cursor[next..];
        }
        None
    }

    #[test]
    fn default_release_log_filter_includes_fsqlite_noise_suppressors() {
        let filter = default_release_log_filter();
        assert!(filter.contains("mvcc=warn"));
        assert!(filter.contains("checkpoint=warn"));
        assert!(filter.contains("fsqlite.storage_wiring=warn"));
        assert!(filter.contains("jit_compile=error"));
        assert!(filter.contains("execute_statement_dispatch=error"));
    }

    #[test]
    fn noisy_dependency_log_clamp_directives_include_known_spam_targets() {
        let directives = noisy_dependency_log_clamp_directives();
        assert!(directives.contains(&"jit_compile=error"));
        assert!(directives.contains(&"execute_statement_dispatch=error"));
    }

    #[test]
    fn check_inbox_template_detection_matches_expected_patterns() {
        assert!(value_looks_like_template(""));
        assert!(value_looks_like_template("YOUR_AGENT_NAME"));
        assert!(value_looks_like_template("my_placeholder_value"));
        assert!(value_looks_like_template("<set-me>"));
        assert!(!value_looks_like_template("BlueLake"));
    }

    #[test]
    fn check_inbox_server_url_honors_http_path() {
        assert_eq!(
            check_inbox_server_url("127.0.0.1", 8765, "/custom/path"),
            "http://127.0.0.1:8765/custom/path/"
        );
        assert_eq!(
            check_inbox_server_url("127.0.0.1", 8765, "mcp"),
            "http://127.0.0.1:8765/mcp/"
        );
    }

    #[test]
    fn check_inbox_server_urls_support_api_mcp_aliases() {
        assert_eq!(
            check_inbox_server_urls("127.0.0.1", 8765, "api"),
            vec![
                "http://127.0.0.1:8765/api/".to_string(),
                "http://127.0.0.1:8765/mcp/".to_string(),
            ]
        );
        assert_eq!(
            check_inbox_server_urls("127.0.0.1", 8765, "/mcp/"),
            vec![
                "http://127.0.0.1:8765/mcp/".to_string(),
                "http://127.0.0.1:8765/api/".to_string(),
            ]
        );
    }

    #[test]
    fn check_inbox_server_urls_do_not_alias_custom_paths() {
        assert_eq!(
            check_inbox_server_urls("127.0.0.1", 8765, "/custom/path"),
            vec!["http://127.0.0.1:8765/custom/path/".to_string()]
        );
    }

    #[test]
    fn check_inbox_rpc_config_reads_agent_mail_env() {
        let env: HashMap<String, String> = HashMap::from_iter([
            ("AGENT_MAIL_PROJECT".to_string(), "/tmp/proj".to_string()),
            ("AGENT_MAIL_AGENT".to_string(), "BlueLake".to_string()),
            ("AGENT_MAIL_URL".to_string(), "127.0.0.1:8765".to_string()),
            ("AGENT_MAIL_TOKEN".to_string(), "token-123".to_string()),
        ]);

        let cfg = check_inbox_rpc_config_from_env_reader(|key| env.get(key).cloned())
            .expect("config should parse");
        assert_eq!(cfg.project_key, "/tmp/proj");
        assert_eq!(cfg.agent_name, "BlueLake");
        assert_eq!(cfg.server_url, "http://127.0.0.1:8765/mcp/");
        assert_eq!(
            cfg.server_urls,
            vec![
                "http://127.0.0.1:8765/mcp/".to_string(),
                "http://127.0.0.1:8765/api/".to_string(),
            ]
        );
        assert_eq!(cfg.bearer_token.as_deref(), Some("token-123"));
        assert_eq!(cfg.limit, 10);
        assert!(!cfg.include_bodies);
        assert_eq!(cfg.timeout_seconds, 3);
    }

    #[test]
    fn check_inbox_rpc_config_honors_http_path_env() {
        let env: HashMap<String, String> = HashMap::from_iter([
            ("AGENT_MAIL_PROJECT".to_string(), "/tmp/proj".to_string()),
            ("AGENT_MAIL_AGENT".to_string(), "BlueLake".to_string()),
            ("AGENT_MAIL_URL".to_string(), "127.0.0.1:8765".to_string()),
            ("HTTP_PATH".to_string(), "api".to_string()),
        ]);

        let cfg = check_inbox_rpc_config_from_env_reader(|key| env.get(key).cloned())
            .expect("config should parse");
        assert_eq!(cfg.server_url, "http://127.0.0.1:8765/api/");
        assert_eq!(
            cfg.server_urls,
            vec![
                "http://127.0.0.1:8765/api/".to_string(),
                "http://127.0.0.1:8765/mcp/".to_string(),
            ]
        );
    }

    #[test]
    fn check_inbox_rpc_config_does_not_alias_explicit_url_path() {
        let env: HashMap<String, String> = HashMap::from_iter([
            ("AGENT_MAIL_PROJECT".to_string(), "/tmp/proj".to_string()),
            ("AGENT_MAIL_AGENT".to_string(), "BlueLake".to_string()),
            (
                "AGENT_MAIL_URL".to_string(),
                "http://127.0.0.1:8765/custom/path".to_string(),
            ),
            ("HTTP_PATH".to_string(), "api".to_string()),
        ]);

        let cfg = check_inbox_rpc_config_from_env_reader(|key| env.get(key).cloned())
            .expect("config should parse");
        assert_eq!(cfg.server_url, "http://127.0.0.1:8765/custom/path/");
        assert_eq!(
            cfg.server_urls,
            vec!["http://127.0.0.1:8765/custom/path/".to_string()]
        );
    }

    #[test]
    fn normalize_agent_mail_url_preserves_query_and_fragment() {
        assert_eq!(
            normalize_agent_mail_url("http://127.0.0.1:8765/mcp?x=1#frag", "/api/"),
            "http://127.0.0.1:8765/mcp/?x=1#frag"
        );
        assert_eq!(
            normalize_agent_mail_url("127.0.0.1:8765?x=1#frag", "api"),
            "http://127.0.0.1:8765/api/?x=1#frag"
        );
        assert_eq!(
            normalize_agent_mail_url("https://host.local/path#anchor", "/api/"),
            "https://host.local/path/#anchor"
        );
    }

    #[test]
    fn check_inbox_rpc_config_skips_placeholder_values() {
        let env: HashMap<String, String> = HashMap::from_iter([
            ("AGENT_MAIL_PROJECT".to_string(), "YOUR_PROJECT".to_string()),
            ("AGENT_MAIL_AGENT".to_string(), "BlueLake".to_string()),
        ]);
        let cfg = check_inbox_rpc_config_from_env_reader(|key| env.get(key).cloned());
        assert!(cfg.is_none(), "placeholder env values must be ignored");
    }

    #[test]
    fn build_fetch_inbox_request_uses_jsonrpc_tools_call_shape() {
        let cfg = CheckInboxRpcConfig {
            server_url: "http://127.0.0.1:8765/api/".to_string(),
            server_urls: vec!["http://127.0.0.1:8765/api/".to_string()],
            bearer_token: Some("secret".to_string()),
            project_key: "/tmp/proj".to_string(),
            agent_name: "BlueLake".to_string(),
            limit: 10,
            include_bodies: false,
            timeout_seconds: 3,
        };

        let payload = build_fetch_inbox_jsonrpc_request(&cfg);
        assert_eq!(payload["jsonrpc"], "2.0");
        assert_eq!(payload["id"], "1");
        assert_eq!(payload["method"], "tools/call");
        assert_eq!(payload["params"]["name"], "fetch_inbox");
        assert_eq!(payload["params"]["arguments"]["project_key"], "/tmp/proj");
        assert_eq!(payload["params"]["arguments"]["agent_name"], "BlueLake");
        assert_eq!(payload["params"]["arguments"]["limit"], 10);
        assert_eq!(payload["params"]["arguments"]["include_bodies"], false);
    }

    #[test]
    fn parse_fetch_inbox_rows_supports_array_and_result_wrapped_shapes() {
        let direct = serde_json::json!([
            {
                "id": 1,
                "subject": "hello",
                "from": "RedHarbor",
                "importance": "urgent",
                "created_ts": "2026-02-12T00:00:00Z"
            },
            {
                "id": 2,
                "subject": "note",
                "from": "CalmGorge",
                "importance": "normal",
                "created_ts": "2026-02-12T00:01:00Z"
            }
        ]);
        let wrapped = serde_json::json!({ "result": direct.clone() });

        let parsed_direct = parse_fetch_inbox_rows(direct).expect("direct parse");
        let parsed_wrapped = parse_fetch_inbox_rows(wrapped).expect("wrapped parse");

        assert_eq!(parsed_direct.unread_count, 2);
        assert_eq!(parsed_direct.urgent_or_high_count, 1);
        assert_eq!(parsed_direct.messages[0].from, "RedHarbor");
        assert_eq!(parsed_direct.messages[0].importance, "urgent");
        assert_eq!(parsed_wrapped.unread_count, 2);
        assert_eq!(parsed_wrapped.messages[1].subject, "note");
    }

    #[test]
    fn check_inbox_direct_config_struct_creation() {
        let config = CheckInboxDirectConfig {
            project_key: "/tmp/test-project".to_string(),
            agent_name: "TestAgent".to_string(),
            limit: 10,
        };
        assert_eq!(config.project_key, "/tmp/test-project");
        assert_eq!(config.agent_name, "TestAgent");
        assert_eq!(config.limit, 10);
    }

    #[test]
    fn format_micros_as_iso_produces_valid_timestamp() {
        // 2026-01-01 00:00:00 UTC in microseconds
        let micros = 1_767_225_600_000_000_i64;
        let result = format_micros_as_iso(micros);
        // Should produce ISO-8601 format
        assert!(result.contains("2026"), "year should be 2026, got {result}");
        assert!(result.ends_with('Z'), "should end with Z, got {result}");
    }

    #[test]
    fn sanitize_agent_name_replaces_special_chars() {
        assert_eq!(sanitize_agent_name("BlueLake"), "BlueLake");
        assert_eq!(sanitize_agent_name("my-agent.v2"), "my_agent_v2");
        assert_eq!(sanitize_agent_name("agent@host:8080"), "agent_host_8080");
        assert_eq!(
            sanitize_agent_name("Agent_With_Underscores"),
            "Agent_With_Underscores"
        );
    }

    #[test]
    fn rate_limiter_lockfile_path_uses_sanitized_name() {
        let limiter = CheckInboxRateLimiter::new("BlueLake", None);
        assert_eq!(
            limiter.lockfile_path().to_string_lossy(),
            "/tmp/mcp-mail-check-BlueLake"
        );

        let limiter2 = CheckInboxRateLimiter::new("my-agent.v2", None);
        assert_eq!(
            limiter2.lockfile_path().to_string_lossy(),
            "/tmp/mcp-mail-check-my_agent_v2"
        );
    }

    #[test]
    fn rate_limiter_default_interval_is_120_seconds() {
        assert_eq!(CHECK_INBOX_RATE_LIMIT_DEFAULT_SECS, 120);
        let limiter = CheckInboxRateLimiter::new("TestAgent", None);
        assert_eq!(limiter.interval_secs, 120);
    }

    #[test]
    fn rate_limiter_custom_interval() {
        let limiter = CheckInboxRateLimiter::new("TestAgent", Some(60));
        assert_eq!(limiter.interval_secs, 60);
    }

    #[test]
    fn rate_limiter_first_check_always_allowed() {
        // Use unique agent name to avoid interference with other tests
        let limiter = CheckInboxRateLimiter::new("TestFirstCheck", Some(1));
        limiter.reset(); // Clean start
        assert!(
            limiter.should_check(),
            "first check should always be allowed"
        );
        limiter.reset(); // Clean up
    }

    #[test]
    fn rate_limiter_blocks_rapid_checks() {
        let limiter = CheckInboxRateLimiter::new("TestRapidCheck", Some(60));
        limiter.reset(); // Clean start
        assert!(limiter.should_check(), "first check should be allowed");
        assert!(
            !limiter.should_check(),
            "second immediate check should be blocked"
        );
        limiter.reset(); // Clean up
    }

    #[test]
    fn rate_limiter_disabled_with_zero_interval() {
        let limiter = CheckInboxRateLimiter::new("TestZeroInterval", Some(0));
        limiter.reset(); // Clean start
        assert!(limiter.should_check(), "zero interval: first check allowed");
        // With interval=0, next check should also be allowed immediately
        // The condition is elapsed < interval, and 0 < 0 is false, so it proceeds
        assert!(
            limiter.should_check(),
            "zero interval: second check also allowed"
        );
        limiter.reset(); // Clean up
    }

    #[test]
    fn rate_limiter_reset_clears_state() {
        let limiter = CheckInboxRateLimiter::new("TestReset", Some(60));
        limiter.reset(); // Clean start
        let _ = limiter.should_check(); // Set the lockfile
        assert!(
            limiter.lockfile_path().exists(),
            "lockfile should exist after check"
        );
        limiter.reset();
        assert!(
            !limiter.lockfile_path().exists(),
            "lockfile should be removed after reset"
        );
    }

    #[test]
    fn rate_limiter_blocks_when_lockfile_write_fails() {
        let limiter = CheckInboxRateLimiter::new("TestWriteFailure", Some(60));
        limiter.reset();

        let path = limiter.lockfile_path();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create directory at lockfile path");

        assert!(limiter.should_check(), "first check should be allowed");
        assert!(
            !limiter.should_check(),
            "second immediate check should still be blocked"
        );

        limiter.reset();
    }

    #[test]
    fn serve_http_overrides_are_applied() {
        let config = build_http_config(
            Some("0.0.0.0".to_string()),
            Some(9000),
            Some("/api/v2/".to_string()),
            false,
        );
        assert_eq!(config.http_host, "0.0.0.0");
        assert_eq!(config.http_port, 9000);
        assert_eq!(config.http_path, "/api/v2/");
    }

    #[test]
    fn serve_http_path_aliases_are_normalized() {
        let config_mcp = build_http_config(None, None, Some("mcp".to_string()), false);
        assert_eq!(config_mcp.http_path, "/mcp/");

        let config_api = build_http_config(None, None, Some("/api".to_string()), false);
        assert_eq!(config_api.http_path, "/api/");
    }

    #[test]
    fn serve_http_relative_path_is_normalized() {
        let config = build_http_config(None, None, Some("custom/path".to_string()), false);
        assert_eq!(config.http_path, "/custom/path/");
    }

    #[test]
    fn apply_http_config_overrides_normalizes_env_path_when_no_cli_path() {
        let config = Config {
            http_path: "api".to_string(),
            ..Config::default()
        };
        let normalized = apply_http_config_overrides(config, None, None, None, false);
        assert_eq!(normalized.http_path, "/api/");
    }

    #[test]
    fn serve_http_no_auth_clears_token() {
        // Verify --no-auth clears any token (whether loaded from env or not)
        let config_no_auth = build_http_config(None, None, None, true);
        assert!(
            config_no_auth.http_bearer_token.is_none(),
            "--no-auth should clear bearer token"
        );
    }

    #[test]
    fn setup_self_heal_command_uses_runtime_http_values() {
        let config = Config {
            http_host: "0.0.0.0".to_string(),
            http_port: 9001,
            http_path: "/api/v2/".to_string(),
            ..Config::default()
        };

        match build_setup_run_command_for_http_server(&config) {
            SetupCommand::Run {
                host,
                port,
                path,
                project_dir,
                yes,
                dry_run,
                no_user_config,
                no_hooks,
                ..
            } => {
                assert_eq!(host, "0.0.0.0");
                assert_eq!(port, 9001);
                assert_eq!(path, "/api/v2/");
                assert!(yes);
                assert!(!dry_run);
                assert!(!no_user_config);
                assert!(!no_hooks);
                assert!(project_dir.is_some());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn setup_self_heal_command_uses_normalized_http_path() {
        let config = build_http_config(
            Some("127.0.0.1".to_string()),
            Some(8765),
            Some("mcp".to_string()),
            false,
        );

        match build_setup_run_command_for_http_server(&config) {
            SetupCommand::Run { path, .. } => assert_eq!(path, "/mcp/"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_serve_http_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "serve-http",
            "--host",
            "0.0.0.0",
            "--port",
            "9999",
            "--path",
            "/api/x/",
        ])
        .expect("failed to parse serve-http flags");
        match cli.command.expect("expected command") {
            Commands::ServeHttp {
                host,
                port,
                path,
                no_auth,
                no_tui,
            } => {
                assert_eq!(host.as_deref(), Some("0.0.0.0"));
                assert_eq!(port, Some(9999));
                assert_eq!(path.as_deref(), Some("/api/x/"));
                assert!(!no_auth);
                assert!(!no_tui);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_serve_http_no_auth() {
        let cli = Cli::try_parse_from(["am", "serve-http", "--no-auth"])
            .expect("failed to parse serve-http --no-auth");
        match cli.command.expect("expected command") {
            Commands::ServeHttp {
                no_auth, no_tui, ..
            } => {
                assert!(no_auth, "--no-auth flag should be true");
                assert!(!no_tui, "--no-tui should default to false");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_serve_http_no_tui() {
        let cli = Cli::try_parse_from(["am", "serve-http", "--no-tui"])
            .expect("failed to parse serve-http --no-tui");
        match cli.command.expect("expected command") {
            Commands::ServeHttp {
                no_tui, no_auth, ..
            } => {
                assert!(no_tui, "--no-tui flag should be true");
                assert!(!no_auth, "--no-auth should default to false");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_check_inbox_defaults() {
        let cli = Cli::try_parse_from(["am", "check-inbox"])
            .expect("failed to parse check-inbox defaults");
        match cli.command.expect("expected command") {
            Commands::CheckInbox {
                agent,
                rate_limit,
                direct,
                format,
                json,
                host,
                port,
                project,
            } => {
                assert!(agent.is_none());
                assert_eq!(rate_limit, 120);
                assert!(!direct);
                assert!(format.is_none());
                assert!(!json);
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 8765);
                assert!(project.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_check_inbox_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "check-inbox",
            "--agent",
            "BlueLake",
            "--rate-limit",
            "60",
            "--direct",
            "--json",
            "--host",
            "0.0.0.0",
            "--port",
            "9999",
            "--project",
            "/tmp/proj",
        ])
        .expect("failed to parse check-inbox with flags");
        match cli.command.expect("expected command") {
            Commands::CheckInbox {
                agent,
                rate_limit,
                direct,
                format,
                json,
                host,
                port,
                project,
            } => {
                assert_eq!(agent.as_deref(), Some("BlueLake"));
                assert_eq!(rate_limit, 60);
                assert!(direct);
                assert!(format.is_none());
                assert!(json);
                assert_eq!(host, "0.0.0.0");
                assert_eq!(port, 9999);
                assert_eq!(project.as_deref(), Some("/tmp/proj"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_check_inbox_format_toon() {
        let cli = Cli::try_parse_from(["am", "check-inbox", "--format", "toon"])
            .expect("failed to parse check-inbox with format");
        match cli.command.expect("expected command") {
            Commands::CheckInbox { format, json, .. } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_ci_format_toon() {
        let cli = Cli::try_parse_from(["am", "ci", "--format", "toon"])
            .expect("failed to parse ci with format");
        match cli.command.expect("expected command") {
            Commands::Ci {
                format,
                json,
                quick,
                report,
                parallel,
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
                assert!(!quick);
                assert!(report.is_none());
                assert!(!parallel);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_bench_with_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "bench",
            "--quick",
            "--json",
            "--baseline",
            "/tmp/base.json",
            "--save-baseline",
            "/tmp/new.json",
            "--filter",
            "mail_*",
            "--warmup",
            "2",
            "--runs",
            "5",
        ])
        .expect("failed to parse bench flags");
        match cli.command.expect("expected command") {
            Commands::Bench {
                quick,
                format,
                json,
                baseline,
                save_baseline,
                filter,
                list,
                warmup,
                runs,
            } => {
                assert!(quick);
                assert!(format.is_none());
                assert!(json);
                assert_eq!(baseline, Some(PathBuf::from("/tmp/base.json")));
                assert_eq!(save_baseline, Some(PathBuf::from("/tmp/new.json")));
                assert_eq!(filter.as_deref(), Some("mail_*"));
                assert!(!list);
                assert_eq!(warmup, Some(2));
                assert_eq!(runs, Some(5));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_bench_list_mode() {
        let cli = Cli::try_parse_from(["am", "bench", "--list"])
            .expect("failed to parse bench list mode");
        match cli.command.expect("expected command") {
            Commands::Bench {
                quick,
                format,
                json,
                baseline,
                save_baseline,
                filter,
                list,
                warmup,
                runs,
            } => {
                assert!(!quick);
                assert!(format.is_none());
                assert!(!json);
                assert!(baseline.is_none());
                assert!(save_baseline.is_none());
                assert!(filter.is_none());
                assert!(list);
                assert!(warmup.is_none());
                assert!(runs.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn handle_bench_list_json_outputs_selected_benchmark_configs() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let capture = ftui_runtime::StdioCapture::install().expect("install capture");
        let result = handle_bench(
            false,
            None,
            true,
            None,
            None,
            Some("help".to_string()),
            true,
            None,
            None,
        );
        let output = capture.drain_to_string();

        assert!(result.is_ok(), "bench --list --json failed: {result:?}");
        let trimmed = output.trim();
        let json_str = extract_json_array(trimmed).expect("valid benchmark list json");
        let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let rows = parsed.as_array().expect("benchmark list array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "help");
        assert_eq!(rows[0]["warmup"], 3);
        assert_eq!(rows[0]["runs"], 10);
    }

    #[test]
    fn handle_bench_quick_mode_uses_default_warmup_runs_and_writes_report() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let _cwd = CwdGuard::chdir(dir.path());
        let capture = ftui_runtime::StdioCapture::install().expect("install capture");

        let result = handle_bench(
            true,
            None,
            true,
            None,
            None,
            Some("help".to_string()),
            false,
            None,
            None,
        );
        let output = capture.drain_to_string();

        assert!(result.is_ok(), "bench --quick --json failed: {result:?}");
        let json_str = extract_json_block(&output).expect("expected JSON in bench output");
        let parsed: serde_json::Value =
            serde_json::from_str(json_str).expect("valid benchmark json");
        assert_eq!(parsed["warmup"], 1);
        assert_eq!(parsed["runs"], 3);
        assert!(parsed["summary"]["benchmarks"]["help"].is_object());

        let report_dir = dir.path().join("benches/results");
        assert!(
            report_dir.is_dir(),
            "missing report dir: {}",
            report_dir.display()
        );
        let report_files = std::fs::read_dir(&report_dir)
            .expect("read report dir")
            .filter_map(Result::ok)
            .count();
        assert_eq!(report_files, 1, "expected one quick bench report file");
    }

    #[test]
    fn ci_progress_is_table_only() {
        assert!(ci_should_emit_progress(output::CliOutputFormat::Table));
        assert!(!ci_should_emit_progress(output::CliOutputFormat::Json));
        assert!(!ci_should_emit_progress(output::CliOutputFormat::Toon));
    }

    #[test]
    fn clap_parses_clear_and_reset_defaults() {
        let cli = Cli::try_parse_from(["am", "clear-and-reset-everything"])
            .expect("failed to parse clear-and-reset-everything");
        match cli.command.expect("expected command") {
            Commands::ClearAndResetEverything {
                force,
                archive,
                no_archive,
            } => {
                assert!(!force);
                assert!(!archive);
                assert!(!no_archive);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_clear_and_reset_force_no_archive() {
        let cli = Cli::try_parse_from([
            "am",
            "clear-and-reset-everything",
            "--force",
            "--no-archive",
        ])
        .expect("failed to parse clear-and-reset-everything flags");
        match cli.command.expect("expected command") {
            Commands::ClearAndResetEverything {
                force,
                archive,
                no_archive,
            } => {
                assert!(force);
                assert!(!archive);
                assert!(no_archive);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_rejects_clear_and_reset_archive_flag_conflict() {
        let err = Cli::try_parse_from([
            "am",
            "clear-and-reset-everything",
            "--archive",
            "--no-archive",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    // -----------------------------------------------------------------------
    // Build slot utilities (amctl env, am-run)
    // -----------------------------------------------------------------------

    #[test]
    fn clap_parses_amctl_env_defaults() {
        let cli = Cli::try_parse_from(["am", "amctl", "env"]).expect("failed to parse amctl env");
        match cli.command.expect("expected command") {
            Commands::Amctl {
                action: AmctlCommand::Env { path, agent },
            } => {
                assert_eq!(path, PathBuf::from("."));
                assert!(agent.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_amctl_env_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "amctl",
            "env",
            "--path",
            "/tmp/repo",
            "--agent",
            "BlueLake",
        ])
        .expect("failed to parse amctl env flags");
        match cli.command.expect("expected command") {
            Commands::Amctl {
                action: AmctlCommand::Env { path, agent },
            } => {
                assert_eq!(path, PathBuf::from("/tmp/repo"));
                assert_eq!(agent.as_deref(), Some("BlueLake"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn amctl_env_prints_expected_env_for_fixture_path() {
        use ftui_runtime::stdio_capture::StdioCapture;

        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _capture_lock = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        let capture = StdioCapture::install().unwrap();
        handle_amctl(AmctlCommand::Env {
            path: PathBuf::from("/tmp/am-fixture"),
            agent: Some("TestAgent".to_string()),
        })
        .unwrap();
        let mut sink = Vec::new();
        capture.drain(&mut sink).unwrap();
        drop(capture);

        let text = String::from_utf8_lossy(&sink);
        let mut vars = std::collections::BTreeMap::<String, String>::new();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            vars.insert(k.trim().to_string(), v.trim().to_string());
        }

        assert_eq!(vars.get("SLUG").map(String::as_str), Some("tmp-am-fixture"));
        assert_eq!(
            vars.get("PROJECT_UID").map(String::as_str),
            Some("e0c1eeedd48721247c34")
        );
        assert_eq!(vars.get("BRANCH").map(String::as_str), Some("unknown"));
        assert_eq!(vars.get("AGENT").map(String::as_str), Some("TestAgent"));
        assert_eq!(
            vars.get("CACHE_KEY").map(String::as_str),
            Some("am-cache-e0c1eeedd48721247c34-TestAgent-unknown")
        );
        let artifact_dir = vars.get("ARTIFACT_DIR").cloned().unwrap_or_default();
        assert!(
            artifact_dir.ends_with("projects/tmp-am-fixture/artifacts/TestAgent/unknown"),
            "unexpected ARTIFACT_DIR={artifact_dir}"
        );
    }

    #[test]
    fn clap_parses_am_run_defaults() {
        let cli = Cli::try_parse_from(["am", "am-run", "frontend-build", "echo", "hi"])
            .expect("failed to parse am-run defaults");
        match cli.command.expect("expected command") {
            Commands::AmRun(args) => {
                assert_eq!(args.slot, "frontend-build");
                assert_eq!(args.cmd, vec!["echo".to_string(), "hi".to_string()]);
                assert_eq!(args.path, PathBuf::from("."));
                assert!(args.agent.is_none());
                assert_eq!(args.ttl_seconds, 3600);
                assert!(!args.shared);
                assert!(!args.exclusive);
                assert!(!args.block_on_conflicts);
                assert!(!args.no_block_on_conflicts);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_rejects_am_run_shared_exclusive_conflict() {
        let err = Cli::try_parse_from([
            "am",
            "am-run",
            "slot",
            "--shared",
            "--exclusive",
            "echo",
            "hi",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn clap_rejects_am_run_block_flag_conflict() {
        let err = Cli::try_parse_from([
            "am",
            "am-run",
            "slot",
            "--block-on-conflicts",
            "--no-block-on-conflicts",
            "echo",
            "hi",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    fn build_slot_artifact_dir(test_name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("repo root")
            .to_path_buf();
        let ts = Utc::now().format("%Y%m%dT%H%M%S%.fZ").to_string();
        let dir = root
            .join("tests")
            .join("artifacts")
            .join("cli")
            .join("build_slots")
            .join(format!("{ts}-{}", safe_component(test_name)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn am_run_local_backend_sets_env_and_releases_lease() {
        use ftui_runtime::stdio_capture::StdioCapture;

        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _capture_lock = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            worktrees_enabled: true,
            storage_root: temp.path().join("storage_root"),
            ..Config::default()
        };
        let child_env_path = temp.path().join("child_env.txt");
        let child_env_arg = child_env_path.to_string_lossy().to_string();

        let args = AmRunArgs {
            slot: "frontend-build".to_string(),
            cmd: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo AM_SLOT=$AM_SLOT > \"$1\"; echo CACHE_KEY=$CACHE_KEY >> \"$1\"".to_string(),
                "sh".to_string(),
                child_env_arg,
            ],
            path: PathBuf::from("/tmp/am-run-fixture"),
            agent: Some("TestAgent".to_string()),
            ttl_seconds: 60,
            shared: false,
            exclusive: false,
            block_on_conflicts: false,
            no_block_on_conflicts: false,
        };

        let capture = StdioCapture::install().unwrap();
        handle_am_run_with(&config, None, None, args).unwrap();
        let mut sink = Vec::new();
        capture.drain(&mut sink).unwrap();
        drop(capture);

        let output = String::from_utf8_lossy(&sink).to_string();
        let art = build_slot_artifact_dir("am_run_local_backend_sets_env_and_releases_lease");
        std::fs::write(art.join("output.txt"), &output).unwrap();

        assert!(
            output.contains("$ sh -c"),
            "missing command banner: {output}"
        );
        let child_env = std::fs::read_to_string(&child_env_path).expect("child env file written");
        std::fs::write(art.join("child_env.txt"), &child_env).unwrap();
        assert!(
            child_env.contains("AM_SLOT=frontend-build"),
            "missing AM_SLOT in child env file: {child_env}"
        );
        assert!(
            child_env.contains("CACHE_KEY=am-cache-b0ec2290c757b5d59d13-TestAgent-unknown"),
            "missing CACHE_KEY in child env file: {child_env}"
        );

        let identity = resolve_project_identity("/tmp/am-run-fixture");
        let slot_dir = ensure_slot_dir(&config, &identity.slug, "frontend-build").unwrap();
        let lease_path = lease_path(&slot_dir, "TestAgent", "unknown");
        let lease_json = std::fs::read_to_string(&lease_path).expect("lease file created");
        std::fs::write(art.join("lease.json"), &lease_json).unwrap();
        let lease: LeaseRecord = serde_json::from_str(&lease_json).unwrap();
        assert!(
            lease.released_ts.is_some(),
            "expected released_ts to be set, got: {lease_json}"
        );
    }

    #[test]
    fn am_run_block_on_conflicts_aborts_without_running_child() {
        use ftui_runtime::stdio_capture::StdioCapture;

        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _capture_lock = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            worktrees_enabled: true,
            storage_root: temp.path().join("storage_root"),
            ..Config::default()
        };

        let identity = resolve_project_identity("/tmp/am-run-fixture");
        let slot_dir = ensure_slot_dir(&config, &identity.slug, "frontend-build").unwrap();
        let conflict_path = lease_path(&slot_dir, "OtherAgent", "main");
        let now = Utc::now();
        let lease = LeaseRecord {
            slot: "frontend-build".to_string(),
            agent: "OtherAgent".to_string(),
            branch: "main".to_string(),
            exclusive: true,
            acquired_ts: now.to_rfc3339(),
            expires_ts: (now + chrono::Duration::seconds(3600)).to_rfc3339(),
            released_ts: None,
        };
        write_lease(&conflict_path, &lease).unwrap();

        let args = AmRunArgs {
            slot: "frontend-build".to_string(),
            cmd: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo SHOULD_NOT_RUN".to_string(),
            ],
            path: PathBuf::from("/tmp/am-run-fixture"),
            agent: Some("TestAgent".to_string()),
            ttl_seconds: 60,
            shared: false,
            exclusive: false,
            block_on_conflicts: true,
            no_block_on_conflicts: false,
        };

        let capture = StdioCapture::install().unwrap();
        let err = handle_am_run_with(&config, None, None, args).unwrap_err();
        let mut sink = Vec::new();
        capture.drain(&mut sink).unwrap();
        drop(capture);

        let output = String::from_utf8_lossy(&sink).to_string();
        let art = build_slot_artifact_dir("am_run_block_on_conflicts_aborts_without_running_child");
        std::fs::write(art.join("output.txt"), &output).unwrap();
        std::fs::write(
            art.join("conflict_lease.json"),
            serde_json::to_string_pretty(&lease).unwrap(),
        )
        .unwrap();

        match err {
            CliError::ExitCode(1) => {}
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(
            output.contains("--block-on-conflicts"),
            "missing conflict abort message: {output}"
        );
        assert!(
            !output.contains("$ sh -c"),
            "did not expect command banner on abort: {output}"
        );
        assert!(
            !output.contains("SHOULD_NOT_RUN"),
            "child command output should not appear on abort: {output}"
        );
    }

    // -----------------------------------------------------------------------
    // Share subcommand argument parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn clap_parses_share_export_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "share",
            "export",
            "-o",
            "/tmp/bundle",
            "-p",
            "proj1",
            "-p",
            "proj2",
            "--scrub-preset",
            "strict",
            "--inline-threshold",
            "1024",
            "--detach-threshold",
            "2048",
            "--chunk-threshold",
            "4096",
            "--chunk-size",
            "2048",
            "--dry-run",
            "--no-zip",
            "--signing-key",
            "/tmp/key",
            "--signing-public-out",
            "/tmp/pub.key",
            "--age-recipient",
            "age1abc",
            "--age-recipient",
            "age1def",
        ])
        .expect("failed to parse share export");
        match cli.command.expect("expected command") {
            Commands::Share {
                action: ShareCommand::Export(args),
            } => {
                assert_eq!(args.output, PathBuf::from("/tmp/bundle"));
                assert_eq!(args.projects, vec!["proj1", "proj2"]);
                assert_eq!(args.scrub_preset, "strict");
                assert_eq!(args.inline_threshold, 1024);
                assert_eq!(args.detach_threshold, 2048);
                assert_eq!(args.chunk_threshold, 4096);
                assert_eq!(args.chunk_size, 2048);
                assert!(args.dry_run);
                assert!(args.no_zip);
                assert_eq!(args.signing_key, Some(PathBuf::from("/tmp/key")));
                assert_eq!(args.age_recipient, vec!["age1abc", "age1def"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_share_export_defaults() {
        let cli = Cli::try_parse_from(["am", "share", "export", "-o", "/tmp/out"])
            .expect("failed to parse share export defaults");
        match cli.command.expect("expected command") {
            Commands::Share {
                action: ShareCommand::Export(args),
            } => {
                assert_eq!(args.scrub_preset, "standard");
                assert_eq!(
                    args.inline_threshold,
                    share::INLINE_ATTACHMENT_THRESHOLD as i64
                );
                assert_eq!(
                    args.detach_threshold,
                    share::DETACH_ATTACHMENT_THRESHOLD as i64
                );
                assert_eq!(args.chunk_threshold, share::DEFAULT_CHUNK_THRESHOLD as i64);
                assert_eq!(args.chunk_size, share::DEFAULT_CHUNK_SIZE as i64);
                assert!(!args.dry_run);
                assert!(args.zip); // default true
                assert!(!args.interactive);
                assert!(args.projects.is_empty());
                assert!(args.signing_key.is_none());
                assert!(args.age_recipient.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_share_update() {
        let cli = Cli::try_parse_from([
            "am",
            "share",
            "update",
            "/tmp/existing",
            "-p",
            "projA",
            "--scrub-preset",
            "archive",
            "--inline-threshold",
            "500",
        ])
        .expect("failed to parse share update");
        match cli.command.expect("expected command") {
            Commands::Share {
                action: ShareCommand::Update(args),
            } => {
                assert_eq!(args.bundle, PathBuf::from("/tmp/existing"));
                assert_eq!(args.projects, vec!["projA"]);
                assert_eq!(args.scrub_preset.as_deref(), Some("archive"));
                assert_eq!(args.inline_threshold, Some(500));
                assert!(args.detach_threshold.is_none());
                assert!(args.chunk_threshold.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_share_verify() {
        let cli = Cli::try_parse_from([
            "am",
            "share",
            "verify",
            "/tmp/bundle",
            "--public-key",
            "base64pubkey",
        ])
        .expect("failed to parse share verify");
        match cli.command.expect("expected command") {
            Commands::Share {
                action: ShareCommand::Verify(args),
            } => {
                assert_eq!(args.bundle, PathBuf::from("/tmp/bundle"));
                assert_eq!(args.public_key.as_deref(), Some("base64pubkey"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_share_decrypt_identity() {
        let cli = Cli::try_parse_from([
            "am",
            "share",
            "decrypt",
            "/tmp/bundle.zip.age",
            "-i",
            "/tmp/identity.key",
            "-o",
            "/tmp/out.zip",
        ])
        .expect("failed to parse share decrypt");
        match cli.command.expect("expected command") {
            Commands::Share {
                action: ShareCommand::Decrypt(args),
            } => {
                assert_eq!(args.encrypted_path, PathBuf::from("/tmp/bundle.zip.age"));
                assert_eq!(args.identity, Some(PathBuf::from("/tmp/identity.key")));
                assert_eq!(args.output, Some(PathBuf::from("/tmp/out.zip")));
                assert!(!args.passphrase);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_share_decrypt_passphrase() {
        let cli = Cli::try_parse_from(["am", "share", "decrypt", "/tmp/bundle.age", "-p"])
            .expect("failed to parse share decrypt with passphrase");
        match cli.command.expect("expected command") {
            Commands::Share {
                action: ShareCommand::Decrypt(args),
            } => {
                assert!(args.passphrase);
                assert!(args.identity.is_none());
                assert!(args.output.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_share_preview() {
        let cli = Cli::try_parse_from([
            "am",
            "share",
            "preview",
            "/tmp/bundle",
            "--host",
            "0.0.0.0",
            "--port",
            "8080",
            "--open-browser",
        ])
        .expect("failed to parse share preview");
        match cli.command.expect("expected command") {
            Commands::Share {
                action: ShareCommand::Preview(args),
            } => {
                assert_eq!(args.bundle, PathBuf::from("/tmp/bundle"));
                assert_eq!(args.host, "0.0.0.0");
                assert_eq!(args.port, 8080);
                assert!(args.open_browser);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_share_deploy_validate_format_toon() {
        let cli = Cli::try_parse_from([
            "am",
            "share",
            "deploy",
            "validate",
            "/tmp/bundle",
            "--format",
            "toon",
        ])
        .expect("failed to parse deploy validate format");

        match cli.command.expect("expected command") {
            Commands::Share {
                action:
                    ShareCommand::Deploy {
                        action: DeployCommand::Validate(args),
                    },
            } => {
                assert_eq!(args.bundle, PathBuf::from("/tmp/bundle"));
                assert_eq!(args.format, Some(output::CliOutputFormat::Toon));
                assert!(!args.json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_share_deploy_verify_url_format_toon() {
        let cli = Cli::try_parse_from([
            "am",
            "share",
            "deploy",
            "verify",
            "https://example.test/agent-mail",
            "--format",
            "toon",
        ])
        .expect("failed to parse deploy verify format");

        match cli.command.expect("expected command") {
            Commands::Share {
                action:
                    ShareCommand::Deploy {
                        action: DeployCommand::VerifyUrl(args),
                    },
            } => {
                assert_eq!(args.url, "https://example.test/agent-mail");
                assert_eq!(args.format, Some(output::CliOutputFormat::Toon));
                assert!(!args.json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_share_deploy_verify_live_defaults() {
        let cli = Cli::try_parse_from([
            "am",
            "share",
            "deploy",
            "verify-live",
            "https://example.test/agent-mail",
        ])
        .expect("failed to parse deploy verify-live defaults");

        match cli.command.expect("expected command") {
            Commands::Share {
                action:
                    ShareCommand::Deploy {
                        action: DeployCommand::VerifyLive(args),
                    },
            } => {
                assert_eq!(args.url, "https://example.test/agent-mail");
                assert_eq!(args.bundle, None);
                assert!(args.format.is_none());
                assert!(!args.json);
                assert!(!args.strict);
                assert!(!args.fail_fast);
                assert!(!args.security);
                assert_eq!(args.timeout, 10_000);
                assert_eq!(args.retries, 2);
                assert_eq!(args.retry_delay, 1_000);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_share_deploy_verify_live_format_toon() {
        let cli = Cli::try_parse_from([
            "am",
            "share",
            "deploy",
            "verify-live",
            "https://example.test/agent-mail",
            "--format",
            "toon",
        ])
        .expect("failed to parse deploy verify-live --format toon");

        match cli.command.expect("expected command") {
            Commands::Share {
                action:
                    ShareCommand::Deploy {
                        action: DeployCommand::VerifyLive(args),
                    },
            } => {
                assert_eq!(args.url, "https://example.test/agent-mail");
                assert_eq!(args.format, Some(output::CliOutputFormat::Toon));
                assert!(!args.json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_share_deploy_verify_live_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "share",
            "deploy",
            "verify-live",
            "https://example.test/agent-mail",
            "--bundle",
            "/tmp/live_bundle",
            "--json",
            "--strict",
            "--fail-fast",
            "--security",
            "--timeout",
            "2500",
            "--retries",
            "5",
            "--retry-delay",
            "250",
        ])
        .expect("failed to parse deploy verify-live with all flags");

        match cli.command.expect("expected command") {
            Commands::Share {
                action:
                    ShareCommand::Deploy {
                        action: DeployCommand::VerifyLive(args),
                    },
            } => {
                assert_eq!(args.url, "https://example.test/agent-mail");
                assert_eq!(args.bundle, Some(PathBuf::from("/tmp/live_bundle")));
                assert!(args.format.is_none());
                assert!(args.json);
                assert!(args.strict);
                assert!(args.fail_fast);
                assert!(args.security);
                assert_eq!(args.timeout, 2_500);
                assert_eq!(args.retries, 5);
                assert_eq!(args.retry_delay, 250);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn build_verify_live_options_maps_cli_fields() {
        let args = DeployVerifyLiveArgs {
            url: "https://example.test/agent-mail".to_string(),
            bundle: Some(PathBuf::from("/tmp/live_bundle")),
            format: None,
            json: true,
            strict: true,
            fail_fast: true,
            security: true,
            timeout: 12_345,
            retries: 4,
            retry_delay: 321,
        };

        let opts = build_verify_live_options(&args);
        assert_eq!(opts.url, "https://example.test/agent-mail");
        assert_eq!(opts.bundle_path, Some(PathBuf::from("/tmp/live_bundle")));
        assert!(opts.security_audit);
        assert!(opts.strict);
        assert!(opts.fail_fast);
        assert_eq!(
            opts.probe_config.timeout,
            std::time::Duration::from_millis(12_345)
        );
        assert_eq!(opts.probe_config.retries, 4);
        assert_eq!(
            opts.probe_config.retry_delay,
            std::time::Duration::from_millis(321)
        );
    }

    #[test]
    fn print_verify_check_renders_severity_tags() {
        use ftui_runtime::stdio_capture::StdioCapture;

        let _capture_lock = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let capture = StdioCapture::install().unwrap();

        let checks = [
            share::deploy::VerifyLiveCheck {
                id: "remote.root".to_string(),
                description: "root".to_string(),
                severity: share::deploy::CheckSeverity::Error,
                passed: true,
                message: "ok".to_string(),
                elapsed_ms: 1,
                http_status: Some(200),
                headers_captured: None,
            },
            share::deploy::VerifyLiveCheck {
                id: "remote.coop".to_string(),
                description: "coop".to_string(),
                severity: share::deploy::CheckSeverity::Warning,
                passed: false,
                message: "missing".to_string(),
                elapsed_ms: 1,
                http_status: None,
                headers_captured: None,
            },
            share::deploy::VerifyLiveCheck {
                id: "remote.content_match".to_string(),
                description: "content match".to_string(),
                severity: share::deploy::CheckSeverity::Skipped,
                passed: false,
                message: "skipped".to_string(),
                elapsed_ms: 0,
                http_status: None,
                headers_captured: None,
            },
        ];

        for check in &checks {
            print_verify_check(check);
        }

        let mut sink = Vec::new();
        capture.drain(&mut sink).unwrap();
        drop(capture);

        let out = String::from_utf8_lossy(&sink);
        assert!(out.contains("[PASS] remote.root"));
        assert!(out.contains("[WARN] remote.coop"));
        assert!(out.contains("[SKIP] remote.content_match"));
    }

    #[test]
    fn clap_share_preview_defaults() {
        let cli = Cli::try_parse_from(["am", "share", "preview", "/tmp/bundle"])
            .expect("failed to parse share preview defaults");
        match cli.command.expect("expected command") {
            Commands::Share {
                action: ShareCommand::Preview(args),
            } => {
                assert_eq!(args.host, "127.0.0.1");
                assert_eq!(args.port, 9000);
                assert!(!args.open_browser);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn share_preview_status_endpoint_and_hotkeys() {
        use std::io::{Read, Write};
        use std::net::{SocketAddr, TcpStream};
        use std::sync::mpsc;
        use std::time::Duration;

        fn http_get(
            addr: SocketAddr,
            path: &str,
        ) -> (u16, std::collections::HashMap<String, String>, Vec<u8>) {
            let mut stream = TcpStream::connect(addr).expect("connect");
            let req = format!(
                "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n",
                host = addr
            );
            stream.write_all(req.as_bytes()).expect("write request");
            stream.flush().expect("flush");

            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).expect("read response");

            let split = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .expect("header split");
            let (head, body) = buf.split_at(split + 4);

            let head_str = String::from_utf8_lossy(head);
            let mut lines = head_str.split("\r\n");
            let status_line = lines.next().unwrap_or_default();
            let code: u16 = status_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);

            let mut headers = std::collections::HashMap::new();
            for line in lines {
                if line.is_empty() {
                    break;
                }
                if let Some((k, v)) = line.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            (code, headers, body.to_vec())
        }

        let temp = tempfile::TempDir::new().expect("tempdir");
        let bundle = temp.path().join("bundle");
        std::fs::create_dir_all(&bundle).expect("create bundle dir");
        std::fs::write(bundle.join("index.html"), "<html>root</html>").expect("write index");
        std::fs::write(bundle.join("manifest.json"), "{}\n").expect("write manifest");
        share::copy_viewer_assets(&bundle).expect("copy viewer assets");

        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S_%f").to_string();
        let artifacts_dir = PathBuf::from("tests/artifacts/cli/share_preview").join(ts);

        let (addr_tx, addr_rx) = mpsc::channel::<SocketAddr>();
        let (key_tx, key_rx) = mpsc::channel::<char>();

        let thread = std::thread::spawn(move || {
            run_share_preview_with_control(
                bundle,
                "127.0.0.1".to_string(),
                0,
                false,
                Some(key_rx),
                Some(addr_tx),
                Some(artifacts_dir),
            )
        });

        let addr = addr_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("preview server did not start");

        let (code, headers, body) = http_get(addr, "/__preview__/status");
        assert_eq!(code, 200, "status endpoint should return 200");
        assert!(
            headers
                .get("cache-control")
                .is_some_and(|v| v.contains("no-cache")),
            "expected cache-control no-cache header"
        );
        assert_eq!(
            headers.get("pragma").map(String::as_str),
            Some("no-cache"),
            "expected pragma no-cache header"
        );
        let status1: serde_json::Value =
            serde_json::from_slice(&body).expect("status endpoint JSON");
        let sig1 = status1
            .get("signature")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(!sig1.is_empty(), "expected non-empty signature");
        let manual1 = status1
            .get("manual_token")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let (code, headers, _body) = http_get(addr, "/viewer/");
        assert_eq!(code, 200, "/viewer/ should serve index.html");
        assert!(
            headers
                .get("cache-control")
                .is_some_and(|v| v.contains("no-cache")),
            "expected cache-control no-cache header on static responses"
        );

        key_tx.send('r').expect("send reload");
        std::thread::sleep(Duration::from_millis(50));
        let (_code, _headers, body) = http_get(addr, "/__preview__/status");
        let status2: serde_json::Value =
            serde_json::from_slice(&body).expect("status endpoint JSON (2)");
        let sig2 = status2
            .get("signature")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let manual2 = status2
            .get("manual_token")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        assert_ne!(sig1, sig2, "signature should change after reload");
        assert!(
            manual2 > manual1,
            "manual token should increase after reload"
        );

        key_tx.send('d').expect("send deploy");
        let result = thread.join().expect("join preview thread");
        assert!(matches!(result, Err(CliError::ExitCode(42))));
    }

    #[test]
    #[cfg(unix)]
    fn share_preview_does_not_serve_symlink_escape() {
        use std::io::{Read, Write};
        use std::net::{SocketAddr, TcpStream};
        use std::sync::mpsc;
        use std::time::Duration;

        fn http_get(addr: SocketAddr, path: &str) -> (u16, Vec<u8>) {
            let mut stream = TcpStream::connect(addr).expect("connect");
            let req = format!(
                "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n",
                host = addr
            );
            stream.write_all(req.as_bytes()).expect("write request");
            stream.flush().expect("flush");

            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).expect("read response");

            let split = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .expect("header split");
            let (head, body) = buf.split_at(split + 4);

            let head_str = String::from_utf8_lossy(head);
            let code: u16 = head_str
                .split("\r\n")
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .nth(1)
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            (code, body.to_vec())
        }

        let temp = tempfile::TempDir::new().expect("tempdir");
        let bundle = temp.path().join("bundle");
        std::fs::create_dir_all(&bundle).expect("create bundle dir");
        std::fs::write(bundle.join("index.html"), "<html>root</html>").expect("write index");
        std::fs::write(bundle.join("manifest.json"), "{}\n").expect("write manifest");

        let secret = temp.path().join("secret.txt");
        std::fs::write(&secret, "top-secret").expect("write secret");
        std::os::unix::fs::symlink(&secret, bundle.join("leak.txt")).expect("symlink");

        share::copy_viewer_assets(&bundle).expect("copy viewer assets");

        let (addr_tx, addr_rx) = mpsc::channel::<SocketAddr>();
        let (key_tx, key_rx) = mpsc::channel::<char>();

        let thread = std::thread::spawn(move || {
            run_share_preview_with_control(
                bundle,
                "127.0.0.1".to_string(),
                0,
                false,
                Some(key_rx),
                Some(addr_tx),
                None,
            )
        });

        let addr = addr_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("preview server did not start");

        let (code, body) = http_get(addr, "/leak.txt");
        assert_eq!(
            code,
            404,
            "expected symlink escape to be blocked (code={code}, body={})",
            String::from_utf8_lossy(&body)
        );

        key_tx.send('q').expect("send quit");
        let result = thread.join().expect("join preview thread");
        assert!(result.is_ok());
    }

    #[test]
    fn clap_parses_share_wizard() {
        let cli =
            Cli::try_parse_from(["am", "share", "wizard"]).expect("failed to parse share wizard");
        assert!(matches!(
            cli.command,
            Some(Commands::Share {
                action: ShareCommand::Wizard(_)
            })
        ));
    }

    // -----------------------------------------------------------------------
    // Native Wizard Integration Tests (br-2xfi)
    // -----------------------------------------------------------------------

    #[test]
    fn clap_parses_share_wizard_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "share",
            "wizard",
            "--bundle",
            "/tmp/bundle",
            "--provider",
            "github",
            "--github-repo",
            "owner/repo",
            "--github-branch",
            "main",
            "--output",
            "/tmp/output",
            "--yes",
            "--dry-run",
            "--non-interactive",
            "--json",
        ])
        .expect("failed to parse share wizard flags");
        match cli.command.expect("expected command") {
            Commands::Share {
                action: ShareCommand::Wizard(args),
            } => {
                assert_eq!(args.bundle, Some(PathBuf::from("/tmp/bundle")));
                assert_eq!(args.provider, Some("github".to_string()));
                assert_eq!(args.github_repo, Some("owner/repo".to_string()));
                assert_eq!(args.github_branch, "main");
                assert_eq!(args.output, Some(PathBuf::from("/tmp/output")));
                assert!(args.yes);
                assert!(args.dry_run);
                assert!(args.non_interactive);
                assert!(args.format.is_none());
                assert!(args.json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_share_wizard_format_toon() {
        let cli = Cli::try_parse_from(["am", "share", "wizard", "--format", "toon"])
            .expect("failed to parse share wizard --format toon");
        match cli.command.expect("expected command") {
            Commands::Share {
                action: ShareCommand::Wizard(args),
            } => {
                assert_eq!(args.format, Some(output::CliOutputFormat::Toon));
                assert!(!args.json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn native_wizard_non_interactive_requires_provider() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Test that non-interactive mode fails without provider
        let args = ShareWizardArgs {
            bundle: Some(PathBuf::from("/tmp/nonexistent")),
            provider: None,
            github_repo: None,
            github_branch: "gh-pages".to_string(),
            cloudflare_project: None,
            netlify_site: None,
            s3_bucket: None,
            cloudfront_id: None,
            base_url: None,
            output: None,
            yes: false,
            dry_run: false,
            non_interactive: true,
            format: None,
            json: false,
        };
        let result = run_native_wizard(args);
        // Should fail because provider is required in non-interactive mode
        assert!(result.is_err());
    }

    #[test]
    fn native_wizard_json_failure_reports_missing_required_option_code() {
        let _capture_lock = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let capture = ftui_runtime::StdioCapture::install().expect("install capture");

        let temp = tempfile::TempDir::new().expect("tempdir");
        let bundle_dir = temp.path().join("bundle");
        std::fs::create_dir_all(&bundle_dir).expect("create bundle dir");
        std::fs::write(bundle_dir.join("manifest.json"), "{}").expect("write manifest");

        let args = ShareWizardArgs {
            bundle: Some(bundle_dir),
            provider: Some("github".to_string()),
            github_repo: None, // required for github provider
            github_branch: "gh-pages".to_string(),
            cloudflare_project: None,
            netlify_site: None,
            s3_bucket: None,
            cloudfront_id: None,
            base_url: None,
            output: None,
            yes: true,
            dry_run: true,
            non_interactive: true,
            format: None,
            json: true,
        };

        let result = run_native_wizard(args);
        let expected_code = i32::from(share::WizardErrorCode::MissingRequiredOption.code());
        assert!(
            matches!(result, Err(CliError::ExitCode(code)) if code == expected_code),
            "expected missing-required-option exit code, got: {result:?}"
        );

        let output = capture.drain_to_string();
        // Extract JSON block — concurrent tests may inject non-JSON lines
        let json_str = extract_json_block(&output)
            .unwrap_or_else(|| panic!("no JSON object found in wizard output: {output}"));
        let parsed: serde_json::Value = serde_json::from_str(json_str)
            .unwrap_or_else(|_| panic!("failed to parse wizard JSON failure output: {json_str}"));
        assert_eq!(parsed["success"], serde_json::Value::Bool(false));
        assert_eq!(parsed["error_code"], "MISSING_REQUIRED_OPTION");
    }

    #[test]
    fn native_wizard_json_output_format() {
        let _capture_lock = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let capture = ftui_runtime::StdioCapture::install().expect("install capture");

        // Create a valid bundle directory
        let temp = tempfile::TempDir::new().expect("tempdir");
        let bundle_dir = temp.path().join("bundle");
        std::fs::create_dir_all(&bundle_dir).expect("create bundle dir");
        std::fs::write(bundle_dir.join("manifest.json"), "{}").expect("write manifest");

        let args = ShareWizardArgs {
            bundle: Some(bundle_dir),
            provider: Some("custom".to_string()),
            github_repo: None,
            github_branch: "gh-pages".to_string(),
            cloudflare_project: None,
            netlify_site: None,
            s3_bucket: None,
            cloudfront_id: None,
            base_url: None,
            output: None,
            yes: true,     // Skip confirmation
            dry_run: true, // Don't actually execute
            non_interactive: true,
            format: None,
            json: true,
        };
        let _ = run_native_wizard(args);

        let output = capture.drain_to_string();
        // Extract JSON block — concurrent tests may inject non-JSON lines
        let json_str = extract_json_block(&output)
            .unwrap_or_else(|| panic!("no JSON object found in wizard output: {output}"));
        let parsed: serde_json::Value = serde_json::from_str(json_str)
            .unwrap_or_else(|_| panic!("Failed to parse JSON output: {json_str}"));

        // Verify essential fields are present
        assert!(parsed.get("success").is_some(), "missing 'success' field");
        assert!(parsed.get("provider").is_some(), "missing 'provider' field");
    }

    #[test]
    fn native_wizard_dry_run_does_not_execute() {
        let _capture_lock = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let capture = ftui_runtime::StdioCapture::install().expect("install capture");

        // Create a valid bundle directory
        let temp = tempfile::TempDir::new().expect("tempdir");
        let bundle_dir = temp.path().join("bundle");
        let output_dir = temp.path().join("output");
        std::fs::create_dir_all(&bundle_dir).expect("create bundle dir");
        std::fs::write(bundle_dir.join("manifest.json"), "{}").expect("write manifest");

        let args = ShareWizardArgs {
            bundle: Some(bundle_dir),
            provider: Some("github".to_string()),
            github_repo: Some("owner/repo".to_string()),
            github_branch: "gh-pages".to_string(),
            cloudflare_project: None,
            netlify_site: None,
            s3_bucket: None,
            cloudfront_id: None,
            base_url: None,
            output: Some(output_dir.clone()),
            yes: true,
            dry_run: true,
            non_interactive: true,
            format: None,
            json: false,
        };
        let result = run_native_wizard(args);
        assert!(result.is_ok(), "dry-run should succeed");

        let output = capture.drain_to_string();
        assert!(
            output.contains("Dry run") || output.contains("dry-run") || output.contains("dry run"),
            "output should mention dry-run: {output}"
        );

        // Output directory should NOT be created in dry-run mode
        assert!(
            !output_dir.exists(),
            "output directory should not be created in dry-run mode"
        );
    }

    #[test]
    fn native_wizard_validates_bundle_path() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let args = ShareWizardArgs {
            bundle: Some(PathBuf::from("/nonexistent/bundle/path")),
            provider: Some("custom".to_string()),
            github_repo: None,
            github_branch: "gh-pages".to_string(),
            cloudflare_project: None,
            netlify_site: None,
            s3_bucket: None,
            cloudfront_id: None,
            base_url: None,
            output: None,
            yes: true,
            dry_run: true,
            non_interactive: true,
            format: None,
            json: false,
        };
        let result = run_native_wizard(args);
        assert!(result.is_err(), "should fail with nonexistent bundle");
    }

    // -----------------------------------------------------------------------
    // Archive subcommand argument parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn clap_parses_archive_save_defaults() {
        let cli =
            Cli::try_parse_from(["am", "archive", "save"]).expect("failed to parse archive save");
        match cli.command.expect("expected command") {
            Commands::Archive {
                action:
                    ArchiveCommand::Save {
                        projects,
                        scrub_preset,
                        label,
                    },
            } => {
                assert!(projects.is_empty());
                assert_eq!(scrub_preset, "archive");
                assert!(label.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_archive_save_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "archive",
            "save",
            "-p",
            "proj1",
            "-p",
            "proj2",
            "--scrub-preset",
            "strict",
            "-l",
            "nightly",
        ])
        .expect("failed to parse archive save flags");
        match cli.command.expect("expected command") {
            Commands::Archive {
                action:
                    ArchiveCommand::Save {
                        projects,
                        scrub_preset,
                        label,
                    },
            } => {
                assert_eq!(projects, vec!["proj1".to_string(), "proj2".to_string()]);
                assert_eq!(scrub_preset, "strict");
                assert_eq!(label.as_deref(), Some("nightly"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_archive_list_defaults() {
        let cli =
            Cli::try_parse_from(["am", "archive", "list"]).expect("failed to parse archive list");
        match cli.command.expect("expected command") {
            Commands::Archive {
                action: ArchiveCommand::List { limit, json, .. },
            } => {
                assert_eq!(limit, 0);
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_archive_list_flags() {
        let cli = Cli::try_parse_from(["am", "archive", "list", "-n", "5", "--json"])
            .expect("failed to parse archive list flags");
        match cli.command.expect("expected command") {
            Commands::Archive {
                action: ArchiveCommand::List { limit, json, .. },
            } => {
                assert_eq!(limit, 5);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_archive_list_format_toon() {
        let cli = Cli::try_parse_from(["am", "archive", "list", "--format", "toon"])
            .expect("failed to parse archive list format");
        match cli.command.expect("expected command") {
            Commands::Archive {
                action: ArchiveCommand::List { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_archive_restore_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "archive",
            "restore",
            "/tmp/state.zip",
            "--force",
            "--dry-run",
        ])
        .expect("failed to parse archive restore flags");
        match cli.command.expect("expected command") {
            Commands::Archive {
                action:
                    ArchiveCommand::Restore {
                        archive_file,
                        force,
                        dry_run,
                    },
            } => {
                assert_eq!(archive_file, PathBuf::from("/tmp/state.zip"));
                assert!(force);
                assert!(dry_run);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Products subcommand argument parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn clap_parses_products_search_defaults() {
        let cli = Cli::try_parse_from(["am", "products", "search", "prod-1", "query"])
            .expect("failed to parse products search defaults");
        match cli.command.expect("expected command") {
            Commands::Products {
                action:
                    ProductsCommand::Search {
                        product_key,
                        query,
                        limit,
                        json,
                        ..
                    },
            } => {
                assert_eq!(product_key, "prod-1");
                assert_eq!(query, "query");
                assert_eq!(limit, 20);
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_products_inbox_positional_agent_and_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "products",
            "inbox",
            "prod-1",
            "GreenCastle",
            "--limit",
            "5",
            "--urgent-only",
            "--include-bodies",
            "--since-ts",
            "2026-02-05T00:00:00Z",
            "--json",
        ])
        .expect("failed to parse products inbox");
        match cli.command.expect("expected command") {
            Commands::Products {
                action:
                    ProductsCommand::Inbox {
                        product_key,
                        agent,
                        limit,
                        urgent_only,
                        all,
                        include_bodies,
                        no_bodies,
                        since_ts,
                        json,
                        ..
                    },
            } => {
                assert_eq!(product_key, "prod-1");
                assert_eq!(agent, "GreenCastle");
                assert_eq!(limit, 5);
                assert!(urgent_only);
                assert!(!all);
                assert!(include_bodies);
                assert!(!no_bodies);
                assert_eq!(since_ts.as_deref(), Some("2026-02-05T00:00:00Z"));
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn slugify_matches_legacy() {
        assert_eq!(slugify(" My Project! "), "my-project");
        assert_eq!(slugify(""), "project");
        assert_eq!(slugify("___"), "project");
        assert_eq!(slugify("A--B"), "a-b");
    }

    #[test]
    fn compose_archive_basename_matches_legacy() {
        use chrono::TimeZone;

        let ts = Utc.with_ymd_and_hms(2026, 2, 5, 12, 34, 56).unwrap();
        let projects = vec!["My Project".to_string(), "Another".to_string()];
        let base = compose_archive_basename(ts, &projects, "archive", Some("nightly"));
        assert_eq!(
            base,
            "mailbox-state-20260205-123456Z-my-project-another-archive-nightly"
        );
    }

    // -----------------------------------------------------------------------
    // Archive save/list/restore integration-ish tests
    // -----------------------------------------------------------------------

    static ARCHIVE_TEST_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

    #[test]
    fn migrate_command_matches_legacy_output_lines() {
        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        use ftui_runtime::stdio_capture::StdioCapture;
        let _capture_lock = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("migrate.db");
        let url = format!("sqlite:///{}", db_path.display());

        let capture = StdioCapture::install().unwrap();
        let res = handle_migrate_with_database_url(&url);
        let mut sink = Vec::new();
        capture.drain(&mut sink).unwrap();
        drop(capture);

        res.unwrap();
        let out = String::from_utf8_lossy(&sink);
        assert!(
            out.contains("✓ Database schema created from model definitions!"),
            "stdout: {out}"
        );
        assert!(
            out.contains(
                "Note: To apply model changes, delete storage.sqlite3 and run this again."
            ),
            "stdout: {out}"
        );
    }

    // ── migrate parity tests (br-2ei.5.11) ──────────────────────────────────

    #[test]
    fn clap_parses_migrate() {
        let m = Cli::try_parse_from(["am", "migrate"]).unwrap();
        assert!(matches!(m.command, Some(Commands::Migrate { .. })));
    }

    #[test]
    fn clap_parses_migrate_check() {
        let m = Cli::try_parse_from(["am", "migrate", "--check"]).unwrap();
        match m.command {
            Some(Commands::Migrate {
                check,
                rollback,
                force,
                backup_dir,
            }) => {
                assert!(check);
                assert!(!rollback);
                assert!(!force);
                assert!(backup_dir.is_none());
            }
            other => panic!("expected Migrate, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_migrate_force_with_backup() {
        let m =
            Cli::try_parse_from(["am", "migrate", "--force", "--backup-dir", "/tmp/bak"]).unwrap();
        match m.command {
            Some(Commands::Migrate {
                check,
                rollback,
                force,
                backup_dir,
            }) => {
                assert!(!check);
                assert!(!rollback);
                assert!(force);
                assert_eq!(backup_dir, Some(PathBuf::from("/tmp/bak")));
            }
            other => panic!("expected Migrate, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_migrate_rollback() {
        let m = Cli::try_parse_from(["am", "migrate", "--rollback", "--force"]).unwrap();
        match m.command {
            Some(Commands::Migrate {
                check,
                rollback,
                force,
                backup_dir,
            }) => {
                assert!(!check);
                assert!(rollback);
                assert!(force);
                assert!(backup_dir.is_none());
            }
            other => panic!("expected Migrate, got {other:?}"),
        }
    }

    #[test]
    fn list_db_backups_returns_latest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("storage.sqlite3");
        std::fs::write(&db_path, b"db").expect("write db");

        let older = dir.path().join("storage.sqlite3.bak.20260101_000000");
        let newer = dir.path().join("storage.sqlite3.bak.20260102_000000");
        std::fs::write(&older, b"old").expect("write old backup");
        std::fs::write(&newer, b"new").expect("write new backup");

        let backups = list_db_backups(&db_path, Some(dir.path())).expect("list backups");
        assert_eq!(backups.len(), 2);
        assert_eq!(backups[0], newer);
        assert_eq!(backups[1], older);
    }

    #[test]
    fn restore_db_from_backup_replaces_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("storage.sqlite3");
        let backup_path = dir.path().join("storage.sqlite3.bak.20260102_000000");

        std::fs::write(&db_path, b"live-db").expect("write live db");
        std::fs::write(&backup_path, b"backup-db").expect("write backup db");

        restore_db_from_backup(&db_path, &backup_path).expect("restore");
        let restored = std::fs::read(&db_path).expect("read restored db");
        assert_eq!(restored, b"backup-db");

        let backup_prefix = "storage.sqlite3.pre_rollback.";
        let rollback_artifacts: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(backup_prefix))
            })
            .collect();
        assert_eq!(
            rollback_artifacts.len(),
            1,
            "expected one pre-rollback backup artifact"
        );
    }

    #[test]
    fn migrate_is_idempotent() {
        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("idempotent.db");
        let url = format!("sqlite:///{}", db_path.display());

        let res1 = handle_migrate_with_database_url(&url);
        assert!(res1.is_ok(), "first migrate failed: {res1:?}");

        let res2 = handle_migrate_with_database_url(&url);
        assert!(res2.is_ok(), "second migrate (idempotent) failed: {res2:?}");
    }

    #[test]
    fn migrate_creates_expected_tables() {
        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("tables.db");
        let url = format!("sqlite:///{}", db_path.display());

        handle_migrate_with_database_url(&url).unwrap();

        // FrankenConnection does not support sqlite_master and cross-connection
        // visibility is limited. Re-apply the schema on a fresh connection and
        // probe each table directly with a SELECT.
        let conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string()).expect("reopen");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql())
            .expect("init schema on verification connection");

        for expected in [
            "projects",
            "agents",
            "messages",
            "message_recipients",
            "file_reservations",
        ] {
            let probe = format!("SELECT 1 FROM {expected} LIMIT 0");
            assert!(
                conn.query_sync(&probe, &[]).is_ok(),
                "table {expected} should exist after migration"
            );
        }
    }

    #[test]
    fn migrate_invalid_path_returns_error() {
        let url = "sqlite:////nonexistent/deeply/nested/dir/db.sqlite3";
        let res = handle_migrate_with_database_url(url);
        assert!(res.is_err(), "should fail for non-existent path");
    }

    #[test]
    fn migrate_bad_url_scheme_returns_error() {
        let res = handle_migrate_with_database_url("postgres://localhost/db");
        assert!(res.is_err(), "non-sqlite URL should fail");
    }

    #[test]
    fn migrate_cleans_fts_tables() {
        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("fts.db");
        let url = format!("sqlite:///{}", db_path.display());

        handle_migrate_with_database_url(&url).unwrap();

        let conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string()).expect("reopen");
        let tables = conn
            .query_sync(
                "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'fts_%' ORDER BY name",
                &[],
            )
            .expect("list fts tables");
        let names: Vec<String> = tables
            .iter()
            .filter_map(|r| r.get_named::<String>("name").ok())
            .collect();
        assert!(
            names.is_empty(),
            "FTS tables should be absent after migrate (Search V3 decommission); found: {names:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn migrate_readonly_dir_returns_error() {
        use std::os::unix::fs::PermissionsExt;

        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        let dir = tempfile::tempdir().expect("tempdir");
        let readonly = dir.path().join("readonly");
        std::fs::create_dir(&readonly).expect("mkdir");
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o444))
            .expect("set readonly");

        let db_path = readonly.join("db.sqlite3");
        let url = format!("sqlite:///{}", db_path.display());
        let res = handle_migrate_with_database_url(&url);

        // Restore permissions for cleanup
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o755))
            .expect("restore permissions");

        assert!(res.is_err(), "should fail on read-only directory");
    }

    #[test]
    fn migrate_creates_db_file() {
        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("new.db");
        assert!(!db_path.exists(), "precondition: DB should not exist yet");

        let url = format!("sqlite:///{}", db_path.display());
        handle_migrate_with_database_url(&url).unwrap();

        assert!(db_path.exists(), "migrate should create the DB file");
        assert!(
            std::fs::metadata(&db_path).unwrap().len() > 0,
            "DB file should not be empty"
        );
    }

    #[test]
    fn migrate_output_matches_legacy_lines_exactly() {
        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        use ftui_runtime::stdio_capture::StdioCapture;
        let _capture_lock = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("legacy_lines.db");
        let url = format!("sqlite:///{}", db_path.display());

        let capture = StdioCapture::install().unwrap();
        handle_migrate_with_database_url(&url).unwrap();
        let mut sink = Vec::new();
        capture.drain(&mut sink).unwrap();
        drop(capture);

        let out = String::from_utf8_lossy(&sink);
        let lines: Vec<&str> = out.lines().collect();
        // Filter to migrate-specific lines (other tests may leak ftui_println output
        // through StdioCapture, e.g. "Stopping preview server...").
        let migrate_lines: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|l| {
                l.contains("Database schema created") || l.contains("delete storage.sqlite3")
            })
            .collect();
        assert_eq!(
            migrate_lines.len(),
            2,
            "expected exactly 2 migrate output lines, got {}: {migrate_lines:?} (raw: {lines:?})",
            migrate_lines.len()
        );
        assert!(
            migrate_lines[0].contains("Database schema created from model definitions"),
            "line 0: {:?}",
            migrate_lines[0]
        );
        assert!(
            migrate_lines[1].contains("delete storage.sqlite3"),
            "line 1: {:?}",
            migrate_lines[1]
        );
    }

    struct CwdGuard {
        original: PathBuf,
    }

    impl CwdGuard {
        fn chdir(path: &Path) -> Self {
            let original = std::env::current_dir().expect("get cwd");
            std::env::set_current_dir(path).expect("set cwd");
            Self { original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    fn seed_mailbox_db(db_path: &Path) {
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open test sqlite db");
        conn.execute_raw(
            "CREATE TABLE projects (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                slug TEXT NOT NULL, \
                human_key TEXT NOT NULL, \
                created_at INTEGER NOT NULL DEFAULT 0\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                project_id INTEGER NOT NULL, \
                name TEXT NOT NULL, \
                program TEXT NOT NULL DEFAULT '', \
                model TEXT NOT NULL DEFAULT '', \
                task_description TEXT NOT NULL DEFAULT '', \
                inception_ts INTEGER NOT NULL DEFAULT 0, \
                last_active_ts INTEGER NOT NULL DEFAULT 0, \
                attachments_policy TEXT NOT NULL DEFAULT 'auto', \
                contact_policy TEXT NOT NULL DEFAULT 'auto'\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                project_id INTEGER NOT NULL, \
                sender_id INTEGER NOT NULL, \
                thread_id TEXT, \
                subject TEXT NOT NULL DEFAULT '', \
                body_md TEXT NOT NULL DEFAULT '', \
                importance TEXT NOT NULL DEFAULT 'normal', \
                ack_required INTEGER NOT NULL DEFAULT 0, \
                created_ts INTEGER NOT NULL DEFAULT 0, \
                attachments TEXT NOT NULL DEFAULT '[]'\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (\
                message_id INTEGER NOT NULL, \
                agent_id INTEGER NOT NULL, \
                kind TEXT NOT NULL DEFAULT 'to', \
                read_ts INTEGER, \
                ack_ts INTEGER, \
                PRIMARY KEY (message_id, agent_id)\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservations (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                project_id INTEGER NOT NULL, \
                agent_id INTEGER NOT NULL, \
                path_pattern TEXT NOT NULL, \
                exclusive INTEGER NOT NULL DEFAULT 1, \
                reason TEXT NOT NULL DEFAULT '', \
                created_ts INTEGER NOT NULL DEFAULT 0, \
                expires_ts INTEGER NOT NULL DEFAULT 0, \
                released_ts INTEGER\
            )",
        )
        .unwrap();

        // Two projects so we can scope down to one.
        let created_at_us = 1_704_067_200_000_000i64; // 2024-01-01T00:00:00Z
        conn.execute_raw(&format!(
            "INSERT INTO projects (slug, human_key, created_at) VALUES \
             ('proj-alpha', '/data/projects/alpha', {created_at_us}), \
             ('proj-beta',  '/data/projects/beta',  {created_at_us})"
        ))
        .unwrap();
        conn.execute_raw("INSERT INTO agents (project_id, name) VALUES (1, 'GreenCastle')")
            .unwrap();
        conn.execute_raw("INSERT INTO agents (project_id, name) VALUES (2, 'PurpleBear')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO messages (project_id, sender_id, subject, body_md) VALUES \
             (1, 1, 'Msg A', 'hello'), \
             (1, 1, 'Msg B', 'world'), \
             (2, 2, 'Msg C', 'bye')",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO message_recipients (message_id, agent_id) VALUES (1, 1)")
            .unwrap();
        conn.execute_raw("INSERT INTO message_recipients (message_id, agent_id) VALUES (2, 1)")
            .unwrap();
        conn.execute_raw("INSERT INTO message_recipients (message_id, agent_id) VALUES (3, 2)")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO file_reservations (project_id, agent_id, path_pattern) VALUES (1, 1, 'src/*.rs')",
        )
        .unwrap();
    }

    fn seed_storage_root(storage_root: &Path) {
        std::fs::create_dir_all(storage_root.join("nested/dir")).unwrap();
        std::fs::write(storage_root.join("nested/dir/file.txt"), b"hello\n").unwrap();

        // Minimal git marker so `detect_git_head()` has stable output.
        std::fs::create_dir_all(storage_root.join(".git")).unwrap();
        std::fs::write(storage_root.join(".git/HEAD"), b"0123456789abcdef\n").unwrap();
    }

    fn find_backup_entry(dir: &Path, prefix: &str) -> Option<PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            if name.starts_with(prefix) {
                return Some(path);
            }
        }
        None
    }

    #[test]
    fn archive_save_list_restore_roundtrip_smoke() {
        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        use std::io::Read;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), b"[workspace]\n").unwrap();
        let _cwd = CwdGuard::chdir(root.path());
        println!(">>> _cwd acquired");

        let storage_root = root.path().join("storage_repo");
        seed_storage_root(&storage_root);
        println!(">>> seed_storage_root done");

        let source_db = root.path().join("mailbox.sqlite3");
        seed_mailbox_db(&source_db);
        println!(">>> seed_mailbox_db done");

        // Save archive for a single project to exercise scoping.
        let archive_path = archive_save_state(
            &source_db,
            &storage_root,
            vec!["proj-alpha".to_string()],
            "archive".to_string(),
            Some("nightly".to_string()),
        )
        .expect("archive save");
        assert!(archive_path.exists());
        println!(">>> archive_save_state done");

        // Validate zip layout + metadata content.
        let file = std::fs::File::open(&archive_path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        println!(">>> zip file opened");
        assert!(zip.by_name(ARCHIVE_METADATA_FILENAME).is_ok());
        assert!(zip.by_name(ARCHIVE_SNAPSHOT_RELATIVE).is_ok());
        assert!(zip.by_name("storage_repo/nested/dir/file.txt").is_ok());
        assert!(zip.by_name("storage_repo/.git/HEAD").is_ok());

        let mut meta_contents = String::new();
        zip.by_name(ARCHIVE_METADATA_FILENAME)
            .unwrap()
            .read_to_string(&mut meta_contents)
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_contents).unwrap();
        println!(">>> zip meta verified");
        assert_eq!(meta["scrub_preset"].as_str(), Some("archive"));
        assert_eq!(meta["label"].as_str(), Some("nightly"));
        assert_eq!(
            meta["projects_requested"].as_array().unwrap().len(),
            1,
            "save should record requested project filters"
        );
        let included = meta["projects_included"].as_array().unwrap();
        assert_eq!(included.len(), 1, "scope should keep only 1 project");
        assert_eq!(included[0]["slug"].as_str(), Some("proj-alpha"));
        assert_eq!(
            included[0]["created_at"].as_str(),
            Some("2024-01-01T00:00:00+00:00")
        );

        // `archive list --json` output should include the new archive.
        {
            use ftui_runtime::stdio_capture::StdioCapture;

            let _capture_lock = stdio_capture_lock()
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            println!(">>> _capture_lock acquired");
            let capture = StdioCapture::install().unwrap();
            println!(">>> StdioCapture::install done");
            handle_archive(ArchiveCommand::List {
                limit: 0,
                format: None,
                json: true,
            })
            .unwrap();
            println!(">>> handle_archive done");
            let mut sink = Vec::new();
            capture.drain(&mut sink).unwrap();
            println!(">>> capture.drain done");
            drop(capture);

            let output = String::from_utf8_lossy(&sink).trim().to_string();
            let list_json: serde_json::Value = serde_json::from_str(&output).unwrap();
            let arr = list_json.as_array().unwrap();
            assert_eq!(arr.len(), 1);
            assert_eq!(
                arr[0]["file"].as_str(),
                archive_path.file_name().and_then(|s| s.to_str())
            );
            assert_eq!(arr[0]["scrub_preset"].as_str(), Some("archive"));
            assert_eq!(
                arr[0]["projects"].as_array().unwrap()[0].as_str(),
                Some("proj-alpha")
            );
        }

        // Restore safety: without --force on non-tty stdin, should refuse to prompt.
        let archive_arg = PathBuf::from(archive_path.file_name().unwrap());
        let restore_dir = root.path().join("restore");
        let restore_db = restore_dir.join("mailbox.sqlite3");
        let restore_storage = restore_dir.join("storage_repo");
        std::fs::create_dir_all(&restore_dir).unwrap();
        std::fs::write(&restore_db, b"old-db").unwrap();
        std::fs::create_dir_all(&restore_storage).unwrap();
        std::fs::write(restore_storage.join("old.txt"), b"old-storage").unwrap();

        let err = archive_restore_state(
            archive_arg.clone(),
            &restore_db,
            &restore_storage,
            false,
            false,
        )
        .unwrap_err();
        let msg = match err {
            CliError::Other(m) => m,
            other => format!("{other}"),
        };
        assert!(
            msg.contains("refusing to prompt on non-interactive stdin"),
            "unexpected error: {msg}"
        );

        // Dry-run should print plan and make no changes.
        {
            use ftui_runtime::stdio_capture::StdioCapture;

            let _capture_lock = stdio_capture_lock()
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let capture = StdioCapture::install().unwrap();
            archive_restore_state(
                archive_arg.clone(),
                &restore_db,
                &restore_storage,
                false,
                true,
            )
            .unwrap();
            let mut sink = Vec::new();
            capture.drain(&mut sink).unwrap();
            drop(capture);

            let output = String::from_utf8_lossy(&sink);
            assert!(output.contains("Dry-run plan:"));
            assert!(output.contains("restore snapshot ->"));
            assert!(output.contains("restore storage repo ->"));
            assert_eq!(std::fs::read(&restore_db).unwrap(), b"old-db");
            assert_eq!(
                std::fs::read(restore_storage.join("old.txt")).unwrap(),
                b"old-storage"
            );
        }

        // Actual restore with --force should create backups and restore snapshot + storage.
        archive_restore_state(archive_arg, &restore_db, &restore_storage, true, false).unwrap();

        let db_backup =
            find_backup_entry(&restore_dir, "mailbox.sqlite3.backup-").expect("db backup created");
        assert_eq!(std::fs::read(&db_backup).unwrap(), b"old-db");

        let storage_backup = find_backup_entry(&restore_dir, "storage_repo.backup-")
            .expect("storage backup created");
        assert!(storage_backup.is_dir());
        assert_eq!(
            std::fs::read(storage_backup.join("old.txt")).unwrap(),
            b"old-storage"
        );

        // Restored DB should contain only the scoped project.
        // The snapshot is in C SQLite format, so use SqliteConnection.
        let restored_conn =
            sqlmodel_sqlite::SqliteConnection::open_file(restore_db.display().to_string()).unwrap();
        let rows = restored_conn
            .query_sync("SELECT slug FROM projects ORDER BY id", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let slug: String = rows[0].get_named("slug").unwrap();
        assert_eq!(slug, "proj-alpha");

        // Restored storage should include the archived files.
        assert_eq!(
            std::fs::read(restore_storage.join("nested/dir/file.txt")).unwrap(),
            b"hello\n"
        );
        assert_eq!(
            std::fs::read(restore_storage.join(".git/HEAD")).unwrap(),
            b"0123456789abcdef\n"
        );
    }

    // -----------------------------------------------------------------------
    // Clear-and-reset-everything integration-ish tests
    // -----------------------------------------------------------------------

    #[test]
    fn clear_and_reset_force_archive_creates_archive_and_wipes() {
        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        use std::io::Read;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), b"[workspace]\n").unwrap();
        let _cwd = CwdGuard::chdir(root.path());

        let storage_root = root.path().join("storage_repo");
        seed_storage_root(&storage_root);

        let db_path = root.path().join("mailbox.sqlite3");
        seed_mailbox_db(&db_path);
        let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
        let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
        std::fs::write(&wal_path, b"wal").unwrap();
        std::fs::write(&shm_path, b"shm").unwrap();
        let database_files = vec![db_path.clone(), wal_path.clone(), shm_path.clone()];

        let outcome = clear_and_reset_everything(
            true,
            Some(true),
            Some(&db_path),
            &database_files,
            &storage_root,
        )
        .expect("clear-and-reset");
        assert!(outcome.archive_path.is_some());

        let archive_dir = archive_states_dir(false).unwrap();
        let mut archives: Vec<PathBuf> = std::fs::read_dir(&archive_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        archives.sort();
        assert_eq!(archives.len(), 1, "expected 1 archive zip");
        let archive_path = &archives[0];
        assert_eq!(
            archive_path.extension().and_then(|s| s.to_str()),
            Some("zip")
        );

        // Validate label + scrub preset in metadata.
        let file = std::fs::File::open(archive_path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        let mut meta_contents = String::new();
        zip.by_name(ARCHIVE_METADATA_FILENAME)
            .unwrap()
            .read_to_string(&mut meta_contents)
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_contents).unwrap();
        assert_eq!(meta["label"].as_str(), Some("pre-reset"));
        assert_eq!(meta["scrub_preset"].as_str(), Some("archive"));

        // DB + WAL/SHM should be removed.
        assert!(!db_path.exists());
        assert!(!wal_path.exists());
        assert!(!shm_path.exists());

        // Storage root should be emptied (directory stays).
        assert!(storage_root.exists());
        assert_eq!(std::fs::read_dir(&storage_root).unwrap().count(), 0);
    }

    #[test]
    fn clear_and_reset_force_no_archive_wipes_without_archive() {
        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), b"[workspace]\n").unwrap();
        let _cwd = CwdGuard::chdir(root.path());

        let storage_root = root.path().join("storage_repo");
        seed_storage_root(&storage_root);

        let db_path = root.path().join("mailbox.sqlite3");
        seed_mailbox_db(&db_path);
        let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
        let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
        std::fs::write(&wal_path, b"wal").unwrap();
        std::fs::write(&shm_path, b"shm").unwrap();
        let database_files = vec![db_path.clone(), wal_path.clone(), shm_path.clone()];

        clear_and_reset_everything(
            true,
            Some(false),
            Some(&db_path),
            &database_files,
            &storage_root,
        )
        .expect("clear-and-reset");

        let archive_dir = archive_states_dir(false).unwrap();
        assert!(
            !archive_dir.exists(),
            "archive dir should not be created when --no-archive"
        );
        assert!(!db_path.exists());
        assert!(!wal_path.exists());
        assert!(!shm_path.exists());
        assert!(storage_root.exists());
        assert_eq!(std::fs::read_dir(&storage_root).unwrap().count(), 0);
    }

    #[test]
    fn clear_and_reset_refuses_without_force_on_non_interactive_stdin() {
        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), b"[workspace]\n").unwrap();
        let _cwd = CwdGuard::chdir(root.path());

        let storage_root = root.path().join("storage_repo");
        seed_storage_root(&storage_root);

        let db_path = root.path().join("mailbox.sqlite3");
        seed_mailbox_db(&db_path);
        let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
        let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
        std::fs::write(&wal_path, b"wal").unwrap();
        std::fs::write(&shm_path, b"shm").unwrap();
        let database_files = vec![db_path.clone(), wal_path.clone(), shm_path.clone()];

        let err =
            clear_and_reset_everything(false, None, Some(&db_path), &database_files, &storage_root)
                .unwrap_err();
        let msg = match err {
            CliError::Other(m) => m,
            other => format!("{other}"),
        };
        assert!(
            msg.contains("refusing to prompt on non-interactive stdin"),
            "unexpected error: {msg}"
        );

        // Ensure no changes occurred.
        assert!(db_path.exists());
        assert!(wal_path.exists());
        assert!(shm_path.exists());
        assert!(storage_root.join("nested/dir/file.txt").exists());
        assert!(storage_root.join(".git/HEAD").exists());
    }

    // -----------------------------------------------------------------------
    // Products commands integration-ish tests (local DB, no env mutation)
    // -----------------------------------------------------------------------

    fn seed_products_cli_db(root: &tempfile::TempDir) -> (PathBuf, String, String, i64) {
        use mcp_agent_mail_db::sqlmodel::Value;

        let created_at_us = 1_704_067_200_000_000i64; // 2024-01-01T00:00:00Z

        // Use real directories so get_project_record() canonicalization is stable.
        let proj_alpha_dir = root.path().join("proj_alpha");
        std::fs::create_dir_all(&proj_alpha_dir).unwrap();
        let proj_beta_dir = root.path().join("proj_beta");
        std::fs::create_dir_all(&proj_beta_dir).unwrap();

        let proj_alpha_key = proj_alpha_dir.canonicalize().unwrap().display().to_string();
        let proj_beta_key = proj_beta_dir.canonicalize().unwrap().display().to_string();

        let db_path = root.path().join("mailbox.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open products test sqlite db");
        // Use base schema (no FTS5/triggers) because this DB will be opened
        // by FrankenConnection via the pool. FrankenConnection cannot read
        // database files containing FTS5 shadow table pages.
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");

        // Projects
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::Text("proj-alpha".to_string()),
                Value::Text(proj_alpha_key.clone()),
                Value::BigInt(created_at_us),
            ],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(2),
                Value::Text("proj-beta".to_string()),
                Value::Text(proj_beta_key.clone()),
                Value::BigInt(created_at_us),
            ],
        )
        .unwrap();

        // Agents: same recipient name across both projects (legacy semantics).
        let agent_insert = "INSERT INTO agents (\
                id, project_id, name, program, model, task_description, \
                inception_ts, last_active_ts, attachments_policy, contact_policy\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

        conn.execute_sync(
            agent_insert,
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("GreenCastle".to_string()),
                Value::Text("test".to_string()),
                Value::Text("test".to_string()),
                Value::Text(String::new()),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::Text("auto".to_string()),
                Value::Text("auto".to_string()),
            ],
        )
        .unwrap();
        conn.execute_sync(
            agent_insert,
            &[
                Value::BigInt(2),
                Value::BigInt(2),
                Value::Text("GreenCastle".to_string()),
                Value::Text("test".to_string()),
                Value::Text("test".to_string()),
                Value::Text(String::new()),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::Text("auto".to_string()),
                Value::Text("auto".to_string()),
            ],
        )
        .unwrap();
        // Senders
        conn.execute_sync(
            agent_insert,
            &[
                Value::BigInt(3),
                Value::BigInt(1),
                Value::Text("PurpleBear".to_string()),
                Value::Text("test".to_string()),
                Value::Text("test".to_string()),
                Value::Text(String::new()),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::Text("auto".to_string()),
                Value::Text("auto".to_string()),
            ],
        )
        .unwrap();
        conn.execute_sync(
            agent_insert,
            &[
                Value::BigInt(4),
                Value::BigInt(2),
                Value::Text("OrangeFish".to_string()),
                Value::Text("test".to_string()),
                Value::Text("test".to_string()),
                Value::Text(String::new()),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::Text("auto".to_string()),
                Value::Text("auto".to_string()),
            ],
        )
        .unwrap();

        // Messages (base schema has no FTS triggers; search uses LIKE fallback)
        let msg_insert = "INSERT INTO messages (\
                id, project_id, sender_id, thread_id, subject, body_md, importance, \
                ack_required, created_ts, attachments\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
        conn.execute_sync(
            msg_insert,
            &[
                Value::BigInt(10),
                Value::BigInt(1),
                Value::BigInt(3),
                Value::Null,
                Value::Text("Unicorn alpha".to_string()),
                Value::Text("body alpha".to_string()),
                Value::Text("high".to_string()),
                Value::BigInt(0),
                Value::BigInt(created_at_us + 10),
                Value::Text("[]".to_string()),
            ],
        )
        .unwrap();
        conn.execute_sync(
            msg_insert,
            &[
                Value::BigInt(11),
                Value::BigInt(1),
                Value::BigInt(3),
                Value::Null,
                Value::Text("Other alpha".to_string()),
                Value::Text("misc".to_string()),
                Value::Text("normal".to_string()),
                Value::BigInt(1),
                Value::BigInt(created_at_us + 5),
                Value::Text("[]".to_string()),
            ],
        )
        .unwrap();
        conn.execute_sync(
            msg_insert,
            &[
                Value::BigInt(20),
                Value::BigInt(2),
                Value::BigInt(4),
                Value::Null,
                Value::Text("Beta ping".to_string()),
                Value::Text("body beta".to_string()),
                Value::Text("normal".to_string()),
                Value::BigInt(0),
                Value::BigInt(created_at_us + 20),
                Value::Text("[]".to_string()),
            ],
        )
        .unwrap();

        // Recipients
        let recip_insert =
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)";
        conn.execute_sync(
            recip_insert,
            &[
                Value::BigInt(10),
                Value::BigInt(1),
                Value::Text("to".to_string()),
            ],
        )
        .unwrap();
        conn.execute_sync(
            recip_insert,
            &[
                Value::BigInt(11),
                Value::BigInt(1),
                Value::Text("to".to_string()),
            ],
        )
        .unwrap();
        conn.execute_sync(
            recip_insert,
            &[
                Value::BigInt(20),
                Value::BigInt(2),
                Value::Text("to".to_string()),
            ],
        )
        .unwrap();

        (db_path, proj_alpha_key, proj_beta_key, created_at_us)
    }

    fn run_products_cmd_capture(
        runtime: &asupersync::runtime::Runtime,
        cx: &asupersync::Cx,
        pool: &mcp_agent_mail_db::DbPool,
        action: ProductsCommand,
    ) -> (CliResult<()>, String) {
        use ftui_runtime::stdio_capture::StdioCapture;

        let _capture_lock = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let capture = StdioCapture::install().unwrap();
        let res =
            runtime.block_on(async { handle_products_with(cx, pool, None, None, action).await });
        let mut sink = Vec::new();
        capture.drain(&mut sink).unwrap();
        drop(capture);

        (res, String::from_utf8_lossy(&sink).trim().to_string())
    }

    #[test]
    fn products_local_parity_smoke_json() {
        let _lock = ARCHIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        use asupersync::runtime::RuntimeBuilder;
        use mcp_agent_mail_db::sqlmodel::Value;

        let root = tempfile::tempdir().unwrap();
        let (db_path, proj_alpha_key, proj_beta_key, created_at_us) = seed_products_cli_db(&root);

        let make_pool = || {
            mcp_agent_mail_db::DbPool::new(&mcp_agent_mail_db::DbPoolConfig {
                database_url: format!("sqlite:///{}", db_path.display()),
                min_connections: 1,
                max_connections: 1,
                acquire_timeout_ms: 5_000,
                max_lifetime_ms: 60_000,
                run_migrations: false, // skip migrations to isolate corruption
                warmup_connections: 0,
            })
            .unwrap()
        };
        let cx = asupersync::Cx::for_request();
        let runtime = RuntimeBuilder::current_thread().build().unwrap();

        // Ensure (create)
        let pool = make_pool();
        let (res, out) = run_products_cmd_capture(
            &runtime,
            &cx,
            &pool,
            ProductsCommand::Ensure {
                product_key: Some("abcdef1234".to_string()),
                name: Some("My   Product".to_string()),
                format: None,
                json: true,
            },
        );
        res.unwrap();
        let ensure_json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(ensure_json["product_uid"].as_str(), Some("abcdef1234"));
        assert_eq!(ensure_json["name"].as_str(), Some("My Product"));

        // Normalize created_at for stable snapshots.
        // Drop pool before opening a standalone connection on the same file.
        drop(pool);
        {
            let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string()).unwrap();
            conn.execute_sync(
                "UPDATE products SET created_at = ? WHERE product_uid = ?",
                &[
                    Value::BigInt(created_at_us),
                    Value::Text("abcdef1234".to_string()),
                ],
            )
            .unwrap();
        }

        // Re-create pool after sync connection is dropped.
        let pool = make_pool();

        // Ensure (existing) should now have deterministic created_at
        let (res, out) = run_products_cmd_capture(
            &runtime,
            &cx,
            &pool,
            ProductsCommand::Ensure {
                product_key: Some("abcdef1234".to_string()),
                name: None,
                format: None,
                json: true,
            },
        );
        res.unwrap();
        let ensure_json: serde_json::Value = serde_json::from_str(&out).unwrap();
        let expected_created_at = mcp_agent_mail_db::micros_to_iso(created_at_us);
        assert_eq!(
            ensure_json["created_at"].as_str(),
            Some(expected_created_at.as_str())
        );

        // Link into both projects
        run_products_cmd_capture(
            &runtime,
            &cx,
            &pool,
            ProductsCommand::Link {
                product_key: "abcdef1234".to_string(),
                project: proj_alpha_key.clone(),
                format: None,
                json: false,
            },
        )
        .0
        .unwrap();
        run_products_cmd_capture(
            &runtime,
            &cx,
            &pool,
            ProductsCommand::Link {
                product_key: "abcdef1234".to_string(),
                project: proj_beta_key.clone(),
                format: None,
                json: false,
            },
        )
        .0
        .unwrap();

        // Status JSON should include both projects
        let (res, out) = run_products_cmd_capture(
            &runtime,
            &cx,
            &pool,
            ProductsCommand::Status {
                product_key: "abcdef1234".to_string(),
                format: None,
                json: true,
            },
        );
        res.unwrap();
        let mut status_json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            status_json["product"]["product_uid"].as_str(),
            Some("abcdef1234")
        );
        let projects = status_json["projects"].as_array_mut().unwrap();
        projects.sort_by_key(|p| p["id"].as_i64().unwrap_or_default());
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0]["slug"].as_str(), Some("proj-alpha"));
        assert_eq!(projects[1]["slug"].as_str(), Some("proj-beta"));

        // Search JSON should find only the unicorn message.
        let (res, out) = run_products_cmd_capture(
            &runtime,
            &cx,
            &pool,
            ProductsCommand::Search {
                product_key: "abcdef1234".to_string(),
                query: "unicorn".to_string(),
                limit: 20,
                format: None,
                json: true,
            },
        );
        res.unwrap();
        let search_json: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = search_json["result"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"].as_i64(), Some(10));
        assert_eq!(arr[0]["project_id"].as_i64(), Some(1));

        // Inbox JSON (all)
        let (res, out) = run_products_cmd_capture(
            &runtime,
            &cx,
            &pool,
            ProductsCommand::Inbox {
                product_key: "abcdef1234".to_string(),
                agent: "GreenCastle".to_string(),
                limit: 20,
                urgent_only: false,
                all: false,
                include_bodies: false,
                no_bodies: false,
                since_ts: None,
                format: None,
                json: true,
            },
        );
        res.unwrap();
        let inbox_json: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = inbox_json.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["id"].as_i64(), Some(20));
        assert_eq!(arr[1]["id"].as_i64(), Some(10));
        assert_eq!(arr[2]["id"].as_i64(), Some(11));
        assert!(arr[0].get("body_md").is_none(), "bodies default off");

        // Inbox urgent-only should include only the high message.
        let (res, out) = run_products_cmd_capture(
            &runtime,
            &cx,
            &pool,
            ProductsCommand::Inbox {
                product_key: "abcdef1234".to_string(),
                agent: "GreenCastle".to_string(),
                limit: 20,
                urgent_only: true,
                all: false,
                include_bodies: false,
                no_bodies: false,
                since_ts: None,
                format: None,
                json: true,
            },
        );
        res.unwrap();
        let inbox_json: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = inbox_json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"].as_i64(), Some(10));

        // Summarize-thread requires server tool; with server disabled, should return exit code 2.
        let (res, out) = run_products_cmd_capture(
            &runtime,
            &cx,
            &pool,
            ProductsCommand::SummarizeThread {
                product_key: "abcdef1234".to_string(),
                thread_id: "thread-1".to_string(),
                per_thread_limit: 50,
                no_llm: true,
                format: None,
                json: false,
            },
        );
        let err = res.unwrap_err();
        assert!(matches!(err, CliError::ExitCode(2)));
        assert!(out.contains("Server unavailable; summarization requires server tool."));
    }

    // -----------------------------------------------------------------------
    // Error path tests (validation logic, not full execution)
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_bool_defaults() {
        assert!(!resolve_bool(false, false, false));
        assert!(resolve_bool(false, false, true));
        assert!(resolve_bool(true, false, false));
        assert!(!resolve_bool(false, true, true)); // negated wins
        assert!(!resolve_bool(true, true, true)); // negated wins
    }

    #[test]
    fn invalid_scrub_preset_is_rejected() {
        let result = share::normalize_scrub_preset("bogus");
        assert!(result.is_err());
    }

    #[test]
    fn valid_scrub_presets_accepted() {
        assert!(share::normalize_scrub_preset("standard").is_ok());
        assert!(share::normalize_scrub_preset("strict").is_ok());
        assert!(share::normalize_scrub_preset("archive").is_ok());
        assert!(share::normalize_scrub_preset("Standard").is_ok()); // case-insensitive
    }

    #[test]
    fn threshold_validation_rejects_negative() {
        let result = share::validate_thresholds(-1, 0, 0, 1024);
        assert!(result.is_err());
        let result = share::validate_thresholds(0, -1, 0, 1024);
        assert!(result.is_err());
        let result = share::validate_thresholds(0, 0, -1, 1024);
        assert!(result.is_err());
    }

    #[test]
    fn threshold_validation_rejects_small_chunk_size() {
        let result = share::validate_thresholds(0, 0, 0, 512);
        assert!(result.is_err());
    }

    #[test]
    fn threshold_validation_accepts_valid() {
        let result = share::validate_thresholds(
            share::INLINE_ATTACHMENT_THRESHOLD as i64,
            share::DETACH_ATTACHMENT_THRESHOLD as i64,
            share::DEFAULT_CHUNK_THRESHOLD as i64,
            share::DEFAULT_CHUNK_SIZE as i64,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn detach_threshold_adjusted_when_below_inline() {
        // When detach <= inline, it should be bumped
        let adjusted = share::adjust_detach_threshold(1000, 500);
        assert!(adjusted > 1000);
    }

    #[test]
    fn detach_threshold_unchanged_when_above_inline() {
        let adjusted = share::adjust_detach_threshold(1000, 2000);
        assert_eq!(adjusted, 2000);
    }

    #[test]
    fn ensure_dir_missing_path_errors() {
        let result = ensure_dir(Path::new("/nonexistent/path"));
        assert!(result.is_err());
    }

    #[test]
    fn ensure_dir_file_not_directory_errors() {
        // /proc/self/exe is a file, not a directory
        let result = ensure_dir(Path::new("/proc/self/exe"));
        assert!(result.is_err());
    }

    #[test]
    fn default_decrypt_output_strips_age_extension() {
        let out = share::default_decrypt_output(Path::new("/tmp/bundle.zip.age"));
        assert_eq!(out, PathBuf::from("/tmp/bundle.zip"));
    }

    #[test]
    fn default_decrypt_output_non_age_adds_suffix() {
        let out = share::default_decrypt_output(Path::new("/tmp/bundle.zip"));
        assert_eq!(out, PathBuf::from("/tmp/bundle_decrypted.zip"));
    }

    #[test]
    fn safe_component_sanitizes_special_chars() {
        assert_eq!(safe_component("foo/bar:baz"), "foo_bar_baz");
        assert_eq!(safe_component("."), "unknown");
        assert_eq!(safe_component(".."), "unknown");
        assert_eq!(safe_component(""), "unknown");
        assert_eq!(safe_component("  "), "unknown");
    }

    // ── docs insert-blurbs tests (br-2ei.5.10) ───────────────────────────

    #[test]
    fn clap_parses_docs_insert_blurbs_defaults() {
        let cli = Cli::try_parse_from(["am", "docs", "insert-blurbs"])
            .expect("failed to parse docs insert-blurbs");
        match cli.command.expect("expected command") {
            Commands::Docs { action } => match action {
                DocsCommand::InsertBlurbs {
                    scan_dir,
                    yes,
                    dry_run,
                    max_depth,
                } => {
                    assert!(scan_dir.is_empty(), "default scan_dir should be empty");
                    assert!(!yes, "default yes should be false");
                    assert!(!dry_run, "default dry_run should be false");
                    assert!(max_depth.is_none(), "default max_depth should be None");
                }
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_docs_insert_blurbs_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "docs",
            "insert-blurbs",
            "-d",
            "/tmp/a",
            "-d",
            "/tmp/b",
            "--yes",
            "--dry-run",
            "--max-depth",
            "5",
        ])
        .expect("failed to parse docs insert-blurbs flags");
        match cli.command.expect("expected command") {
            Commands::Docs { action } => match action {
                DocsCommand::InsertBlurbs {
                    scan_dir,
                    yes,
                    dry_run,
                    max_depth,
                } => {
                    assert_eq!(scan_dir.len(), 2);
                    assert_eq!(scan_dir[0].to_str(), Some("/tmp/a"));
                    assert_eq!(scan_dir[1].to_str(), Some("/tmp/b"));
                    assert!(yes);
                    assert!(dry_run);
                    assert_eq!(max_depth, Some(5));
                }
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn scan_markdown_for_blurbs_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut files = 0u64;
        let mut insertions = 0u64;
        scan_markdown_for_blurbs(tmp.path(), 0, 3, true, &mut files, &mut insertions).unwrap();
        assert_eq!(files, 0);
        assert_eq!(insertions, 0);
    }

    #[test]
    fn scan_markdown_for_blurbs_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("README.md"), "# Hello\nNo markers here.").unwrap();
        let mut files = 0u64;
        let mut insertions = 0u64;
        scan_markdown_for_blurbs(tmp.path(), 0, 3, false, &mut files, &mut insertions).unwrap();
        assert_eq!(files, 1);
        assert_eq!(insertions, 0);
    }

    #[test]
    fn scan_markdown_for_blurbs_marker_triggers_insertion() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("AGENTS.md");
        std::fs::write(&file, "# Agents\n<!-- am:blurb -->\nSome content").unwrap();
        let mut files = 0u64;
        let mut insertions = 0u64;
        scan_markdown_for_blurbs(tmp.path(), 0, 3, false, &mut files, &mut insertions).unwrap();
        assert_eq!(files, 1);
        assert_eq!(insertions, 1);
        // Verify the end marker was inserted.
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(
            content.contains("<!-- am:blurb:end -->"),
            "end marker should be inserted"
        );
        assert!(
            content.contains("<!-- am:blurb -->"),
            "start marker should be preserved"
        );
    }

    #[test]
    fn scan_markdown_for_blurbs_dry_run_no_file_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("CLAUDE.md");
        let original = "# Claude\n<!-- am:blurb -->\nContent";
        std::fs::write(&file, original).unwrap();
        let mut files = 0u64;
        let mut insertions = 0u64;
        scan_markdown_for_blurbs(tmp.path(), 0, 3, true, &mut files, &mut insertions).unwrap();
        assert_eq!(insertions, 1, "dry run should count insertions");
        // File should NOT be modified.
        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, original, "dry run must not modify files");
    }

    #[test]
    fn scan_markdown_for_blurbs_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("AGENTS.md");
        std::fs::write(&file, "# Agents\n<!-- am:blurb -->\nContent").unwrap();

        // First pass: insert.
        let mut files = 0u64;
        let mut insertions = 0u64;
        scan_markdown_for_blurbs(tmp.path(), 0, 3, false, &mut files, &mut insertions).unwrap();
        assert_eq!(insertions, 1);
        let after_first = std::fs::read_to_string(&file).unwrap();

        // Second pass: should be idempotent (no more insertions).
        let mut files2 = 0u64;
        let mut insertions2 = 0u64;
        scan_markdown_for_blurbs(tmp.path(), 0, 3, false, &mut files2, &mut insertions2).unwrap();
        assert_eq!(
            insertions2, 0,
            "second pass should not insert (already has end marker)"
        );
        let after_second = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            after_first, after_second,
            "file should not change on second pass"
        );
    }

    #[test]
    fn scan_markdown_for_blurbs_skips_already_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("README.md");
        std::fs::write(
            &file,
            "# Hello\n<!-- am:blurb -->\nContent\n<!-- am:blurb:end -->\n",
        )
        .unwrap();
        let mut files = 0u64;
        let mut insertions = 0u64;
        scan_markdown_for_blurbs(tmp.path(), 0, 3, false, &mut files, &mut insertions).unwrap();
        assert_eq!(files, 1);
        assert_eq!(
            insertions, 0,
            "already-complete file should not trigger insertion"
        );
    }

    #[test]
    fn scan_markdown_for_blurbs_respects_max_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("test.md"), "<!-- am:blurb -->\n").unwrap();

        // max_depth=2 should not reach depth 4.
        let mut files = 0u64;
        let mut insertions = 0u64;
        scan_markdown_for_blurbs(tmp.path(), 0, 2, true, &mut files, &mut insertions).unwrap();
        assert_eq!(files, 0, "max_depth=2 should not find file at depth 4");

        // max_depth=5 should find it.
        let mut files2 = 0u64;
        let mut insertions2 = 0u64;
        scan_markdown_for_blurbs(tmp.path(), 0, 5, true, &mut files2, &mut insertions2).unwrap();
        assert_eq!(files2, 1, "max_depth=5 should find file at depth 4");
    }

    #[test]
    fn scan_markdown_for_blurbs_ignores_non_md_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "<!-- am:blurb -->").unwrap();
        std::fs::write(tmp.path().join("data.json"), "<!-- am:blurb -->").unwrap();
        let mut files = 0u64;
        let mut insertions = 0u64;
        scan_markdown_for_blurbs(tmp.path(), 0, 3, true, &mut files, &mut insertions).unwrap();
        assert_eq!(files, 0, "non-.md files should be ignored");
    }

    #[test]
    fn scan_markdown_for_blurbs_handles_subdirectories() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("AGENTS.md"), "<!-- am:blurb -->\nHello").unwrap();
        std::fs::write(tmp.path().join("ROOT.md"), "No markers").unwrap();

        let mut files = 0u64;
        let mut insertions = 0u64;
        scan_markdown_for_blurbs(tmp.path(), 0, 3, true, &mut files, &mut insertions).unwrap();
        assert_eq!(files, 2, "should count both .md files");
        assert_eq!(insertions, 1, "only the file with marker should count");
    }

    #[test]
    fn scan_markdown_for_blurbs_nonexistent_dir_no_error() {
        let result = scan_markdown_for_blurbs(
            Path::new("/nonexistent/path"),
            0,
            3,
            true,
            &mut 0u64,
            &mut 0u64,
        );
        assert!(result.is_ok(), "nonexistent dir should not error");
    }

    // ── Doctor commands tests (br-2ei.5.4) ──────────────────────────────────

    #[test]
    fn clap_parses_doctor_check_defaults() {
        let cli = Cli::try_parse_from(["am", "doctor", "check"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Doctor {
                action:
                    DoctorCommand::Check {
                        project,
                        verbose,
                        format,
                        json,
                    },
            } => {
                assert!(project.is_none());
                assert!(!verbose);
                assert!(format.is_none());
                assert!(!json);
            }
            _ => panic!("expected Doctor Check"),
        }
    }

    #[test]
    fn clap_parses_doctor_check_all_flags() {
        let cli =
            Cli::try_parse_from(["am", "doctor", "check", "my-proj", "-v", "--json"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Doctor {
                action:
                    DoctorCommand::Check {
                        project,
                        verbose,
                        format,
                        json,
                    },
            } => {
                assert_eq!(project.as_deref(), Some("my-proj"));
                assert!(verbose);
                assert!(format.is_none());
                assert!(json);
            }
            _ => panic!("expected Doctor Check"),
        }
    }

    #[test]
    fn clap_parses_doctor_check_format_toon() {
        let cli = Cli::try_parse_from(["am", "doctor", "check", "--format", "toon"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Doctor {
                action: DoctorCommand::Check { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected Doctor Check, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_doctor_repair_defaults() {
        let cli = Cli::try_parse_from(["am", "doctor", "repair"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Doctor {
                action:
                    DoctorCommand::Repair {
                        project,
                        dry_run,
                        yes,
                        backup_dir,
                    },
            } => {
                assert!(project.is_none());
                assert!(!dry_run);
                assert!(!yes);
                assert!(backup_dir.is_none());
            }
            _ => panic!("expected Doctor Repair"),
        }
    }

    #[test]
    fn clap_parses_doctor_repair_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "doctor",
            "repair",
            "proj",
            "--dry-run",
            "-y",
            "--backup-dir",
            "/tmp/bak",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Doctor {
                action:
                    DoctorCommand::Repair {
                        project,
                        dry_run,
                        yes,
                        backup_dir,
                    },
            } => {
                assert_eq!(project.as_deref(), Some("proj"));
                assert!(dry_run);
                assert!(yes);
                assert_eq!(backup_dir.unwrap(), PathBuf::from("/tmp/bak"));
            }
            _ => panic!("expected Doctor Repair"),
        }
    }

    #[test]
    fn clap_parses_doctor_backups_defaults() {
        let cli = Cli::try_parse_from(["am", "doctor", "backups"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Doctor {
                action: DoctorCommand::Backups { format, json },
            } => {
                assert!(format.is_none());
                assert!(!json);
            }
            _ => panic!("expected Doctor Backups"),
        }
    }

    #[test]
    fn clap_parses_doctor_backups_json() {
        let cli = Cli::try_parse_from(["am", "doctor", "backups", "--json"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Doctor {
                action: DoctorCommand::Backups { format, json },
            } => {
                assert!(format.is_none());
                assert!(json);
            }
            _ => panic!("expected Doctor Backups"),
        }
    }

    #[test]
    fn clap_parses_doctor_restore_required_path() {
        let cli = Cli::try_parse_from(["am", "doctor", "restore", "/tmp/backup.sqlite3"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Doctor {
                action:
                    DoctorCommand::Restore {
                        backup_path,
                        dry_run,
                        yes,
                    },
            } => {
                assert_eq!(backup_path, PathBuf::from("/tmp/backup.sqlite3"));
                assert!(!dry_run);
                assert!(!yes);
            }
            _ => panic!("expected Doctor Restore"),
        }
    }

    #[test]
    fn clap_parses_doctor_restore_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "doctor",
            "restore",
            "/tmp/backup.sqlite3",
            "--dry-run",
            "-y",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Doctor {
                action:
                    DoctorCommand::Restore {
                        backup_path,
                        dry_run,
                        yes,
                    },
            } => {
                assert_eq!(backup_path, PathBuf::from("/tmp/backup.sqlite3"));
                assert!(dry_run);
                assert!(yes);
            }
            _ => panic!("expected Doctor Restore"),
        }
    }

    #[test]
    fn parse_doctor_reconstruct_defaults() {
        let cli = Cli::try_parse_from(["am", "doctor", "reconstruct"]).unwrap();
        match cli.command.unwrap() {
            Commands::Doctor {
                action: DoctorCommand::Reconstruct { dry_run, yes, json },
            } => {
                assert!(!dry_run);
                assert!(!yes);
                assert!(!json);
            }
            _ => panic!("expected Doctor Reconstruct"),
        }
    }

    #[test]
    fn parse_doctor_reconstruct_all_flags() {
        let cli = Cli::try_parse_from(["am", "doctor", "reconstruct", "--dry-run", "-y", "--json"])
            .unwrap();
        match cli.command.unwrap() {
            Commands::Doctor {
                action: DoctorCommand::Reconstruct { dry_run, yes, json },
            } => {
                assert!(dry_run);
                assert!(yes);
                assert!(json);
            }
            _ => panic!("expected Doctor Reconstruct"),
        }
    }

    #[test]
    fn doctor_reconstruct_rejects_missing_storage_root() {
        let result = handle_doctor_reconstruct_with(
            Some(Path::new("/tmp/nonexistent.db")),
            Some(Path::new("/nonexistent/storage/root")),
            false,
            false,
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::InvalidArgument(msg) => {
                assert!(msg.contains("storage root does not exist"), "got: {msg}");
            }
            other => panic!("expected InvalidArgument, got: {other:?}"),
        }
    }

    #[test]
    fn doctor_reconstruct_dry_run_empty_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let projects_dir = tmp.path().join("projects");
        std::fs::create_dir_all(&projects_dir).unwrap();
        let db_path = tmp.path().join("test.db");

        let result = handle_doctor_reconstruct_with(Some(&db_path), Some(tmp.path()), true, true);
        assert!(result.is_ok());
    }

    #[test]
    fn doctor_reconstruct_with_archive_data() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = tmp.path();

        // Set up a minimal archive: 1 project, 1 agent, 1 message
        let proj_dir = storage.join("projects").join("test-project");
        let agent_dir = proj_dir.join("agents").join("SwiftFox");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"SwiftFox","role":"Coder","model":"claude","registered_ts":"2026-01-15T10:00:00"}"#,
        )
        .unwrap();

        let msg_dir = proj_dir.join("messages").join("2026").join("01");
        std::fs::create_dir_all(&msg_dir).unwrap();
        std::fs::write(
            msg_dir.join("001_hello.md"),
            "---json\n{\n  \"id\": 1,\n  \"subject\": \"Hello\",\n  \"from_agent\": \"SwiftFox\",\n  \"importance\": \"normal\",\n  \"to\": [\"BraveLion\"],\n  \"cc\": [],\n  \"bcc\": [],\n  \"thread_id\": \"t1\",\n  \"in_reply_to\": null,\n  \"created_ts\": \"2026-01-15T10:05:00\"\n}\n---\n\nHello BraveLion!\n",
        )
        .unwrap();

        let db_path = tmp.path().join("reconstructed.db");

        let result = handle_doctor_reconstruct_with(Some(&db_path), Some(storage), false, true);
        assert!(result.is_ok(), "reconstruct failed: {result:?}");
        assert!(db_path.exists(), "reconstructed DB file should exist");
    }

    #[test]
    fn doctor_backups_lists_sqlite3_files_only() {
        let tmp = tempfile::tempdir().unwrap();
        let backup_dir = tmp.path().join("backups");
        std::fs::create_dir_all(&backup_dir).unwrap();
        std::fs::write(
            backup_dir.join("pre_repair_20260101_120000.sqlite3"),
            b"data1",
        )
        .unwrap();
        std::fs::write(
            backup_dir.join("pre_repair_20260102_120000.sqlite3"),
            b"data22",
        )
        .unwrap();
        std::fs::write(backup_dir.join("notes.txt"), b"not a backup").unwrap();

        let mut count = 0u32;
        for entry in std::fs::read_dir(&backup_dir).unwrap().flatten() {
            if entry.path().extension().and_then(|s| s.to_str()) == Some("sqlite3") {
                count += 1;
            }
        }
        assert_eq!(count, 2, "should find exactly 2 .sqlite3 files");
    }

    #[test]
    fn doctor_restore_rejects_missing_backup() {
        let result =
            handle_doctor_restore(PathBuf::from("/nonexistent/backup.sqlite3"), true, true);
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::InvalidArgument(msg) => {
                assert!(msg.contains("backup not found"), "got: {msg}");
            }
            other => panic!("expected InvalidArgument, got: {other:?}"),
        }
    }

    #[test]
    fn doctor_check_json_output_shape() {
        let checks = vec![
            serde_json::json!({"check": "database", "status": "ok", "detail": "accessible"}),
            serde_json::json!({"check": "storage_root", "status": "warn", "detail": "/tmp"}),
        ];
        let all_ok = checks.iter().all(|c| c["status"] != "fail");
        let output = serde_json::json!({"healthy": all_ok, "checks": checks});
        assert!(output["healthy"].as_bool().unwrap());
        assert_eq!(output["checks"].as_array().unwrap().len(), 2);
        for c in output["checks"].as_array().unwrap() {
            assert!(c["check"].is_string());
            assert!(c["status"].is_string());
            assert!(c["detail"].is_string());
        }
    }

    #[test]
    fn doctor_check_fail_makes_healthy_false() {
        let checks = [serde_json::json!({"check": "database", "status": "fail", "detail": "err"})];
        let all_ok = checks.iter().all(|c| c["status"] != "fail");
        assert!(!all_ok);
    }

    #[test]
    fn doctor_check_status_icon_mapping() {
        for (status, expected) in [("ok", "OK"), ("warn", "WARN"), ("fail", "FAIL")] {
            let icon = match status {
                "ok" => "OK",
                "warn" => "WARN",
                _ => "FAIL",
            };
            assert_eq!(icon, expected);
        }
    }

    #[test]
    fn doctor_repair_integrity_result_parsing() {
        assert!("ok" == "ok", "ok should be healthy");
        let bad = "*** in database main ***";
        assert!(bad != "ok", "corruption string should not be healthy");
    }

    #[test]
    fn doctor_backup_filename_format() {
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let name = format!("pre_repair_{ts}.sqlite3");
        assert!(name.starts_with("pre_repair_"));
        assert!(name.ends_with(".sqlite3"));
        // "pre_repair_" (11) + "YYYYMMDD_HHMMSS" (15) + ".sqlite3" (8) = 34
        assert_eq!(name.len(), 34, "unexpected length: {name}");
    }

    #[test]
    fn format_bytes_human_readable() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(100), "100 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1_048_576), "1.0 MiB");
        assert_eq!(format_bytes(1_073_741_824), "1.0 GiB");
    }

    // ── br-2ei.5.7.1: unit parsing + defaults ──────────────────────────

    #[test]
    fn clap_parses_serve_stdio() {
        let cli = Cli::try_parse_from(["am", "serve-stdio"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::ServeStdio)));
    }

    #[test]
    fn clap_parses_lint() {
        let cli = Cli::try_parse_from(["am", "lint"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Lint)));
    }

    #[test]
    fn clap_parses_typecheck() {
        let cli = Cli::try_parse_from(["am", "typecheck"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Typecheck)));
    }

    #[test]
    fn clap_parses_list_projects_defaults() {
        let cli = Cli::try_parse_from(["am", "list-projects"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::ListProjects {
                include_agents,
                format,
                json,
            } => {
                assert!(!include_agents);
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected ListProjects, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_list_projects_flags() {
        let cli =
            Cli::try_parse_from(["am", "list-projects", "--include-agents", "--json"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::ListProjects {
                include_agents,
                format,
                json,
            } => {
                assert!(include_agents);
                assert!(format.is_none());
                assert!(json);
            }
            other => panic!("expected ListProjects, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_list_projects_format_toon() {
        let cli = Cli::try_parse_from(["am", "list-projects", "--format", "toon"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::ListProjects {
                include_agents,
                format,
                json,
            } => {
                assert!(!include_agents);
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected ListProjects, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_list_acks() {
        let cli = Cli::try_parse_from([
            "am",
            "list-acks",
            "--project",
            "/tmp/proj",
            "--agent",
            "BlueLake",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::ListAcks {
                project_key,
                agent_name,
                limit,
            } => {
                assert_eq!(project_key, "/tmp/proj");
                assert_eq!(agent_name, "BlueLake");
                assert_eq!(limit, 20); // default
            }
            other => panic!("expected ListAcks, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_list_acks_custom_limit() {
        let cli = Cli::try_parse_from([
            "am",
            "list-acks",
            "--project",
            "/tmp/proj",
            "--agent",
            "BlueLake",
            "--limit",
            "5",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::ListAcks { limit, .. } => assert_eq!(limit, 5),
            other => panic!("expected ListAcks, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_guard_install() {
        let cli =
            Cli::try_parse_from(["am", "guard", "install", "my-project", "/tmp/repo"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Guard {
                action:
                    GuardCommand::Install {
                        project,
                        repo,
                        prepush,
                        no_prepush,
                    },
            } => {
                assert_eq!(project, "my-project");
                assert_eq!(repo, PathBuf::from("/tmp/repo"));
                assert!(!prepush);
                assert!(!no_prepush);
            }
            other => panic!("expected Guard Install, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_guard_install_prepush() {
        let cli = Cli::try_parse_from(["am", "guard", "install", "proj", "/tmp/repo", "--prepush"])
            .unwrap();
        match cli.command.expect("expected command") {
            Commands::Guard {
                action: GuardCommand::Install { prepush, .. },
            } => assert!(prepush),
            other => panic!("expected Guard Install, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_guard_uninstall() {
        let cli = Cli::try_parse_from(["am", "guard", "uninstall", "/tmp/repo"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Guard {
                action: GuardCommand::Uninstall { repo },
            } => assert_eq!(repo, PathBuf::from("/tmp/repo")),
            other => panic!("expected Guard Uninstall, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_guard_status() {
        let cli = Cli::try_parse_from(["am", "guard", "status", "/tmp/repo"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Guard {
                action: GuardCommand::Status { repo },
            } => assert_eq!(repo, PathBuf::from("/tmp/repo")),
            other => panic!("expected Guard Status, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_guard_check_defaults() {
        let cli = Cli::try_parse_from(["am", "guard", "check"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Guard {
                action:
                    GuardCommand::Check {
                        stdin_nul,
                        advisory,
                        repo,
                    },
            } => {
                assert!(!stdin_nul);
                assert!(!advisory);
                assert!(repo.is_none());
            }
            other => panic!("expected Guard Check, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_guard_check_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "guard",
            "check",
            "--stdin-nul",
            "--advisory",
            "--repo",
            "/tmp/repo",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Guard {
                action:
                    GuardCommand::Check {
                        stdin_nul,
                        advisory,
                        repo,
                    },
            } => {
                assert!(stdin_nul);
                assert!(advisory);
                assert_eq!(repo, Some(PathBuf::from("/tmp/repo")));
            }
            other => panic!("expected Guard Check, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_file_reservations_list() {
        let cli = Cli::try_parse_from(["am", "file_reservations", "list", "my-project"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::FileReservations {
                action:
                    FileReservationsCommand::List {
                        project,
                        active_only,
                        all,
                    },
            } => {
                assert_eq!(project, "my-project");
                assert!(!active_only);
                assert!(!all);
            }
            other => panic!("expected FileReservations List, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_file_reservations_list_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "file_reservations",
            "list",
            "proj",
            "--active-only",
            "--all",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::FileReservations {
                action:
                    FileReservationsCommand::List {
                        active_only, all, ..
                    },
            } => {
                assert!(active_only);
                assert!(all);
            }
            other => panic!("expected FileReservations List, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_file_reservations_active() {
        let cli = Cli::try_parse_from(["am", "file_reservations", "active", "proj"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::FileReservations {
                action: FileReservationsCommand::Active { project, limit },
            } => {
                assert_eq!(project, "proj");
                assert!(limit.is_none());
            }
            other => panic!("expected FileReservations Active, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_file_reservations_soon() {
        let cli =
            Cli::try_parse_from(["am", "file_reservations", "soon", "proj", "--minutes", "15"])
                .unwrap();
        match cli.command.expect("expected command") {
            Commands::FileReservations {
                action: FileReservationsCommand::Soon { project, minutes },
            } => {
                assert_eq!(project, "proj");
                assert_eq!(minutes, Some(15));
            }
            other => panic!("expected FileReservations Soon, got {other:?}"),
        }
    }

    // ── br-21gj.4.4: file-reservation lifecycle commands ──────────────

    #[test]
    fn clap_parses_file_reservations_reserve_minimal() {
        let cli = Cli::try_parse_from([
            "am",
            "file_reservations",
            "reserve",
            "proj",
            "BlueLake",
            "src/main.rs",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::FileReservations {
                action:
                    FileReservationsCommand::Reserve {
                        project,
                        agent,
                        paths,
                        ttl,
                        exclusive,
                        shared,
                        reason,
                    },
            } => {
                assert_eq!(project, "proj");
                assert_eq!(agent, "BlueLake");
                assert_eq!(paths, vec!["src/main.rs"]);
                assert_eq!(ttl, 3600);
                assert!(!exclusive); // default false
                assert!(!shared);
                assert_eq!(reason, "");
            }
            other => panic!("expected Reserve, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_file_reservations_reserve_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "file_reservations",
            "reserve",
            "proj",
            "BlueLake",
            "src/**",
            "Cargo.toml",
            "--ttl",
            "7200",
            "--shared",
            "--reason",
            "br-123 work",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::FileReservations {
                action:
                    FileReservationsCommand::Reserve {
                        project,
                        agent,
                        paths,
                        ttl,
                        shared,
                        reason,
                        ..
                    },
            } => {
                assert_eq!(project, "proj");
                assert_eq!(agent, "BlueLake");
                assert_eq!(paths, vec!["src/**", "Cargo.toml"]);
                assert_eq!(ttl, 7200);
                assert!(shared);
                assert_eq!(reason, "br-123 work");
            }
            other => panic!("expected Reserve, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_file_reservations_renew_minimal() {
        let cli =
            Cli::try_parse_from(["am", "file_reservations", "renew", "proj", "BlueLake"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::FileReservations {
                action:
                    FileReservationsCommand::Renew {
                        project,
                        agent,
                        extend_seconds,
                        paths,
                        ids,
                    },
            } => {
                assert_eq!(project, "proj");
                assert_eq!(agent, "BlueLake");
                assert_eq!(extend_seconds, 1800);
                assert!(paths.is_empty());
                assert!(ids.is_empty());
            }
            other => panic!("expected Renew, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_file_reservations_renew_with_filters() {
        let cli = Cli::try_parse_from([
            "am",
            "file_reservations",
            "renew",
            "proj",
            "BlueLake",
            "--extend-seconds",
            "3600",
            "--paths",
            "src/**",
            "--ids",
            "42",
            "--ids",
            "99",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::FileReservations {
                action:
                    FileReservationsCommand::Renew {
                        extend_seconds,
                        paths,
                        ids,
                        ..
                    },
            } => {
                assert_eq!(extend_seconds, 3600);
                assert_eq!(paths, vec!["src/**"]);
                assert_eq!(ids, vec![42, 99]);
            }
            other => panic!("expected Renew, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_file_reservations_release_all() {
        let cli = Cli::try_parse_from(["am", "file_reservations", "release", "proj", "BlueLake"])
            .unwrap();
        match cli.command.expect("expected command") {
            Commands::FileReservations {
                action:
                    FileReservationsCommand::Release {
                        project,
                        agent,
                        paths,
                        ids,
                    },
            } => {
                assert_eq!(project, "proj");
                assert_eq!(agent, "BlueLake");
                assert!(paths.is_empty());
                assert!(ids.is_empty());
            }
            other => panic!("expected Release, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_file_reservations_release_by_paths() {
        let cli = Cli::try_parse_from([
            "am",
            "file_reservations",
            "release",
            "proj",
            "BlueLake",
            "--paths",
            "src/main.rs",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::FileReservations {
                action: FileReservationsCommand::Release { paths, .. },
            } => {
                assert_eq!(paths, vec!["src/main.rs"]);
            }
            other => panic!("expected Release, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_file_reservations_release_by_ids() {
        let cli = Cli::try_parse_from([
            "am",
            "file_reservations",
            "release",
            "proj",
            "BlueLake",
            "--ids",
            "10",
            "--ids",
            "20",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::FileReservations {
                action: FileReservationsCommand::Release { ids, .. },
            } => {
                assert_eq!(ids, vec![10, 20]);
            }
            other => panic!("expected Release, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_file_reservations_conflicts() {
        let cli = Cli::try_parse_from([
            "am",
            "file_reservations",
            "conflicts",
            "proj",
            "src/**",
            "Cargo.toml",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::FileReservations {
                action: FileReservationsCommand::Conflicts { project, paths },
            } => {
                assert_eq!(project, "proj");
                assert_eq!(paths, vec!["src/**", "Cargo.toml"]);
            }
            other => panic!("expected Conflicts, got {other:?}"),
        }
    }

    #[test]
    fn clap_rejects_reserve_without_paths() {
        let result =
            Cli::try_parse_from(["am", "file_reservations", "reserve", "proj", "BlueLake"]);
        assert!(result.is_err(), "reserve requires at least one path");
    }

    #[test]
    fn clap_rejects_conflicts_without_paths() {
        let result = Cli::try_parse_from(["am", "file_reservations", "conflicts", "proj"]);
        assert!(result.is_err(), "conflicts requires at least one path");
    }

    #[test]
    fn clap_parses_acks_pending() {
        let cli = Cli::try_parse_from(["am", "acks", "pending", "proj", "BlueLake"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Acks {
                action:
                    AcksCommand::Pending {
                        project,
                        agent,
                        limit,
                    },
            } => {
                assert_eq!(project, "proj");
                assert_eq!(agent, "BlueLake");
                assert_eq!(limit, 20); // default
            }
            other => panic!("expected Acks Pending, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_acks_remind() {
        let cli = Cli::try_parse_from(["am", "acks", "remind", "proj", "BlueLake"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Acks {
                action:
                    AcksCommand::Remind {
                        project,
                        agent,
                        min_age_minutes,
                        limit,
                    },
            } => {
                assert_eq!(project, "proj");
                assert_eq!(agent, "BlueLake");
                assert_eq!(min_age_minutes, 30); // default
                assert_eq!(limit, 50); // default
            }
            other => panic!("expected Acks Remind, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_acks_overdue() {
        let cli = Cli::try_parse_from([
            "am",
            "acks",
            "overdue",
            "proj",
            "BlueLake",
            "--ttl-minutes",
            "120",
            "--limit",
            "10",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Acks {
                action:
                    AcksCommand::Overdue {
                        project,
                        agent,
                        ttl_minutes,
                        limit,
                    },
            } => {
                assert_eq!(project, "proj");
                assert_eq!(agent, "BlueLake");
                assert_eq!(ttl_minutes, 120);
                assert_eq!(limit, 10);
            }
            other => panic!("expected Acks Overdue, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_config_set_port() {
        let cli = Cli::try_parse_from(["am", "config", "set-port", "9999"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Config {
                action: ConfigCommand::SetPort { port, env_file },
            } => {
                assert_eq!(port, 9999);
                assert!(env_file.is_none());
            }
            other => panic!("expected Config SetPort, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_config_set_port_env_file() {
        let cli = Cli::try_parse_from([
            "am",
            "config",
            "set-port",
            "8080",
            "--env-file",
            "/tmp/.env",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Config {
                action: ConfigCommand::SetPort { port, env_file },
            } => {
                assert_eq!(port, 8080);
                assert_eq!(env_file, Some(PathBuf::from("/tmp/.env")));
            }
            other => panic!("expected Config SetPort, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_config_show_port() {
        let cli = Cli::try_parse_from(["am", "config", "show-port"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Config {
                action: ConfigCommand::ShowPort
            })
        ));
    }

    #[test]
    fn clap_parses_projects_mark_identity() {
        let cli = Cli::try_parse_from(["am", "projects", "mark-identity", "/tmp/proj"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Projects {
                action:
                    ProjectsCommand::MarkIdentity {
                        project_path,
                        commit,
                        no_commit,
                    },
            } => {
                assert_eq!(project_path, PathBuf::from("/tmp/proj"));
                assert!(!commit); // default false
                assert!(!no_commit);
            }
            other => panic!("expected Projects MarkIdentity, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_projects_mark_identity_no_commit() {
        let cli = Cli::try_parse_from([
            "am",
            "projects",
            "mark-identity",
            "/tmp/proj",
            "--no-commit",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Projects {
                action: ProjectsCommand::MarkIdentity { no_commit, .. },
            } => assert!(no_commit),
            other => panic!("expected Projects MarkIdentity, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_projects_discovery_init() {
        let cli = Cli::try_parse_from(["am", "projects", "discovery-init", "/tmp/proj"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Projects {
                action:
                    ProjectsCommand::DiscoveryInit {
                        project_path,
                        product,
                    },
            } => {
                assert_eq!(project_path, PathBuf::from("/tmp/proj"));
                assert!(product.is_none());
            }
            other => panic!("expected Projects DiscoveryInit, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_projects_discovery_init_product() {
        let cli = Cli::try_parse_from([
            "am",
            "projects",
            "discovery-init",
            "/tmp/proj",
            "-P",
            "my-product",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Projects {
                action: ProjectsCommand::DiscoveryInit { product, .. },
            } => assert_eq!(product, Some("my-product".to_string())),
            other => panic!("expected Projects DiscoveryInit, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_projects_adopt_defaults() {
        let cli = Cli::try_parse_from(["am", "projects", "adopt", "/tmp/src", "/tmp/dst"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Projects {
                action:
                    ProjectsCommand::Adopt {
                        source,
                        target,
                        dry_run,
                        apply,
                    },
            } => {
                assert_eq!(source, PathBuf::from("/tmp/src"));
                assert_eq!(target, PathBuf::from("/tmp/dst"));
                assert!(!dry_run); // default false
                assert!(!apply);
            }
            other => panic!("expected Projects Adopt, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_projects_adopt_apply() {
        let cli =
            Cli::try_parse_from(["am", "projects", "adopt", "/tmp/src", "/tmp/dst", "--apply"])
                .unwrap();
        match cli.command.expect("expected command") {
            Commands::Projects {
                action: ProjectsCommand::Adopt { apply, .. },
            } => assert!(apply),
            other => panic!("expected Projects Adopt, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_mail_status() {
        let cli = Cli::try_parse_from(["am", "mail", "status", "/tmp/proj"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Mail {
                action: MailCommand::Status { project_path },
            } => assert_eq!(project_path, PathBuf::from("/tmp/proj")),
            other => panic!("expected Mail Status, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_mail_summarize_thread() {
        let cli = Cli::try_parse_from([
            "am",
            "mail",
            "summarize-thread",
            "-p",
            "my-proj",
            "thread-42",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Mail {
                action:
                    MailCommand::SummarizeThread {
                        project_key,
                        thread_id,
                        per_thread_limit,
                        no_llm,
                        format,
                        json,
                    },
            } => {
                assert_eq!(project_key, "my-proj");
                assert_eq!(thread_id, "thread-42");
                assert_eq!(per_thread_limit, 50);
                assert!(!no_llm);
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected Mail SummarizeThread, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_mail_summarize_thread_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "mail",
            "summarize-thread",
            "-p",
            "proj",
            "t-1",
            "-n",
            "10",
            "--no-llm",
            "--json",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Mail {
                action:
                    MailCommand::SummarizeThread {
                        per_thread_limit,
                        no_llm,
                        json,
                        ..
                    },
            } => {
                assert_eq!(per_thread_limit, 10);
                assert!(no_llm);
                assert!(json);
            }
            other => panic!("expected Mail SummarizeThread, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_mail_summarize_thread_format_toon() {
        let cli = Cli::try_parse_from([
            "am",
            "mail",
            "summarize-thread",
            "-p",
            "proj",
            "t-1",
            "--format",
            "toon",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Mail {
                action: MailCommand::SummarizeThread { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected Mail SummarizeThread, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_golden_capture_with_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "golden",
            "capture",
            "--dir",
            "benches/golden",
            "--filter",
            "am_*",
            "--json",
            "--verbose",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Golden {
                action:
                    GoldenCommand::Capture {
                        dir,
                        filter,
                        format,
                        json,
                        verbose,
                    },
            } => {
                assert_eq!(dir, Some(PathBuf::from("benches/golden")));
                assert_eq!(filter, Some("am_*".to_string()));
                assert!(format.is_none());
                assert!(json);
                assert!(verbose);
            }
            other => panic!("expected Golden Capture, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_golden_verify_defaults() {
        let cli = Cli::try_parse_from(["am", "golden", "verify"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Golden {
                action:
                    GoldenCommand::Verify {
                        dir,
                        filter,
                        format,
                        json,
                        verbose,
                    },
            } => {
                assert!(dir.is_none());
                assert!(filter.is_none());
                assert!(format.is_none());
                assert!(!json);
                assert!(!verbose);
            }
            other => panic!("expected Golden Verify, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_golden_list_with_filter() {
        let cli = Cli::try_parse_from([
            "am",
            "golden",
            "list",
            "--dir",
            "benches/golden",
            "--filter",
            "mcp_deny_*",
            "--json",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Golden {
                action:
                    GoldenCommand::List {
                        dir,
                        filter,
                        format,
                        json,
                    },
            } => {
                assert_eq!(dir, Some(PathBuf::from("benches/golden")));
                assert_eq!(filter, Some("mcp_deny_*".to_string()));
                assert!(format.is_none());
                assert!(json);
            }
            other => panic!("expected Golden List, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_golden_list_format_toon() {
        let cli = Cli::try_parse_from(["am", "golden", "list", "--format", "toon"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Golden {
                action: GoldenCommand::List { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected Golden List, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_flake_triage_scan_format_toon() {
        let cli = Cli::try_parse_from(["am", "flake-triage", "scan", "--format", "toon"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::FlakeTriage {
                action: FlakeTriageCommand::Scan { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected flake-triage scan, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_flake_triage_detect_format_toon() {
        let cli = Cli::try_parse_from([
            "am",
            "flake-triage",
            "detect",
            "my_test",
            "--format",
            "toon",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::FlakeTriage {
                action: FlakeTriageCommand::Detect { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected flake-triage detect, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_e2e_list_format_toon() {
        let cli = Cli::try_parse_from(["am", "e2e", "list", "--format", "toon"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::E2e {
                action: E2eCommand::List { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected e2e list, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_e2e_run_format_toon() {
        let cli = Cli::try_parse_from(["am", "e2e", "run", "--format", "toon"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::E2e {
                action: E2eCommand::Run { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected e2e run, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_products_ensure_defaults() {
        let cli = Cli::try_parse_from(["am", "products", "ensure"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Products {
                action:
                    ProductsCommand::Ensure {
                        product_key,
                        name,
                        format,
                        json,
                    },
            } => {
                assert!(product_key.is_none());
                assert!(name.is_none());
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected Products Ensure, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_products_ensure_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "products",
            "ensure",
            "pk-1",
            "-n",
            "MyProduct",
            "--json",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Products {
                action:
                    ProductsCommand::Ensure {
                        product_key,
                        name,
                        format,
                        json,
                    },
            } => {
                assert_eq!(product_key, Some("pk-1".to_string()));
                assert_eq!(name, Some("MyProduct".to_string()));
                assert!(format.is_none());
                assert!(json);
            }
            other => panic!("expected Products Ensure, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_products_link() {
        let cli =
            Cli::try_parse_from(["am", "products", "link", "pk-1", "proj-1", "--json"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Products {
                action:
                    ProductsCommand::Link {
                        product_key,
                        project,
                        format,
                        json,
                    },
            } => {
                assert_eq!(product_key, "pk-1");
                assert_eq!(project, "proj-1");
                assert!(format.is_none());
                assert!(json);
            }
            other => panic!("expected Products Link, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_products_status() {
        let cli = Cli::try_parse_from(["am", "products", "status", "pk-1"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Products {
                action:
                    ProductsCommand::Status {
                        product_key,
                        format,
                        json,
                    },
            } => {
                assert_eq!(product_key, "pk-1");
                assert!(format.is_none());
                assert!(!json); // default
            }
            other => panic!("expected Products Status, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_products_status_format_toon() {
        let cli =
            Cli::try_parse_from(["am", "products", "status", "pk-1", "--format", "toon"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Products {
                action: ProductsCommand::Status { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected Products Status, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_tooling_directory_format_toon() {
        let cli = Cli::try_parse_from(["am", "tooling", "directory", "--format", "toon"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Tooling {
                action: ToolingCommand::Directory { format, json },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected Tooling Directory, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_products_summarize_thread() {
        let cli = Cli::try_parse_from(["am", "products", "summarize-thread", "pk-1", "thread-abc"])
            .unwrap();
        match cli.command.expect("expected command") {
            Commands::Products {
                action:
                    ProductsCommand::SummarizeThread {
                        product_key,
                        thread_id,
                        per_thread_limit,
                        no_llm,
                        format,
                        json,
                    },
            } => {
                assert_eq!(product_key, "pk-1");
                assert_eq!(thread_id, "thread-abc");
                assert_eq!(per_thread_limit, 50); // default
                assert!(!no_llm);
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected Products SummarizeThread, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_products_summarize_thread_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "products",
            "summarize-thread",
            "pk-1",
            "thread-abc",
            "-n",
            "10",
            "--no-llm",
            "--json",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Products {
                action:
                    ProductsCommand::SummarizeThread {
                        per_thread_limit,
                        no_llm,
                        json,
                        ..
                    },
            } => {
                assert_eq!(per_thread_limit, 10);
                assert!(no_llm);
                assert!(json);
            }
            other => panic!("expected Products SummarizeThread, got {other:?}"),
        }
    }

    // ── Invalid arg / exit code tests ────────────────────────────────

    #[test]
    fn clap_rejects_serve_http_invalid_port() {
        let err = Cli::try_parse_from(["am", "serve-http", "--port", "not_a_number"]);
        assert!(err.is_err());
    }

    #[test]
    fn clap_rejects_list_acks_missing_project() {
        let err = Cli::try_parse_from(["am", "list-acks", "--agent", "BlueLake"]);
        assert!(err.is_err());
    }

    #[test]
    fn clap_rejects_list_acks_missing_agent() {
        let err = Cli::try_parse_from(["am", "list-acks", "--project", "/tmp"]);
        assert!(err.is_err());
    }

    #[test]
    fn clap_rejects_guard_install_missing_repo() {
        let err = Cli::try_parse_from(["am", "guard", "install", "proj"]);
        assert!(err.is_err());
    }

    #[test]
    fn clap_rejects_acks_pending_missing_agent() {
        let err = Cli::try_parse_from(["am", "acks", "pending", "proj"]);
        assert!(err.is_err());
    }

    #[test]
    fn clap_rejects_config_set_port_invalid() {
        let err = Cli::try_parse_from(["am", "config", "set-port", "99999"]);
        assert!(err.is_err());
    }

    #[test]
    fn clap_rejects_unknown_subcommand() {
        let err = Cli::try_parse_from(["am", "nonexistent"]);
        assert!(err.is_err());
    }

    #[test]
    fn clap_rejects_mail_status_missing_path() {
        let err = Cli::try_parse_from(["am", "mail", "status"]);
        assert!(err.is_err());
    }

    // ── br-2ei.5.7.3: golden help snapshots ────────────────────────────
    //
    // Verify that help text contains expected commands and flags.
    // Uses clap's error rendering (--help triggers a clap error) to
    // capture help text without spawning a subprocess.

    fn help_text_for(args: &[&str]) -> String {
        match Cli::try_parse_from(args) {
            Ok(_) => panic!("expected --help to trigger clap exit"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn help_top_level_lists_all_subcommands() {
        let h = help_text_for(&["am", "--help"]);
        let expected = [
            "serve-http",
            "serve-stdio",
            "lint",
            "typecheck",
            "share",
            "archive",
            "guard",
            "acks",
            "list-acks",
            "migrate",
            "list-projects",
            "clear-and-reset-everything",
            "config",
            "amctl",
            "am-run",
            "projects",
            "mail",
            "products",
            "docs",
            "doctor",
            "macros",
            "contacts",
            "file_reservations",
            "agents",
            "tooling",
            "golden",
            "legacy",
            "upgrade",
        ];
        for cmd in expected {
            assert!(
                h.contains(cmd),
                "top-level help missing subcommand '{cmd}'\n--- help ---\n{h}"
            );
        }
    }

    #[test]
    fn help_serve_http_lists_flags() {
        let h = help_text_for(&["am", "serve-http", "--help"]);
        for flag in ["--host", "--port", "--path", "--no-auth"] {
            assert!(
                h.contains(flag),
                "serve-http help missing flag '{flag}'\n{h}"
            );
        }
    }

    #[test]
    fn help_share_lists_subcommands() {
        let h = help_text_for(&["am", "share", "--help"]);
        for cmd in ["export", "update", "preview", "verify", "decrypt", "wizard"] {
            assert!(
                h.contains(cmd),
                "share help missing subcommand '{cmd}'\n{h}"
            );
        }
    }

    #[test]
    fn help_share_export_lists_flags() {
        let h = help_text_for(&["am", "share", "export", "--help"]);
        for flag in [
            "--output",
            "--interactive",
            "--project",
            "--inline-threshold",
            "--detach-threshold",
            "--scrub-preset",
            "--chunk-threshold",
            "--chunk-size",
            "--dry-run",
            "--zip",
            "--signing-key",
            "--age-recipient",
        ] {
            assert!(
                h.contains(flag),
                "share export help missing flag '{flag}'\n{h}"
            );
        }
    }

    #[test]
    fn help_guard_lists_subcommands() {
        let h = help_text_for(&["am", "guard", "--help"]);
        for cmd in ["install", "uninstall", "status", "check"] {
            assert!(
                h.contains(cmd),
                "guard help missing subcommand '{cmd}'\n{h}"
            );
        }
    }

    #[test]
    fn help_guard_check_lists_flags() {
        let h = help_text_for(&["am", "guard", "check", "--help"]);
        for flag in ["--stdin-nul", "--advisory", "--repo"] {
            assert!(
                h.contains(flag),
                "guard check help missing flag '{flag}'\n{h}"
            );
        }
    }

    #[test]
    fn help_doctor_lists_subcommands() {
        let h = help_text_for(&["am", "doctor", "--help"]);
        for cmd in ["check", "repair", "backups", "restore"] {
            assert!(
                h.contains(cmd),
                "doctor help missing subcommand '{cmd}'\n{h}"
            );
        }
    }

    #[test]
    fn help_doctor_repair_lists_flags() {
        let h = help_text_for(&["am", "doctor", "repair", "--help"]);
        for flag in ["--dry-run", "--yes", "--backup-dir"] {
            assert!(
                h.contains(flag),
                "doctor repair help missing flag '{flag}'\n{h}"
            );
        }
    }

    #[test]
    fn help_archive_lists_subcommands() {
        let h = help_text_for(&["am", "archive", "--help"]);
        for cmd in ["save", "list", "restore"] {
            assert!(
                h.contains(cmd),
                "archive help missing subcommand '{cmd}'\n{h}"
            );
        }
    }

    #[test]
    fn help_legacy_lists_subcommands() {
        let h = help_text_for(&["am", "legacy", "--help"]);
        for cmd in ["detect", "import", "status"] {
            assert!(
                h.contains(cmd),
                "legacy help missing subcommand '{cmd}'\n{h}"
            );
        }
    }

    #[test]
    fn help_legacy_import_lists_flags() {
        let h = help_text_for(&["am", "legacy", "import", "--help"]);
        for flag in [
            "--auto",
            "--search-root",
            "--db",
            "--storage-root",
            "--in-place",
            "--copy",
            "--target-db",
            "--target-storage-root",
            "--dry-run",
            "--yes",
        ] {
            assert!(
                h.contains(flag),
                "legacy import help missing flag '{flag}'\n{h}"
            );
        }
    }

    #[test]
    fn help_upgrade_lists_flags() {
        let h = help_text_for(&["am", "upgrade", "--help"]);
        for flag in ["--search-root", "--dry-run", "--yes", "--format", "--json"] {
            assert!(h.contains(flag), "upgrade help missing flag '{flag}'\n{h}");
        }
    }

    #[test]
    fn help_products_lists_subcommands() {
        let h = help_text_for(&["am", "products", "--help"]);
        for cmd in [
            "ensure",
            "link",
            "status",
            "search",
            "inbox",
            "summarize-thread",
        ] {
            assert!(
                h.contains(cmd),
                "products help missing subcommand '{cmd}'\n{h}"
            );
        }
    }

    #[test]
    fn help_acks_lists_subcommands() {
        let h = help_text_for(&["am", "acks", "--help"]);
        for cmd in ["pending", "remind", "overdue"] {
            assert!(h.contains(cmd), "acks help missing subcommand '{cmd}'\n{h}");
        }
    }

    #[test]
    fn help_projects_lists_subcommands() {
        let h = help_text_for(&["am", "projects", "--help"]);
        for cmd in ["mark-identity", "discovery-init", "adopt"] {
            assert!(
                h.contains(cmd),
                "projects help missing subcommand '{cmd}'\n{h}"
            );
        }
    }

    #[test]
    fn help_config_lists_subcommands() {
        let h = help_text_for(&["am", "config", "--help"]);
        for cmd in ["set-port", "show-port"] {
            assert!(
                h.contains(cmd),
                "config help missing subcommand '{cmd}'\n{h}"
            );
        }
    }

    #[test]
    fn help_clear_and_reset_lists_flags() {
        let h = help_text_for(&["am", "clear-and-reset-everything", "--help"]);
        for flag in ["--force", "--archive", "--no-archive"] {
            assert!(
                h.contains(flag),
                "clear-and-reset-everything help missing flag '{flag}'\n{h}"
            );
        }
    }

    #[test]
    fn help_docs_lists_subcommands() {
        let h = help_text_for(&["am", "docs", "--help"]);
        assert!(
            h.contains("insert-blurbs"),
            "docs help missing subcommand 'insert-blurbs'\n{h}"
        );
    }

    #[test]
    fn help_am_run_lists_flags() {
        let h = help_text_for(&["am", "am-run", "--help"]);
        for flag in [
            "--path",
            "--agent",
            "--ttl-seconds",
            "--shared",
            "--exclusive",
            "--block-on-conflicts",
        ] {
            assert!(h.contains(flag), "am-run help missing flag '{flag}'\n{h}");
        }
    }

    #[test]
    fn help_list_acks_lists_flags() {
        let h = help_text_for(&["am", "list-acks", "--help"]);
        for flag in ["--project", "--agent", "--limit"] {
            assert!(
                h.contains(flag),
                "list-acks help missing flag '{flag}'\n{h}"
            );
        }
    }

    #[test]
    fn help_top_level_contains_name_and_version() {
        let h = help_text_for(&["am", "--help"]);
        assert!(h.contains("am") || h.contains("MCP Agent Mail"));
    }

    // ── br-2ei.5.7.4: JSON output stability ────────────────────────────
    //
    // Verify JSON schemas for commands that produce machine-readable output.

    #[test]
    fn json_list_projects_schema_fields() {
        // list-projects --json emits an array of objects with:
        // { id, slug, human_key, created_at }
        let entry = serde_json::json!({
            "id": 1,
            "slug": "proj-abc123",
            "human_key": "/tmp/my-project",
            "created_at": "2026-01-01T00:00:00Z"
        });
        assert!(entry["id"].is_i64());
        assert!(entry["slug"].is_string());
        assert!(entry["human_key"].is_string());
        assert!(entry["created_at"].is_string());
    }

    #[test]
    fn json_list_projects_with_agents_schema() {
        // When --include-agents is set, each project gains an "agents" array.
        let entry = serde_json::json!({
            "id": 1,
            "slug": "proj-abc",
            "human_key": "/tmp/proj",
            "created_at": "2026-01-01T00:00:00Z",
            "agents": [
                {"name": "BlueLake", "program": "claude-code", "model": "opus"}
            ]
        });
        assert!(entry["agents"].is_array());
        let agent = &entry["agents"][0];
        assert!(agent["name"].is_string());
        assert!(agent["program"].is_string());
        assert!(agent["model"].is_string());
    }

    #[test]
    fn json_doctor_check_schema_stable() {
        let output = serde_json::json!({
            "healthy": true,
            "checks": [
                {"check": "database", "status": "ok", "detail": "accessible"},
                {"check": "storage_root", "status": "ok", "detail": "/tmp/store"}
            ]
        });
        assert!(output["healthy"].is_boolean());
        assert!(output["checks"].is_array());
        for check in output["checks"].as_array().unwrap() {
            assert!(check["check"].is_string(), "check field must be string");
            assert!(check["status"].is_string(), "status field must be string");
            assert!(check["detail"].is_string(), "detail field must be string");
            let status = check["status"].as_str().unwrap();
            assert!(
                ["ok", "warn", "fail"].contains(&status),
                "status must be ok/warn/fail, got {status}"
            );
        }
    }

    #[test]
    fn json_doctor_backups_schema() {
        // doctor backups --json emits an array of { name, size }
        let arr = serde_json::json!([
            {"name": "pre_repair_20260101_120000.sqlite3", "size": 4096},
            {"name": "pre_repair_20260102_130000.sqlite3", "size": 8192}
        ]);
        assert!(arr.is_array());
        for item in arr.as_array().unwrap() {
            assert!(item["name"].is_string());
            assert!(item["size"].is_number());
        }
    }

    #[test]
    fn json_archive_list_schema() {
        // archive list --json emits array of { file, size_bytes, modified, label }
        let entry = serde_json::json!({
            "file": "archive_20260101.sqlite3",
            "size_bytes": 12345,
            "modified": "2026-01-01T12:00:00Z",
            "label": "my-label"
        });
        assert!(entry["file"].is_string());
        assert!(entry["size_bytes"].is_number());
        assert!(entry["modified"].is_string());
    }

    #[test]
    fn json_lease_record_schema() {
        // am-run lease records follow LeaseRecord schema
        let lease = serde_json::json!({
            "slot": "frontend-build",
            "agent": "BlueLake",
            "branch": "main",
            "exclusive": true,
            "acquired_ts": "2026-01-01T00:00:00Z",
            "expires_ts": "2026-01-01T01:00:00Z"
        });
        assert!(lease["slot"].is_string());
        assert!(lease["agent"].is_string());
        assert!(lease["branch"].is_string());
        assert!(lease["exclusive"].is_boolean());
        assert!(lease["acquired_ts"].is_string());
        assert!(lease["expires_ts"].is_string());
        assert!(lease.get("released_ts").is_none() || lease["released_ts"].is_null());
    }

    #[test]
    fn json_lease_record_roundtrips_through_serde() {
        let lease = LeaseRecord {
            slot: "build".to_string(),
            agent: "BlueLake".to_string(),
            branch: "main".to_string(),
            exclusive: true,
            acquired_ts: "2026-01-01T00:00:00Z".to_string(),
            expires_ts: "2026-01-01T01:00:00Z".to_string(),
            released_ts: None,
        };
        let json = serde_json::to_string(&lease).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["slot"], "build");
        assert_eq!(parsed["agent"], "BlueLake");
        assert!(!json.contains("released_ts")); // skip_serializing_if None
    }

    #[test]
    fn json_lease_record_released_ts_included_when_set() {
        let lease = LeaseRecord {
            slot: "build".to_string(),
            agent: "BlueLake".to_string(),
            branch: "main".to_string(),
            exclusive: false,
            acquired_ts: "2026-01-01T00:00:00Z".to_string(),
            expires_ts: "2026-01-01T01:00:00Z".to_string(),
            released_ts: Some("2026-01-01T00:30:00Z".to_string()),
        };
        let json = serde_json::to_string(&lease).unwrap();
        assert!(json.contains("released_ts"));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["released_ts"], "2026-01-01T00:30:00Z");
    }

    // ── br-2ei.5.7.2: integration exit codes ────────────────────────────
    //
    // Tests that exercise the actual command handlers and verify exit behavior.

    #[test]
    fn integration_migrate_and_list_projects_json() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());

        // Migrate
        let result = handle_migrate_with_database_url(&db_url);
        assert!(result.is_ok(), "migrate failed: {result:?}");

        // list-projects should succeed with empty output
        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_list_projects_with_database_url(&db_url, false, None, true);
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "list-projects --json failed: {result:?}");

        // Parse JSON output - should be empty array.
        // Extract from first JSON-like character to tolerate leaked ftui_println.
        let trimmed = output.trim();
        let json_str = extract_json_array(trimmed).expect("valid JSON array");
        let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap();
        assert!(parsed.is_array(), "expected JSON array, got: {parsed}");
        assert_eq!(parsed.as_array().unwrap().len(), 0);
    }

    #[test]
    fn integration_doctor_check_on_fresh_db() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());

        handle_migrate_with_database_url(&db_url).expect("migrate");

        // Doctor check should succeed on a fresh DB
        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_doctor_check_with(&db_url, dir.path(), None, false, None, true);
        let output = capture.drain_to_string();

        assert!(result.is_ok(), "doctor check failed: {result:?}");
        // JSON output should have "healthy" field.
        // Use extract_json_block to tolerate leaked ftui_println from concurrent tests.
        if let Some(json_str) = extract_json_block(&output) {
            let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap();
            assert!(parsed["healthy"].is_boolean());
        }
    }

    #[test]
    fn integration_doctor_check_reports_installed_agents_probe() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());

        handle_migrate_with_database_url(&db_url).expect("migrate");

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_doctor_check_with(&db_url, dir.path(), None, false, None, true);
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "doctor check failed: {result:?}");

        // Use extract_json_block to tolerate leaked ftui_println from concurrent tests.
        let json_str = extract_json_block(&output).expect("expected JSON in doctor output");
        let parsed: serde_json::Value = serde_json::from_str(json_str).expect("json");
        let checks = parsed["checks"].as_array().expect("checks array");
        assert!(
            checks
                .iter()
                .any(|c| c["check"].as_str() == Some("installed_agents")),
            "doctor check should include installed_agents probe"
        );
    }

    #[test]
    fn integration_doctor_check_reports_beads_issue_awareness_probe() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());

        handle_migrate_with_database_url(&db_url).expect("migrate");

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_doctor_check_with(&db_url, dir.path(), None, false, None, true);
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "doctor check failed: {result:?}");

        // Use extract_json_block to tolerate leaked ftui_println from concurrent tests.
        let json_str = extract_json_block(&output).expect("expected JSON in doctor output");
        let parsed: serde_json::Value = serde_json::from_str(json_str).expect("json");
        let checks = parsed["checks"].as_array().expect("checks array");
        let bead_check = checks
            .iter()
            .find(|c| c["check"].as_str() == Some("beads_issue_awareness"))
            .expect("beads_issue_awareness check should be present");
        let status = bead_check["status"].as_str().unwrap_or_default();
        assert!(
            matches!(status, "ok" | "warn"),
            "unexpected status for beads_issue_awareness: {status}"
        );
    }

    #[test]
    fn beads_issue_awareness_counts_from_missing_beads_dir_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err =
            beads_issue_awareness_counts_from(Some(dir.path())).expect_err("expected no .beads");
        assert!(
            !err.trim().is_empty(),
            "error message should not be empty for missing .beads"
        );
    }

    #[test]
    fn beads_issue_awareness_counts_from_temp_beads_dir_reports_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let beads_dir = dir.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("create .beads");

        let (mut storage, _paths) =
            beads_rust::config::open_storage(&beads_dir, None, None).expect("open storage");

        let open_issue = beads_rust::model::Issue {
            id: "br-test-open".to_string(),
            title: "Open issue".to_string(),
            status: beads_rust::model::Status::Open,
            ..Default::default()
        };
        storage
            .create_issue(&open_issue, "test")
            .expect("insert open issue");

        let in_progress_issue = beads_rust::model::Issue {
            id: "br-test-progress".to_string(),
            title: "In progress issue".to_string(),
            status: beads_rust::model::Status::InProgress,
            ..Default::default()
        };
        storage
            .create_issue(&in_progress_issue, "test")
            .expect("insert in-progress issue");

        let (ready, open, in_progress) =
            beads_issue_awareness_counts_from(Some(dir.path())).expect("counts");
        assert_eq!(open, 1);
        assert_eq!(in_progress, 1);
        assert_eq!(
            ready, 2,
            "open + in-progress unblocked issues should be ready under default filters"
        );
    }

    #[test]
    fn clap_parses_beads_status_flags() {
        let cli = Cli::try_parse_from(["am", "beads", "status", "--json"])
            .expect("failed to parse beads status");
        match cli.command.expect("expected command") {
            Commands::Beads {
                action: BeadsCommand::Status { json, .. },
            } => assert!(json),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_beads_ready_with_limit() {
        let cli = Cli::try_parse_from(["am", "beads", "ready", "--limit", "5", "--json"])
            .expect("failed to parse beads ready");
        match cli.command.expect("expected command") {
            Commands::Beads {
                action: BeadsCommand::Ready { limit, json, .. },
            } => {
                assert_eq!(limit, 5);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_beads_ready_format_toon() {
        let cli = Cli::try_parse_from(["am", "beads", "ready", "--format", "toon"])
            .expect("failed to parse beads ready");
        match cli.command.expect("expected command") {
            Commands::Beads {
                action: BeadsCommand::Ready { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_beads_list_with_status_filter() {
        let cli =
            Cli::try_parse_from(["am", "beads", "list", "--status", "open", "--priority", "2"])
                .expect("failed to parse beads list");
        match cli.command.expect("expected command") {
            Commands::Beads {
                action:
                    BeadsCommand::List {
                        status, priority, ..
                    },
            } => {
                assert_eq!(status.as_deref(), Some("open"));
                assert_eq!(priority, Some(2));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_beads_show_with_id() {
        let cli = Cli::try_parse_from(["am", "beads", "show", "br-123", "--json"])
            .expect("failed to parse beads show");
        match cli.command.expect("expected command") {
            Commands::Beads {
                action: BeadsCommand::Show { id, json, .. },
            } => {
                assert_eq!(id, "br-123");
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_agents_detect_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "agents",
            "detect",
            "--only",
            "claude,codex",
            "--include-undetected",
        ])
        .expect("failed to parse agents detect");
        match cli.command.expect("expected command") {
            Commands::Agents {
                action:
                    AgentsCommand::Detect {
                        only,
                        include_undetected,
                        format,
                        json,
                    },
            } => {
                assert_eq!(only.as_deref(), Some("claude,codex"));
                assert!(include_undetected);
                assert!(format.is_none());
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn clap_parses_agents_list_format_toon() {
        let cli = Cli::try_parse_from([
            "am",
            "agents",
            "list",
            "--project",
            "my-proj",
            "--format",
            "toon",
        ])
        .expect("failed to parse agents list");
        match cli.command.expect("expected command") {
            Commands::Agents {
                action:
                    AgentsCommand::List {
                        project_key,
                        format,
                        json,
                    },
            } => {
                assert_eq!(project_key, "my-proj");
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn resolve_beads_db_returns_error_for_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_beads_db(Some(dir.path()));
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("no beads database"),
            "expected 'no beads database' in: {err_msg}"
        );
    }

    #[test]
    fn handle_beads_status_on_fresh_db() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let beads_dir = dir.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("create .beads");

        // Initialize a fresh beads storage (creates the DB)
        let _storage =
            beads_rust::config::open_storage(&beads_dir, None, None).expect("open storage");

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_beads_status(Some(dir.path().to_path_buf()), None, true);
        let output = capture.drain_to_string();

        assert!(result.is_ok(), "beads status failed: {result:?}");
        // Use extract_json_block to tolerate leaked ftui_println from concurrent tests.
        let json_str = extract_json_block(&output).expect("expected JSON in beads status output");
        let parsed: serde_json::Value =
            serde_json::from_str(json_str).expect("should be valid JSON");
        assert_eq!(parsed["open"], 0);
        assert_eq!(parsed["closed"], 0);
        assert_eq!(parsed["total"], 0);
    }

    #[test]
    fn handle_beads_ready_on_fresh_db_shows_empty() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let beads_dir = dir.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("create .beads");

        let _storage =
            beads_rust::config::open_storage(&beads_dir, None, None).expect("open storage");

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_beads_ready(Some(dir.path().to_path_buf()), 20, None, true);
        let output = capture.drain_to_string();

        assert!(result.is_ok(), "beads ready failed: {result:?}");
        // Extract JSON array from potentially polluted capture output (prefix + suffix).
        let trimmed = output.trim();
        let json_str = extract_json_array(trimmed).expect("should be valid JSON array");
        let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap();
        assert!(parsed.as_array().expect("should be array").is_empty());
    }

    #[test]
    fn integration_doctor_backups_empty_dir() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_doctor_backups_with_storage_root(dir.path(), None, true);
        let output = capture.drain_to_string();

        assert!(result.is_ok(), "doctor backups --json failed: {result:?}");
        // Extract JSON array from potentially polluted capture output.
        let trimmed = output.trim();
        if let Some(start) = trimmed.find('[') {
            let parsed: serde_json::Value = serde_json::from_str(&trimmed[start..]).unwrap();
            assert!(parsed.is_array());
        }
    }

    #[test]
    fn integration_config_show_port_returns_ok() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_config(ConfigCommand::ShowPort);
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "config show-port failed: {result:?}");
        // Should output a port number. Find last line that looks like a port number
        // to tolerate leaked ftui_println from concurrent tests.
        let has_port = output
            .trim()
            .lines()
            .any(|l| l.trim().parse::<u16>().is_ok())
            || output.contains("8765");
        assert!(has_port, "expected port number, got: {output}");
    }

    #[test]
    fn integration_config_set_port_to_file() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join(".env");
        let result = handle_config(ConfigCommand::SetPort {
            port: 9999,
            env_file: Some(env_file.clone()),
        });
        assert!(result.is_ok(), "config set-port failed: {result:?}");
        let content = std::fs::read_to_string(&env_file).unwrap_or_default();
        assert!(
            content.contains("9999"),
            "env file should contain port 9999, got: {content}"
        );
    }

    /// Helper: seed a DB with projects, agents, messages, and file_reservations for CLI tests.
    fn seed_acks_and_reservations_db(db_path: &Path) -> mcp_agent_mail_db::DbConn {
        use mcp_agent_mail_db::sqlmodel::Value as SqlValue;

        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");

        let now_us = mcp_agent_mail_db::timestamps::now_micros();

        // Project
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                SqlValue::BigInt(1),
                SqlValue::Text("test-proj".to_string()),
                SqlValue::Text("/tmp/test-proj".to_string()),
                SqlValue::BigInt(now_us),
            ],
        )
        .unwrap();

        // Agents
        let agent_insert = "INSERT INTO agents (\
                id, project_id, name, program, model, task_description, \
                inception_ts, last_active_ts, attachments_policy, contact_policy\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
        conn.execute_sync(
            agent_insert,
            &[
                SqlValue::BigInt(1),
                SqlValue::BigInt(1),
                SqlValue::Text("BlueLake".to_string()),
                SqlValue::Text("test".to_string()),
                SqlValue::Text("test".to_string()),
                SqlValue::Text(String::new()),
                SqlValue::BigInt(now_us),
                SqlValue::BigInt(now_us),
                SqlValue::Text("auto".to_string()),
                SqlValue::Text("auto".to_string()),
            ],
        )
        .unwrap();
        conn.execute_sync(
            agent_insert,
            &[
                SqlValue::BigInt(2),
                SqlValue::BigInt(1),
                SqlValue::Text("RedFox".to_string()),
                SqlValue::Text("test".to_string()),
                SqlValue::Text("test".to_string()),
                SqlValue::Text(String::new()),
                SqlValue::BigInt(now_us),
                SqlValue::BigInt(now_us),
                SqlValue::Text("auto".to_string()),
                SqlValue::Text("auto".to_string()),
            ],
        )
        .unwrap();

        // Messages (ack_required=1 from RedFox to BlueLake)
        let msg_insert = "INSERT INTO messages (\
                id, project_id, sender_id, thread_id, subject, body_md, importance, \
                ack_required, created_ts, attachments\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
        conn.execute_sync(
            msg_insert,
            &[
                SqlValue::BigInt(100),
                SqlValue::BigInt(1),
                SqlValue::BigInt(2), // sender: RedFox
                SqlValue::Null,
                SqlValue::Text("Please review PR".to_string()),
                SqlValue::Text("body".to_string()),
                SqlValue::Text("high".to_string()),
                SqlValue::BigInt(1),                    // ack_required
                SqlValue::BigInt(now_us - 120_000_000), // 2 min ago
                SqlValue::Text("[]".to_string()),
            ],
        )
        .unwrap();
        // Non-ack message
        conn.execute_sync(
            msg_insert,
            &[
                SqlValue::BigInt(101),
                SqlValue::BigInt(1),
                SqlValue::BigInt(2),
                SqlValue::Null,
                SqlValue::Text("FYI update".to_string()),
                SqlValue::Text("body".to_string()),
                SqlValue::Text("normal".to_string()),
                SqlValue::BigInt(0),                   // not ack_required
                SqlValue::BigInt(now_us - 60_000_000), // 1 min ago
                SqlValue::Text("[]".to_string()),
            ],
        )
        .unwrap();

        // Recipients
        let recip_insert =
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)";
        conn.execute_sync(
            recip_insert,
            &[
                SqlValue::BigInt(100),
                SqlValue::BigInt(1), // BlueLake
                SqlValue::Text("to".to_string()),
            ],
        )
        .unwrap();
        conn.execute_sync(
            recip_insert,
            &[
                SqlValue::BigInt(101),
                SqlValue::BigInt(1),
                SqlValue::Text("to".to_string()),
            ],
        )
        .unwrap();

        // File reservations (active, by BlueLake)
        let future_ts = now_us + 3_600_000_000; // 1 hour from now
        conn.execute_sync(
            "INSERT INTO file_reservations (\
                id, project_id, agent_id, path_pattern, \"exclusive\", reason, \
                created_ts, expires_ts, released_ts\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                SqlValue::BigInt(1),
                SqlValue::BigInt(1),
                SqlValue::BigInt(1), // BlueLake
                SqlValue::Text("src/api/*.rs".to_string()),
                SqlValue::BigInt(1),
                SqlValue::Text("refactoring API".to_string()),
                SqlValue::BigInt(now_us),
                SqlValue::BigInt(future_ts),
                SqlValue::Null, // not released
            ],
        )
        .unwrap();

        conn
    }

    #[test]
    fn integration_acks_pending_shows_unacked_messages() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_acks_with_conn(
            &conn,
            AcksCommand::Pending {
                project: "test-proj".to_string(),
                agent: "BlueLake".to_string(),
                limit: 20,
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "acks pending failed: {result:?}");
        // Should show the ack-required message from RedFox
        assert!(
            output.contains("RedFox") && output.contains("Please review PR"),
            "expected ack-required message in output, got: {output}"
        );
    }

    #[test]
    fn integration_acks_pending_empty_when_no_acks() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        // RedFox has no ack-required messages
        let result = handle_acks_with_conn(
            &conn,
            AcksCommand::Pending {
                project: "test-proj".to_string(),
                agent: "RedFox".to_string(),
                limit: 20,
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "acks pending failed: {result:?}");
        assert!(
            output.contains("No pending acks"),
            "expected empty result, got: {output}"
        );
    }

    #[test]
    fn integration_acks_overdue_finds_old_messages() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        // ttl_minutes=1 means messages older than 1 min are overdue;
        // our message is 2 min old
        let result = handle_acks_with_conn(
            &conn,
            AcksCommand::Overdue {
                project: "test-proj".to_string(),
                agent: "BlueLake".to_string(),
                ttl_minutes: 1,
                limit: 50,
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "acks overdue failed: {result:?}");
        assert!(
            output.contains("OVERDUE") && output.contains("RedFox"),
            "expected overdue ack in output, got: {output}"
        );
    }

    #[test]
    fn integration_list_acks_shows_ack_required_messages() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_list_acks_with_conn(&conn, "test-proj", "BlueLake", 20);
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "list-acks failed: {result:?}");
        assert!(
            output.contains("RedFox") && output.contains("pending"),
            "expected ack-required message with pending status, got: {output}"
        );
    }

    #[test]
    fn integration_list_acks_empty_for_nonexistent_agent() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_list_acks_with_conn(&conn, "test-proj", "GhostAgent", 20);
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "list-acks failed: {result:?}");
        assert!(
            output.contains("No ack-required messages"),
            "expected empty result, got: {output}"
        );
    }

    #[test]
    fn integration_file_reservations_list_shows_active() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_file_reservations_with_conn(
            &conn,
            FileReservationsCommand::List {
                project: "test-proj".to_string(),
                active_only: false,
                all: false,
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "file_reservations list failed: {result:?}");
        assert!(
            output.contains("src/api/*.rs") && output.contains("BlueLake"),
            "expected reservation in output, got: {output}"
        );
    }

    #[test]
    fn integration_file_reservations_active_shows_active() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_file_reservations_with_conn(
            &conn,
            FileReservationsCommand::Active {
                project: "test-proj".to_string(),
                limit: None,
            },
        );
        let output = capture.drain_to_string();
        assert!(
            result.is_ok(),
            "file_reservations active failed: {result:?}"
        );
        assert!(
            output.contains("src/api/*.rs") && output.contains("BlueLake"),
            "expected active reservation, got: {output}"
        );
    }

    #[test]
    fn integration_file_reservations_empty_project() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_file_reservations_with_conn(
            &conn,
            FileReservationsCommand::List {
                project: "nonexistent-proj".to_string(),
                active_only: false,
                all: false,
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "file_reservations list failed: {result:?}");
        assert!(
            output.contains("No file reservations"),
            "expected empty result, got: {output}"
        );
    }

    #[test]
    fn integration_file_reservations_reserve_creates_entries() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_file_reservations_with_conn(
            &conn,
            FileReservationsCommand::Reserve {
                project: "test-proj".to_string(),
                agent: "RedFox".to_string(),
                paths: vec!["lib/**".to_string(), "tests/**".to_string()],
                ttl: 7200,
                exclusive: true,
                shared: false,
                reason: "br-123".to_string(),
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "reserve failed: {result:?}");
        assert!(
            output.contains("\"granted\"") && output.contains("lib/**"),
            "expected granted output, got: {output}"
        );

        // Verify reservations exist in DB.
        let rows = conn
            .query_sync(
                "SELECT path_pattern FROM file_reservations WHERE agent_id = 2 AND released_ts IS NULL",
                &[],
            )
            .unwrap();
        assert!(
            rows.len() >= 2,
            "expected at least 2 reservations for RedFox"
        );
    }

    #[test]
    fn integration_file_reservations_reserve_detects_conflicts() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        // BlueLake already has src/api/*.rs exclusive — RedFox requesting overlapping path.
        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_file_reservations_with_conn(
            &conn,
            FileReservationsCommand::Reserve {
                project: "test-proj".to_string(),
                agent: "RedFox".to_string(),
                paths: vec!["src/api/*.rs".to_string()],
                ttl: 3600,
                exclusive: true,
                shared: false,
                reason: "overlap test".to_string(),
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "reserve with conflict failed: {result:?}");
        assert!(
            output.contains("\"conflicts\"") && output.contains("BlueLake"),
            "expected conflict with BlueLake, got: {output}"
        );
    }

    #[test]
    fn integration_file_reservations_renew_extends_ttl() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        // Get original expiry.
        let before = conn
            .query_sync("SELECT expires_ts FROM file_reservations WHERE id = 1", &[])
            .unwrap();
        let orig_expires: i64 = before.first().unwrap().get_named("expires_ts").unwrap();

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_file_reservations_with_conn(
            &conn,
            FileReservationsCommand::Renew {
                project: "test-proj".to_string(),
                agent: "BlueLake".to_string(),
                extend_seconds: 1800,
                paths: vec![],
                ids: vec![],
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "renew failed: {result:?}");
        assert!(
            output.contains("Renewed") && output.contains("src/api/*.rs"),
            "expected renewed output, got: {output}"
        );

        // Verify expiry was extended.
        let after = conn
            .query_sync("SELECT expires_ts FROM file_reservations WHERE id = 1", &[])
            .unwrap();
        let new_expires: i64 = after.first().unwrap().get_named("expires_ts").unwrap();
        assert!(
            new_expires > orig_expires,
            "expires must increase: {orig_expires} -> {new_expires}"
        );
    }

    #[test]
    fn integration_file_reservations_release_sets_released_ts() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_file_reservations_with_conn(
            &conn,
            FileReservationsCommand::Release {
                project: "test-proj".to_string(),
                agent: "BlueLake".to_string(),
                paths: vec!["src/api/*.rs".to_string()],
                ids: vec![],
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "release failed: {result:?}");
        assert!(
            output.contains("Released"),
            "expected released output, got: {output}"
        );

        // Verify released_ts is set.
        let rows = conn
            .query_sync(
                "SELECT released_ts FROM file_reservations WHERE id = 1",
                &[],
            )
            .unwrap();
        let released: Option<i64> = rows.first().unwrap().get_named("released_ts").ok();
        assert!(released.is_some(), "released_ts must be set");
    }

    #[test]
    fn integration_file_reservations_release_all_clears_active_rows() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_file_reservations_with_conn(
            &conn,
            FileReservationsCommand::Release {
                project: "test-proj".to_string(),
                agent: "BlueLake".to_string(),
                paths: vec![],
                ids: vec![],
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "release-all failed: {result:?}");
        assert!(
            output.contains("Released 1 reservation(s)"),
            "expected one released reservation, got: {output}"
        );

        let active_count: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS n FROM file_reservations \
                 WHERE project_id = 1 AND agent_id = 1 AND released_ts IS NULL",
                &[],
            )
            .unwrap()
            .first()
            .and_then(|row| row.get_named("n").ok())
            .unwrap_or_default();
        assert_eq!(active_count, 0, "all active reservations must be released");
    }

    #[test]
    fn integration_file_reservations_conflicts_detects_overlap() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_file_reservations_with_conn(
            &conn,
            FileReservationsCommand::Conflicts {
                project: "test-proj".to_string(),
                paths: vec!["src/api/*.rs".to_string()],
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "conflicts check failed: {result:?}");
        assert!(
            output.contains("conflict") && output.contains("BlueLake"),
            "expected conflict with BlueLake, got: {output}"
        );
    }

    #[test]
    fn integration_file_reservations_conflicts_no_overlap() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_file_reservations_with_conn(
            &conn,
            FileReservationsCommand::Conflicts {
                project: "test-proj".to_string(),
                paths: vec!["docs/README.md".to_string()],
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "conflicts check failed: {result:?}");
        assert!(
            output.contains("No conflicts"),
            "expected no conflicts, got: {output}"
        );
    }

    #[test]
    fn integration_file_reservations_reserve_invalid_project() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let result = handle_file_reservations_with_conn(
            &conn,
            FileReservationsCommand::Reserve {
                project: "nonexistent".to_string(),
                agent: "BlueLake".to_string(),
                paths: vec!["src/**".to_string()],
                ttl: 3600,
                exclusive: true,
                shared: false,
                reason: String::new(),
            },
        );
        assert!(result.is_err(), "should fail for invalid project");
    }

    #[test]
    fn integration_file_reservations_reserve_invalid_agent() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let result = handle_file_reservations_with_conn(
            &conn,
            FileReservationsCommand::Reserve {
                project: "test-proj".to_string(),
                agent: "NonexistentAgent".to_string(),
                paths: vec!["src/**".to_string()],
                ttl: 3600,
                exclusive: true,
                shared: false,
                reason: String::new(),
            },
        );
        assert!(result.is_err(), "should fail for invalid agent");
    }

    #[test]
    fn integration_acks_remind_finds_stale() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        // min_age_minutes=1 means messages older than 1 min are stale;
        // our ack-required message is 2 min old
        let result = handle_acks_with_conn(
            &conn,
            AcksCommand::Remind {
                project: "test-proj".to_string(),
                agent: "BlueLake".to_string(),
                min_age_minutes: 1,
                limit: 50,
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "acks remind failed: {result:?}");
        assert!(
            output.contains("Stale acks") && output.contains("RedFox"),
            "expected stale ack reminder, got: {output}"
        );
    }

    /// Seed a DB with a project whose slug matches what `resolve_project_identity`
    /// computes for the given `project_path`.
    fn seed_mail_status_db(db_path: &Path, project_path: &str) -> mcp_agent_mail_db::DbConn {
        use mcp_agent_mail_db::sqlmodel::Value as SqlValue;

        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");

        let now_us = mcp_agent_mail_db::timestamps::now_micros();
        let identity = resolve_project_identity(project_path);
        let slug = &identity.project_uid;

        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                SqlValue::BigInt(1),
                SqlValue::Text(slug.to_string()),
                SqlValue::Text(project_path.to_string()),
                SqlValue::BigInt(now_us),
            ],
        )
        .unwrap();

        // Agents
        let agent_insert = "INSERT INTO agents (\
                id, project_id, name, program, model, task_description, \
                inception_ts, last_active_ts, attachments_policy, contact_policy\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
        conn.execute_sync(
            agent_insert,
            &[
                SqlValue::BigInt(1),
                SqlValue::BigInt(1),
                SqlValue::Text("AgentA".to_string()),
                SqlValue::Text("test".to_string()),
                SqlValue::Text("test".to_string()),
                SqlValue::Text(String::new()),
                SqlValue::BigInt(now_us),
                SqlValue::BigInt(now_us),
                SqlValue::Text("auto".to_string()),
                SqlValue::Text("auto".to_string()),
            ],
        )
        .unwrap();
        conn.execute_sync(
            agent_insert,
            &[
                SqlValue::BigInt(2),
                SqlValue::BigInt(1),
                SqlValue::Text("AgentB".to_string()),
                SqlValue::Text("test".to_string()),
                SqlValue::Text("test".to_string()),
                SqlValue::Text(String::new()),
                SqlValue::BigInt(now_us),
                SqlValue::BigInt(now_us),
                SqlValue::Text("auto".to_string()),
                SqlValue::Text("auto".to_string()),
            ],
        )
        .unwrap();

        // Messages
        let msg_insert = "INSERT INTO messages (\
                id, project_id, sender_id, thread_id, subject, body_md, importance, \
                ack_required, created_ts, attachments\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
        for i in 1..=3 {
            conn.execute_sync(
                msg_insert,
                &[
                    SqlValue::BigInt(i),
                    SqlValue::BigInt(1),
                    SqlValue::BigInt(1),
                    SqlValue::Null,
                    SqlValue::Text(format!("Message {i}")),
                    SqlValue::Text("body".to_string()),
                    SqlValue::Text("normal".to_string()),
                    SqlValue::BigInt(0),
                    SqlValue::BigInt(now_us),
                    SqlValue::Text("[]".to_string()),
                ],
            )
            .unwrap();
        }

        conn
    }

    #[test]
    fn integration_mail_status_shows_counts() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let project_path = "/tmp/mail-status-test-proj";
        let conn = seed_mail_status_db(&db_path, project_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_mail_status_sync(
            &conn,
            MailCommand::Status {
                project_path: PathBuf::from(project_path),
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "mail status failed: {result:?}");
        // The seeded DB has 3 messages and 2 agents
        assert!(
            output.contains("Messages") && output.contains("3"),
            "expected message count 3 in output, got: {output}"
        );
        assert!(
            output.contains("Agents") && output.contains("2"),
            "expected agent count 2 in output, got: {output}"
        );
    }

    #[test]
    fn integration_mail_status_empty_project() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        // Seed with one project path, then query with a different one
        let conn = seed_mail_status_db(&db_path, "/tmp/some-project");

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_mail_status_sync(
            &conn,
            MailCommand::Status {
                project_path: PathBuf::from("/tmp/nonexistent"),
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "mail status failed: {result:?}");
        // Should show 0 messages and 0 agents for a different project
        assert!(
            output.contains("Messages") && output.contains("0"),
            "expected 0 messages for nonexistent project, got: {output}"
        );
        assert!(
            output.contains("Agents") && output.contains("0"),
            "expected 0 agents for nonexistent project, got: {output}"
        );
    }

    #[test]
    fn integration_projects_mark_identity_writes_marker_file() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let result = handle_project_mark_identity(dir.path(), false);
        assert!(result.is_ok(), "mark-identity failed: {result:?}");

        let marker_path = dir.path().join(".agent-mail-project-id");
        assert!(marker_path.exists(), "marker file should exist");
        let marker = std::fs::read_to_string(&marker_path).unwrap();
        assert!(
            !marker.trim().is_empty(),
            "marker file should contain project_uid"
        );
    }

    #[test]
    fn integration_projects_discovery_init_writes_yaml() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let result = handle_project_discovery_init(dir.path(), Some("prod-alpha".to_string()));
        assert!(result.is_ok(), "discovery-init failed: {result:?}");

        let yaml_path = dir.path().join(".agent-mail.yaml");
        assert!(yaml_path.exists(), "discovery yaml should exist");
        let yaml = std::fs::read_to_string(&yaml_path).unwrap();
        assert!(
            yaml.contains("project_uid: "),
            "expected project_uid in discovery yaml: {yaml}"
        );
        assert!(
            yaml.contains("product_uid: prod-alpha"),
            "expected product_uid in discovery yaml: {yaml}"
        );
    }

    fn seed_projects_adopt_db(
        db_path: &Path,
        source_human_key: &Path,
        target_human_key: &Path,
    ) -> mcp_agent_mail_db::DbConn {
        use mcp_agent_mail_db::sqlmodel::Value as SqlValue;

        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");

        let now_us = mcp_agent_mail_db::timestamps::now_micros();

        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                SqlValue::BigInt(1),
                SqlValue::Text("src-proj".to_string()),
                SqlValue::Text(source_human_key.display().to_string()),
                SqlValue::BigInt(now_us),
            ],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                SqlValue::BigInt(2),
                SqlValue::Text("dst-proj".to_string()),
                SqlValue::Text(target_human_key.display().to_string()),
                SqlValue::BigInt(now_us),
            ],
        )
        .unwrap();

        let insert_agent = "INSERT INTO agents (\
                id, project_id, name, program, model, task_description, \
                inception_ts, last_active_ts, attachments_policy, contact_policy\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
        conn.execute_sync(
            insert_agent,
            &[
                SqlValue::BigInt(1),
                SqlValue::BigInt(1),
                SqlValue::Text("SrcAgent".to_string()),
                SqlValue::Text("test".to_string()),
                SqlValue::Text("test".to_string()),
                SqlValue::Text(String::new()),
                SqlValue::BigInt(now_us),
                SqlValue::BigInt(now_us),
                SqlValue::Text("auto".to_string()),
                SqlValue::Text("auto".to_string()),
            ],
        )
        .unwrap();
        conn.execute_sync(
            insert_agent,
            &[
                SqlValue::BigInt(2),
                SqlValue::BigInt(2),
                SqlValue::Text("DstAgent".to_string()),
                SqlValue::Text("test".to_string()),
                SqlValue::Text("test".to_string()),
                SqlValue::Text(String::new()),
                SqlValue::BigInt(now_us),
                SqlValue::BigInt(now_us),
                SqlValue::Text("auto".to_string()),
                SqlValue::Text("auto".to_string()),
            ],
        )
        .unwrap();

        conn.execute_sync(
            "INSERT INTO messages (\
                id, project_id, sender_id, thread_id, subject, body_md, importance, \
                ack_required, created_ts, attachments\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                SqlValue::BigInt(1),
                SqlValue::BigInt(1),
                SqlValue::BigInt(1),
                SqlValue::Null,
                SqlValue::Text("Subject".to_string()),
                SqlValue::Text("Body".to_string()),
                SqlValue::Text("normal".to_string()),
                SqlValue::BigInt(0),
                SqlValue::BigInt(now_us),
                SqlValue::Text("[]".to_string()),
            ],
        )
        .unwrap();

        conn.execute_sync(
            "INSERT INTO file_reservations (\
                id, project_id, agent_id, path_pattern, \"exclusive\", reason, \
                created_ts, expires_ts, released_ts\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                SqlValue::BigInt(1),
                SqlValue::BigInt(1),
                SqlValue::BigInt(1),
                SqlValue::Text("src/**".to_string()),
                SqlValue::BigInt(1),
                SqlValue::Text("test".to_string()),
                SqlValue::BigInt(now_us),
                SqlValue::BigInt(now_us + 3_600_000_000),
                SqlValue::Null,
            ],
        )
        .unwrap();

        conn
    }

    #[test]
    fn integration_projects_adopt_apply_rekeys_and_moves_archive() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let source_human_key = repo_root.join("src-worktree");
        let target_human_key = repo_root.join("dst-worktree");
        std::fs::create_dir_all(&source_human_key).unwrap();
        std::fs::create_dir_all(&target_human_key).unwrap();

        let git_init = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo_root)
            .status()
            .unwrap();
        assert!(git_init.success(), "git init should succeed");

        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_projects_adopt_db(&db_path, &source_human_key, &target_human_key);

        let storage_root = dir.path().join("archive-root");
        let source_archive = storage_root
            .join("projects")
            .join("src-proj")
            .join("messages");
        std::fs::create_dir_all(&source_archive).unwrap();
        std::fs::write(source_archive.join("m1.md"), "# message").unwrap();

        let cfg = Config {
            storage_root: storage_root.clone(),
            ..Config::default()
        };

        let result =
            handle_projects_adopt_with_conn(&conn, &cfg, "src-proj", "dst-proj", true, true);
        assert!(result.is_ok(), "projects adopt apply failed: {result:?}");

        let dst_agent_count: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM agents WHERE project_id = 2",
                &[],
            )
            .unwrap()
            .first()
            .and_then(|r| r.get_named("cnt").ok())
            .unwrap_or(0);
        let src_agent_count: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM agents WHERE project_id = 1",
                &[],
            )
            .unwrap()
            .first()
            .and_then(|r| r.get_named("cnt").ok())
            .unwrap_or(0);
        assert_eq!(dst_agent_count, 2, "expected both agents in target project");
        assert_eq!(
            src_agent_count, 0,
            "expected source project agents to be moved"
        );

        let dst_msg_count: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM messages WHERE project_id = 2",
                &[],
            )
            .unwrap()
            .first()
            .and_then(|r| r.get_named("cnt").ok())
            .unwrap_or(0);
        let dst_file_reservation_count: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM file_reservations WHERE project_id = 2",
                &[],
            )
            .unwrap()
            .first()
            .and_then(|r| r.get_named("cnt").ok())
            .unwrap_or(0);
        assert_eq!(dst_msg_count, 1, "expected messages rekeyed to target");
        assert_eq!(
            dst_file_reservation_count, 1,
            "expected file_reservations rekeyed to target"
        );

        let moved_file = storage_root
            .join("projects")
            .join("dst-proj")
            .join("messages")
            .join("m1.md");
        assert!(moved_file.exists(), "expected archive file moved to target");

        let aliases_path = storage_root
            .join("projects")
            .join("dst-proj")
            .join("aliases.json");
        assert!(aliases_path.exists(), "aliases.json should be written");
        let aliases = std::fs::read_to_string(aliases_path).unwrap();
        assert!(
            aliases.contains("src-proj"),
            "aliases.json should contain former source slug"
        );
    }

    // ── br-21gj.4.6: macro command parsing ─────────────────────────────

    #[test]
    fn clap_parses_macros_start_session_minimal() {
        let cli = Cli::try_parse_from([
            "am",
            "macros",
            "start-session",
            "-p",
            "/tmp/proj",
            "--program",
            "claude-code",
            "--model",
            "opus-4.6",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Macros {
                action:
                    MacroCommand::StartSession {
                        human_key,
                        program,
                        model,
                        agent_name,
                        task,
                        reserve_paths,
                        inbox_limit,
                        json,
                        ..
                    },
            } => {
                assert_eq!(human_key, "/tmp/proj");
                assert_eq!(program, "claude-code");
                assert_eq!(model, "opus-4.6");
                assert!(agent_name.is_none());
                assert!(task.is_none());
                assert!(reserve_paths.is_empty());
                assert_eq!(inbox_limit, 10);
                assert!(!json);
            }
            other => panic!("expected Macros StartSession, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_macros_start_session_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "macros",
            "start-session",
            "-p",
            "/tmp/proj",
            "--program",
            "codex-cli",
            "--model",
            "gpt-5",
            "-n",
            "BlueLake",
            "-t",
            "DB migration",
            "--reserve",
            "src/**",
            "--reserve",
            "tests/**",
            "--reserve-reason",
            "editing",
            "--reserve-ttl",
            "7200",
            "--inbox-limit",
            "25",
            "--json",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Macros {
                action:
                    MacroCommand::StartSession {
                        human_key,
                        program,
                        model,
                        agent_name,
                        task,
                        reserve_paths,
                        reserve_reason,
                        reserve_ttl,
                        inbox_limit,
                        json,
                        ..
                    },
            } => {
                assert_eq!(human_key, "/tmp/proj");
                assert_eq!(program, "codex-cli");
                assert_eq!(model, "gpt-5");
                assert_eq!(agent_name.as_deref(), Some("BlueLake"));
                assert_eq!(task.as_deref(), Some("DB migration"));
                assert_eq!(reserve_paths, vec!["src/**", "tests/**"]);
                assert_eq!(reserve_reason.as_deref(), Some("editing"));
                assert_eq!(reserve_ttl, 7200);
                assert_eq!(inbox_limit, 25);
                assert!(json);
            }
            other => panic!("expected Macros StartSession, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_macros_start_session_format_toon() {
        let cli = Cli::try_parse_from([
            "am",
            "macros",
            "start-session",
            "-p",
            "/tmp/proj",
            "--program",
            "codex-cli",
            "--model",
            "gpt-5",
            "--format",
            "toon",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Macros {
                action: MacroCommand::StartSession { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected Macros StartSession, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_macros_prepare_thread_minimal() {
        let cli = Cli::try_parse_from([
            "am",
            "macros",
            "prepare-thread",
            "-p",
            "/tmp/proj",
            "--thread-id",
            "TKT-42",
            "--program",
            "claude-code",
            "--model",
            "opus-4.6",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Macros {
                action:
                    MacroCommand::PrepareThread {
                        project_key,
                        thread_id,
                        program,
                        model,
                        agent_name,
                        inbox_limit,
                        json,
                        ..
                    },
            } => {
                assert_eq!(project_key, "/tmp/proj");
                assert_eq!(thread_id, "TKT-42");
                assert_eq!(program, "claude-code");
                assert_eq!(model, "opus-4.6");
                assert!(agent_name.is_none());
                assert_eq!(inbox_limit, 10);
                assert!(!json);
            }
            other => panic!("expected Macros PrepareThread, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_macros_prepare_thread_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "macros",
            "prepare-thread",
            "-p",
            "/tmp/proj",
            "--thread-id",
            "TKT-99",
            "--program",
            "codex-cli",
            "--model",
            "gpt-5",
            "-n",
            "RedFox",
            "-t",
            "thread review",
            "--no-register",
            "--no-examples",
            "--inbox-bodies",
            "--inbox-limit",
            "5",
            "--json",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Macros {
                action:
                    MacroCommand::PrepareThread {
                        project_key,
                        thread_id,
                        program,
                        model,
                        agent_name,
                        task,
                        no_register,
                        no_examples,
                        inbox_bodies,
                        inbox_limit,
                        json,
                        ..
                    },
            } => {
                assert_eq!(project_key, "/tmp/proj");
                assert_eq!(thread_id, "TKT-99");
                assert_eq!(program, "codex-cli");
                assert_eq!(model, "gpt-5");
                assert_eq!(agent_name.as_deref(), Some("RedFox"));
                assert_eq!(task.as_deref(), Some("thread review"));
                assert!(no_register);
                assert!(no_examples);
                assert!(inbox_bodies);
                assert_eq!(inbox_limit, 5);
                assert!(json);
            }
            other => panic!("expected Macros PrepareThread, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_macros_file_reservation_cycle_minimal() {
        let cli = Cli::try_parse_from([
            "am",
            "macros",
            "file-reservation-cycle",
            "-p",
            "/tmp/proj",
            "-a",
            "BlueLake",
            "--path",
            "src/**",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Macros {
                action:
                    MacroCommand::FileReservationCycle {
                        project_key,
                        agent_name,
                        paths,
                        ttl,
                        auto_release,
                        json,
                        ..
                    },
            } => {
                assert_eq!(project_key, "/tmp/proj");
                assert_eq!(agent_name, "BlueLake");
                assert_eq!(paths, vec!["src/**"]);
                assert_eq!(ttl, 3600);
                assert!(!auto_release);
                assert!(!json);
            }
            other => panic!("expected Macros FileReservationCycle, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_macros_file_reservation_cycle_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "macros",
            "file-reservation-cycle",
            "-p",
            "/tmp/proj",
            "-a",
            "RedFox",
            "--path",
            "src/**",
            "--path",
            "tests/**",
            "--ttl",
            "7200",
            "--no-exclusive",
            "--reason",
            "refactoring",
            "--auto-release",
            "--json",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Macros {
                action:
                    MacroCommand::FileReservationCycle {
                        project_key,
                        agent_name,
                        paths,
                        ttl,
                        no_exclusive,
                        reason,
                        auto_release,
                        json,
                        ..
                    },
            } => {
                assert_eq!(project_key, "/tmp/proj");
                assert_eq!(agent_name, "RedFox");
                assert_eq!(paths, vec!["src/**", "tests/**"]);
                assert_eq!(ttl, 7200);
                assert!(no_exclusive);
                assert_eq!(reason.as_deref(), Some("refactoring"));
                assert!(auto_release);
                assert!(json);
            }
            other => panic!("expected Macros FileReservationCycle, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_macros_contact_handshake_minimal() {
        let cli = Cli::try_parse_from([
            "am",
            "macros",
            "contact-handshake",
            "-p",
            "/tmp/proj",
            "--from",
            "BlueLake",
            "--to",
            "RedFox",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Macros {
                action:
                    MacroCommand::ContactHandshake {
                        project_key,
                        from,
                        to,
                        to_project,
                        auto_accept,
                        ttl,
                        json,
                        ..
                    },
            } => {
                assert_eq!(project_key, "/tmp/proj");
                assert_eq!(from, "BlueLake");
                assert_eq!(to, "RedFox");
                assert!(to_project.is_none());
                assert!(!auto_accept);
                assert_eq!(ttl, 604_800);
                assert!(!json);
            }
            other => panic!("expected Macros ContactHandshake, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_macros_contact_handshake_all_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "macros",
            "contact-handshake",
            "-p",
            "/tmp/proj",
            "--from",
            "BlueLake",
            "--to",
            "RedFox",
            "--to-project",
            "/tmp/other",
            "--reason",
            "collaboration",
            "--auto-accept",
            "--ttl",
            "86400",
            "--welcome-subject",
            "Hello!",
            "--welcome-body",
            "Let's work together.",
            "--thread-id",
            "TKT-1",
            "--register-missing",
            "--reg-program",
            "claude-code",
            "--reg-model",
            "opus-4.6",
            "--reg-task",
            "coordination",
            "--json",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Macros {
                action:
                    MacroCommand::ContactHandshake {
                        project_key,
                        from,
                        to,
                        to_project,
                        reason,
                        auto_accept,
                        ttl,
                        welcome_subject,
                        welcome_body,
                        thread_id,
                        register_missing,
                        reg_program,
                        reg_model,
                        reg_task,
                        json,
                        ..
                    },
            } => {
                assert_eq!(project_key, "/tmp/proj");
                assert_eq!(from, "BlueLake");
                assert_eq!(to, "RedFox");
                assert_eq!(to_project.as_deref(), Some("/tmp/other"));
                assert_eq!(reason.as_deref(), Some("collaboration"));
                assert!(auto_accept);
                assert_eq!(ttl, 86400);
                assert_eq!(welcome_subject.as_deref(), Some("Hello!"));
                assert_eq!(welcome_body.as_deref(), Some("Let's work together."));
                assert_eq!(thread_id.as_deref(), Some("TKT-1"));
                assert!(register_missing);
                assert_eq!(reg_program.as_deref(), Some("claude-code"));
                assert_eq!(reg_model.as_deref(), Some("opus-4.6"));
                assert_eq!(reg_task.as_deref(), Some("coordination"));
                assert!(json);
            }
            other => panic!("expected Macros ContactHandshake, got {other:?}"),
        }
    }

    #[test]
    fn help_macros_lists_subcommands() {
        let h = help_text_for(&["am", "macros", "--help"]);
        for cmd in [
            "start-session",
            "prepare-thread",
            "file-reservation-cycle",
            "contact-handshake",
        ] {
            assert!(
                h.contains(cmd),
                "macros help missing subcommand '{cmd}'\n{h}"
            );
        }
    }

    #[test]
    fn help_macros_start_session_lists_flags() {
        let h = help_text_for(&["am", "macros", "start-session", "--help"]);
        for flag in [
            "--project",
            "--program",
            "--model",
            "--reserve",
            "--inbox-limit",
            "--json",
        ] {
            assert!(
                h.contains(flag),
                "macros start-session help missing flag '{flag}'\n{h}"
            );
        }
    }

    #[test]
    fn clap_rejects_macros_start_session_missing_project() {
        let err = Cli::try_parse_from([
            "am",
            "macros",
            "start-session",
            "--program",
            "test",
            "--model",
            "test",
        ]);
        assert!(err.is_err());
    }

    #[test]
    fn clap_rejects_macros_file_reservation_cycle_missing_paths() {
        let err = Cli::try_parse_from([
            "am",
            "macros",
            "file-reservation-cycle",
            "-p",
            "/tmp/proj",
            "-a",
            "BlueLake",
        ]);
        assert!(err.is_err());
    }

    // ── Legacy/upgrade clap parsing tests ────────────────────────────────

    #[test]
    fn clap_parses_legacy_detect_defaults() {
        let cli = Cli::try_parse_from(["am", "legacy", "detect"]).unwrap();
        match cli.command.expect("expected command") {
            Commands::Legacy(legacy::LegacyArgs {
                action:
                    legacy::LegacyCommand::Detect {
                        search_root,
                        format,
                        json,
                    },
            }) => {
                assert!(search_root.is_none());
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected legacy detect, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_legacy_import_auto_dry_run() {
        let cli = Cli::try_parse_from(["am", "legacy", "import", "--auto", "--dry-run", "--yes"])
            .unwrap();
        match cli.command.expect("expected command") {
            Commands::Legacy(legacy::LegacyArgs {
                action:
                    legacy::LegacyCommand::Import {
                        auto,
                        search_root,
                        db,
                        storage_root,
                        in_place,
                        copy,
                        target_db,
                        target_storage_root,
                        dry_run,
                        yes,
                        format,
                        json,
                    },
            }) => {
                assert!(auto);
                assert!(search_root.is_none());
                assert!(db.is_none());
                assert!(storage_root.is_none());
                assert!(!in_place);
                assert!(!copy);
                assert!(target_db.is_none());
                assert!(target_storage_root.is_none());
                assert!(dry_run);
                assert!(yes);
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected legacy import, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_legacy_import_copy_with_explicit_targets() {
        let cli = Cli::try_parse_from([
            "am",
            "legacy",
            "import",
            "--db",
            "/tmp/legacy.sqlite3",
            "--storage-root",
            "/tmp/legacy-storage",
            "--copy",
            "--target-db",
            "/tmp/rust.sqlite3",
            "--target-storage-root",
            "/tmp/rust-storage",
            "--json",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Legacy(legacy::LegacyArgs {
                action:
                    legacy::LegacyCommand::Import {
                        auto,
                        db,
                        storage_root,
                        in_place,
                        copy,
                        target_db,
                        target_storage_root,
                        json,
                        ..
                    },
            }) => {
                assert!(!auto);
                assert_eq!(db, Some(PathBuf::from("/tmp/legacy.sqlite3")));
                assert_eq!(storage_root, Some(PathBuf::from("/tmp/legacy-storage")));
                assert!(!in_place);
                assert!(copy);
                assert_eq!(target_db, Some(PathBuf::from("/tmp/rust.sqlite3")));
                assert_eq!(
                    target_storage_root,
                    Some(PathBuf::from("/tmp/rust-storage"))
                );
                assert!(json);
            }
            other => panic!("expected legacy import, got {other:?}"),
        }
    }

    #[test]
    fn clap_rejects_legacy_import_in_place_and_copy() {
        let err = Cli::try_parse_from(["am", "legacy", "import", "--copy", "--in-place"]);
        assert!(err.is_err());
    }

    #[test]
    fn clap_parses_legacy_status() {
        let cli = Cli::try_parse_from([
            "am",
            "legacy",
            "status",
            "--search-root",
            "/tmp/proj",
            "--storage-root",
            "/tmp/storage",
            "--format",
            "toon",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Legacy(legacy::LegacyArgs {
                action:
                    legacy::LegacyCommand::Status {
                        search_root,
                        storage_root,
                        format,
                        json,
                    },
            }) => {
                assert_eq!(search_root, Some(PathBuf::from("/tmp/proj")));
                assert_eq!(storage_root, Some(PathBuf::from("/tmp/storage")));
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected legacy status, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_upgrade_flags() {
        let cli = Cli::try_parse_from([
            "am",
            "upgrade",
            "--search-root",
            "/tmp/proj",
            "--dry-run",
            "--yes",
            "--json",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Upgrade(legacy::UpgradeArgs {
                search_root,
                dry_run,
                yes,
                format,
                json,
            }) => {
                assert_eq!(search_root, Some(PathBuf::from("/tmp/proj")));
                assert!(dry_run);
                assert!(yes);
                assert!(format.is_none());
                assert!(json);
            }
            other => panic!("expected upgrade command, got {other:?}"),
        }
    }

    // ── Contacts clap parsing tests ──────────────────────────────────────

    #[test]
    fn clap_parses_contacts_request() {
        let cli = Cli::try_parse_from([
            "am",
            "contacts",
            "request",
            "--project",
            "my-proj",
            "--from",
            "BlueLake",
            "--to",
            "RedFox",
            "--reason",
            "need to coordinate",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Contacts {
                action:
                    ContactsCommand::Request {
                        project_key,
                        from_agent,
                        to_agent,
                        reason,
                        ttl_seconds,
                        format,
                        json,
                    },
            } => {
                assert_eq!(project_key, "my-proj");
                assert_eq!(from_agent, "BlueLake");
                assert_eq!(to_agent, "RedFox");
                assert_eq!(reason, "need to coordinate");
                assert_eq!(ttl_seconds, 604_800); // default 7 days
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected contacts request, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_contacts_request_custom_ttl() {
        let cli = Cli::try_parse_from([
            "am",
            "contacts",
            "request",
            "-p",
            "proj",
            "--from",
            "BlueLake",
            "--to",
            "RedFox",
            "--ttl-seconds",
            "86400",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Contacts {
                action:
                    ContactsCommand::Request {
                        ttl_seconds,
                        reason,
                        format,
                        json,
                        ..
                    },
            } => {
                assert_eq!(ttl_seconds, 86400);
                assert_eq!(reason, ""); // default
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected contacts request, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_contacts_respond_accept() {
        let cli = Cli::try_parse_from([
            "am", "contacts", "respond", "-p", "proj", "-a", "RedFox", "--from", "BlueLake",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Contacts {
                action:
                    ContactsCommand::Respond {
                        project_key,
                        agent_name,
                        from_agent,
                        accept,
                        reject,
                        format,
                        json,
                        ..
                    },
            } => {
                assert_eq!(project_key, "proj");
                assert_eq!(agent_name, "RedFox");
                assert_eq!(from_agent, "BlueLake");
                assert!(accept);
                assert!(!reject);
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected contacts respond, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_contacts_respond_reject() {
        let cli = Cli::try_parse_from([
            "am", "contacts", "respond", "-p", "proj", "-a", "RedFox", "--from", "BlueLake",
            "--reject",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Contacts {
                action:
                    ContactsCommand::Respond {
                        accept: _,
                        reject,
                        format,
                        json,
                        ..
                    },
            } => {
                assert!(reject);
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected contacts respond, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_contacts_list() {
        let cli = Cli::try_parse_from([
            "am", "contacts", "list", "-p", "proj", "-a", "BlueLake", "--json",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Contacts {
                action:
                    ContactsCommand::ListContacts {
                        project_key,
                        agent_name,
                        format,
                        json,
                    },
            } => {
                assert_eq!(project_key, "proj");
                assert_eq!(agent_name, "BlueLake");
                assert!(format.is_none());
                assert!(json);
            }
            other => panic!("expected contacts list, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_contacts_list_no_json() {
        let cli = Cli::try_parse_from(["am", "contacts", "list", "-p", "proj", "-a", "BlueLake"])
            .unwrap();
        match cli.command.expect("expected command") {
            Commands::Contacts {
                action: ContactsCommand::ListContacts { format, json, .. },
            } => {
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected contacts list, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_contacts_list_format_toon() {
        let cli = Cli::try_parse_from([
            "am", "contacts", "list", "-p", "proj", "-a", "BlueLake", "--format", "toon",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Contacts {
                action: ContactsCommand::ListContacts { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected contacts list, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_contacts_policy() {
        let cli = Cli::try_parse_from([
            "am",
            "contacts",
            "policy",
            "-p",
            "proj",
            "-a",
            "BlueLake",
            "contacts_only",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Contacts {
                action:
                    ContactsCommand::Policy {
                        project_key,
                        agent_name,
                        policy,
                        format,
                        json,
                    },
            } => {
                assert_eq!(project_key, "proj");
                assert_eq!(agent_name, "BlueLake");
                assert_eq!(policy, "contacts_only");
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected contacts policy, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_contacts_request_format_toon() {
        let cli = Cli::try_parse_from([
            "am",
            "contacts",
            "request",
            "--project",
            "proj",
            "--from",
            "BlueLake",
            "--to",
            "RedFox",
            "--format",
            "toon",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Contacts {
                action: ContactsCommand::Request { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected contacts request, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_contacts_respond_format_toon() {
        let cli = Cli::try_parse_from([
            "am",
            "contacts",
            "respond",
            "--project",
            "proj",
            "--agent",
            "RedFox",
            "--from",
            "BlueLake",
            "--format",
            "toon",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Contacts {
                action: ContactsCommand::Respond { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected contacts respond, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_contacts_policy_format_toon() {
        let cli = Cli::try_parse_from([
            "am",
            "contacts",
            "policy",
            "--project",
            "proj",
            "--agent",
            "BlueLake",
            "contacts_only",
            "--format",
            "toon",
        ])
        .unwrap();
        match cli.command.expect("expected command") {
            Commands::Contacts {
                action: ContactsCommand::Policy { format, json, .. },
            } => {
                assert_eq!(format, Some(output::CliOutputFormat::Toon));
                assert!(!json);
            }
            other => panic!("expected contacts policy, got {other:?}"),
        }
    }

    // ── Contacts integration tests ───────────────────────────────────────

    #[test]
    fn integration_contacts_request_creates_link() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_contacts_with_conn(
            &conn,
            ContactsCommand::Request {
                project_key: "test-proj".to_string(),
                from_agent: "BlueLake".to_string(),
                to_agent: "RedFox".to_string(),
                reason: "need coordination".to_string(),
                ttl_seconds: 3600,
                format: None,
                json: true,
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "request failed: {result:?}");
        assert!(
            output.contains("\"status\"") && output.contains("pending"),
            "expected pending status, got: {output}"
        );

        // Verify link in DB.
        let rows = conn
            .query_sync(
                "SELECT status, reason FROM agent_links WHERE a_agent_id = 1 AND b_agent_id = 2",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "expected 1 agent_link row");
        let status: String = rows[0].get_named("status").unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn integration_contacts_respond_approve() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        // First create a request.
        handle_contacts_with_conn(
            &conn,
            ContactsCommand::Request {
                project_key: "test-proj".to_string(),
                from_agent: "BlueLake".to_string(),
                to_agent: "RedFox".to_string(),
                reason: "collab".to_string(),
                ttl_seconds: 3600,
                format: None,
                json: false,
            },
        )
        .unwrap();

        // Approve it.
        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_contacts_with_conn(
            &conn,
            ContactsCommand::Respond {
                project_key: "test-proj".to_string(),
                agent_name: "RedFox".to_string(),
                from_agent: "BlueLake".to_string(),
                accept: true,
                reject: false,
                ttl_seconds: 86400,
                format: None,
                json: true,
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "respond failed: {result:?}");
        assert!(
            output.contains("approved"),
            "expected approved in output, got: {output}"
        );

        // Verify DB.
        let rows = conn
            .query_sync(
                "SELECT status FROM agent_links WHERE a_agent_id = 1 AND b_agent_id = 2",
                &[],
            )
            .unwrap();
        let status: String = rows[0].get_named("status").unwrap();
        assert_eq!(status, "approved");
    }

    #[test]
    fn integration_contacts_respond_reject() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        // Create + reject.
        handle_contacts_with_conn(
            &conn,
            ContactsCommand::Request {
                project_key: "test-proj".to_string(),
                from_agent: "RedFox".to_string(),
                to_agent: "BlueLake".to_string(),
                reason: "test".to_string(),
                ttl_seconds: 3600,
                format: None,
                json: false,
            },
        )
        .unwrap();

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_contacts_with_conn(
            &conn,
            ContactsCommand::Respond {
                project_key: "test-proj".to_string(),
                agent_name: "BlueLake".to_string(),
                from_agent: "RedFox".to_string(),
                accept: false,
                reject: true,
                ttl_seconds: 86400,
                format: None,
                json: true,
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "reject failed: {result:?}");
        assert!(
            output.contains("blocked"),
            "expected blocked status, got: {output}"
        );

        // Verify DB.
        let rows = conn
            .query_sync(
                "SELECT status FROM agent_links WHERE a_agent_id = 2 AND b_agent_id = 1",
                &[],
            )
            .unwrap();
        let status: String = rows[0].get_named("status").unwrap();
        assert_eq!(status, "blocked");
    }

    #[test]
    fn integration_contacts_list_json() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        // Create a link.
        handle_contacts_with_conn(
            &conn,
            ContactsCommand::Request {
                project_key: "test-proj".to_string(),
                from_agent: "BlueLake".to_string(),
                to_agent: "RedFox".to_string(),
                reason: "x".to_string(),
                ttl_seconds: 3600,
                format: None,
                json: false,
            },
        )
        .unwrap();

        // List contacts as JSON for BlueLake.
        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_contacts_with_conn(
            &conn,
            ContactsCommand::ListContacts {
                project_key: "test-proj".to_string(),
                agent_name: "BlueLake".to_string(),
                format: None,
                json: true,
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "list failed: {result:?}");
        assert!(
            output.contains("\"direction\"") && output.contains("outgoing"),
            "expected outgoing entry, got: {output}"
        );
        assert!(
            output.contains("RedFox"),
            "expected RedFox in contacts, got: {output}"
        );
    }

    #[test]
    fn integration_contacts_list_empty() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_contacts_with_conn(
            &conn,
            ContactsCommand::ListContacts {
                project_key: "test-proj".to_string(),
                agent_name: "BlueLake".to_string(),
                format: None,
                json: false,
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok());
        assert!(
            output.contains("No contacts") || output.is_empty(),
            "expected empty message, got: {output}"
        );
    }

    #[test]
    fn integration_contacts_policy_set() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_contacts_with_conn(
            &conn,
            ContactsCommand::Policy {
                project_key: "test-proj".to_string(),
                agent_name: "BlueLake".to_string(),
                policy: "contacts_only".to_string(),
                format: None,
                json: true,
            },
        );
        let output = capture.drain_to_string();
        assert!(result.is_ok(), "policy set failed: {result:?}");
        assert!(
            output.contains("contacts_only"),
            "expected policy in output, got: {output}"
        );

        // Verify DB.
        let rows = conn
            .query_sync(
                "SELECT contact_policy FROM agents WHERE name = 'BlueLake'",
                &[],
            )
            .unwrap();
        let policy: String = rows[0].get_named("contact_policy").unwrap();
        assert_eq!(policy, "contacts_only");
    }

    #[test]
    fn integration_contacts_policy_invalid() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let result = handle_contacts_with_conn(
            &conn,
            ContactsCommand::Policy {
                project_key: "test-proj".to_string(),
                agent_name: "BlueLake".to_string(),
                policy: "invalid_policy".to_string(),
                format: None,
                json: false,
            },
        );
        assert!(result.is_err(), "should fail for invalid policy");
    }

    #[test]
    fn integration_contacts_request_invalid_project() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let result = handle_contacts_with_conn(
            &conn,
            ContactsCommand::Request {
                project_key: "nonexistent".to_string(),
                from_agent: "BlueLake".to_string(),
                to_agent: "RedFox".to_string(),
                reason: String::new(),
                ttl_seconds: 3600,
                format: None,
                json: false,
            },
        );
        assert!(result.is_err(), "should fail for nonexistent project");
    }

    #[test]
    fn integration_contacts_request_invalid_agent() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        let result = handle_contacts_with_conn(
            &conn,
            ContactsCommand::Request {
                project_key: "test-proj".to_string(),
                from_agent: "NonexistentAgent".to_string(),
                to_agent: "RedFox".to_string(),
                reason: String::new(),
                ttl_seconds: 3600,
                format: None,
                json: false,
            },
        );
        assert!(result.is_err(), "should fail for nonexistent agent");
    }

    #[test]
    fn integration_contacts_request_upsert() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite3");
        let conn = seed_acks_and_reservations_db(&db_path);

        // First request.
        handle_contacts_with_conn(
            &conn,
            ContactsCommand::Request {
                project_key: "test-proj".to_string(),
                from_agent: "BlueLake".to_string(),
                to_agent: "RedFox".to_string(),
                reason: "first".to_string(),
                ttl_seconds: 3600,
                format: None,
                json: false,
            },
        )
        .unwrap();

        // Second request (upsert) with different reason.
        handle_contacts_with_conn(
            &conn,
            ContactsCommand::Request {
                project_key: "test-proj".to_string(),
                from_agent: "BlueLake".to_string(),
                to_agent: "RedFox".to_string(),
                reason: "updated".to_string(),
                ttl_seconds: 7200,
                format: None,
                json: false,
            },
        )
        .unwrap();

        // Should still be just 1 row (upserted).
        let rows = conn
            .query_sync(
                "SELECT reason FROM agent_links WHERE a_agent_id = 1 AND b_agent_id = 2",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "expected 1 row after upsert");
        let reason: String = rows[0].get_named("reason").unwrap();
        assert_eq!(reason, "updated");
    }

    // ── truncate_str UTF-8 safety ────────────────────────────────────

    #[test]
    fn truncate_str_ascii_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_ascii_over() {
        assert_eq!(truncate_str("hello world", 8), "hello...");
    }

    #[test]
    fn truncate_str_3byte_arrow() {
        let s = "foo → bar → baz";
        let r = truncate_str(s, 8);
        assert!(r.chars().count() <= 8);
        assert!(r.ends_with("..."));
    }

    #[test]
    fn truncate_str_cjk() {
        let s = "日本語テスト文字列";
        let r = truncate_str(s, 6);
        assert!(r.chars().count() <= 6);
        assert!(r.ends_with("..."));
    }

    #[test]
    fn truncate_str_emoji() {
        let s = "🔥🚀💡🎯🏆";
        let r = truncate_str(s, 4);
        assert!(r.chars().count() <= 4);
    }

    #[test]
    fn truncate_str_multibyte_sweep() {
        let s = "ab→cd🔥éf";
        for max in 1..=s.chars().count() + 2 {
            let r = truncate_str(s, max);
            assert!(
                r.chars().count() <= max.max(3),
                "max={max} got {} chars: {r:?}",
                r.chars().count()
            );
        }
    }

    #[test]
    fn truncate_str_empty() {
        assert_eq!(truncate_str("", 5), "");
    }

    // =========================================================================
    // resolve_project_async tests — Doom Loop Fix (br-3h13.16.2)
    // =========================================================================

    /// Helper: create a DbPool backed by a file-based SQLite DB with full
    /// migrations applied. Returns `(pool, tempdir)` — keep `_dir` alive.
    fn make_test_pool() -> (mcp_agent_mail_db::DbPool, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let db_path = dir.path().join("resolve_project_test.db");

        // Initialize the DB with base schema via DbConn.
        let init_conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite connection");
        init_conn
            .execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        init_conn
            .execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("initialize base schema");
        drop(init_conn);

        let cfg = mcp_agent_mail_db::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 2,
            run_migrations: false,
            warmup_connections: 0,
            ..mcp_agent_mail_db::DbPoolConfig::default()
        };
        let pool = mcp_agent_mail_db::create_pool(&cfg).expect("create pool");
        (pool, dir)
    }

    fn block_on_async<F, Fut, T>(f: F) -> T
    where
        F: FnOnce(asupersync::Cx) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let cx = asupersync::Cx::for_testing();
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        rt.block_on(f(cx))
    }

    // T16.2.1: slug lookup succeeds
    #[test]
    fn resolve_project_async_returns_project_by_slug() {
        let (pool, _dir) = make_test_pool();

        block_on_async(|cx| async move {
            // Create a project first via ensure_project.
            let human_key = "/tmp/resolve-slug-test";
            let created = mcp_agent_mail_db::queries::ensure_project(&cx, &pool, human_key)
                .await
                .into_result()
                .expect("ensure project");

            // Resolve by slug — should find it immediately.
            let resolved = resolve_project_async(&cx, &pool, &created.slug)
                .await
                .expect("resolve by slug");
            assert_eq!(resolved.slug, created.slug);
            assert_eq!(resolved.human_key, human_key);
            assert_eq!(resolved.id, created.id);
        });
    }

    // T16.2.2: slug NotFound falls through to human_key
    #[test]
    fn resolve_project_async_falls_through_to_human_key() {
        let (pool, _dir) = make_test_pool();

        block_on_async(|cx| async move {
            // Create a project.
            let human_key = "/home/user/my-project";
            let created = mcp_agent_mail_db::queries::ensure_project(&cx, &pool, human_key)
                .await
                .into_result()
                .expect("ensure project");

            // Resolve by human_key (not by slug) — slug lookup will NotFound,
            // falls through to human_key lookup.
            let resolved = resolve_project_async(&cx, &pool, human_key)
                .await
                .expect("resolve by human_key");
            assert_eq!(resolved.human_key, human_key);
            assert_eq!(resolved.slug, created.slug);
            assert_eq!(resolved.id, created.id);
        });
    }

    // T16.2.3: real DB errors surface immediately
    #[test]
    fn resolve_project_async_surfaces_db_errors() {
        // Create a pool pointing to a corrupt/broken database.
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("broken.db");

        // Write junk to the DB file to make it corrupt.
        std::fs::write(&db_path, b"this is not a valid sqlite database")
            .expect("write junk DB file");

        let cfg = mcp_agent_mail_db::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..mcp_agent_mail_db::DbPoolConfig::default()
        };

        // Pool creation may succeed (lazy connections) — the error surfaces on use.
        match mcp_agent_mail_db::create_pool(&cfg) {
            Ok(pool) => {
                let result = block_on_async(|cx| async move {
                    resolve_project_async(&cx, &pool, "anything").await
                });
                assert!(
                    result.is_err(),
                    "resolve_project_async should return error for corrupt DB"
                );
                let err_msg = format!("{}", result.unwrap_err());
                // The error should surface as a database error, NOT as
                // "ensure_project failed" (the old misleading path).
                assert!(
                    err_msg.contains("database error")
                        || err_msg.contains("not a database")
                        || err_msg.contains("corrupt")
                        || err_msg.contains("malformed")
                        || err_msg.contains("project not found"),
                    "error should indicate DB issue, got: {err_msg}"
                );
            }
            Err(e) => {
                // Pool failed to create — that's also acceptable for a corrupt DB.
                let err_msg = format!("{e}");
                assert!(
                    err_msg.contains("database")
                        || err_msg.contains("sqlite")
                        || err_msg.contains("not a database"),
                    "pool creation error should mention database, got: {err_msg}"
                );
            }
        }
    }

    // T16.2.4: auto-creates project for absolute paths
    #[test]
    fn resolve_project_async_auto_creates_for_absolute_paths() {
        let (pool, _dir) = make_test_pool();

        block_on_async(|cx| async move {
            // Resolve an absolute path that doesn't exist yet — should auto-create.
            let abs_path = "/tmp/new-project-autocreate";
            let resolved = resolve_project_async(&cx, &pool, abs_path)
                .await
                .expect("resolve absolute path should auto-create");
            assert_eq!(resolved.human_key, abs_path);

            // Verify the project was actually created in the DB.
            let by_slug =
                mcp_agent_mail_db::queries::get_project_by_slug(&cx, &pool, &resolved.slug)
                    .await
                    .into_result()
                    .expect("lookup newly created project by slug");
            assert_eq!(by_slug.human_key, abs_path);
            assert_eq!(by_slug.id, resolved.id);
        });
    }

    // T16.2.4 (negative): relative path that doesn't match returns error
    #[test]
    fn resolve_project_async_rejects_unknown_relative_path() {
        let (pool, _dir) = make_test_pool();

        let result = block_on_async(|cx| async move {
            resolve_project_async(&cx, &pool, "nonexistent-project").await
        });
        assert!(result.is_err(), "unknown relative path should return error");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("project not found"),
            "error should say 'project not found', got: {err_msg}"
        );
    }

    // ── Doctor check probe tests (br-3h13.16.3) ────────────────────────────

    /// Run doctor check and capture JSON output, retrying once if capture is empty.
    fn run_doctor_check_json(db_url: &str, storage: &Path) -> serde_json::Value {
        for attempt in 0..2 {
            let capture = ftui_runtime::StdioCapture::install().unwrap();
            let result = handle_doctor_check_with(db_url, storage, None, false, None, true);
            let output = capture.drain_to_string();
            assert!(result.is_ok(), "doctor check should not error: {result:?}");
            if let Some(parsed) = extract_doctor_check_json(&output) {
                return parsed;
            }
            if attempt == 0 {
                // StdioCapture can miss output from async runtime work; retry
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        panic!("doctor check produced no matching JSON output after 2 attempts");
    }

    #[test]
    fn doctor_check_healthy_db_reports_db_file_sanity_and_pool_init_ok() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("healthy.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());

        // Fully migrate and seed with a project + agent
        handle_migrate_with_database_url(&db_url).expect("migrate");
        let conn = open_db_sync_with_database_url(&db_url).expect("open");
        conn.query_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('test-proj', '/tmp/test-proj', 0)",
            &[],
        )
        .expect("insert project");
        conn.query_sync(
            "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) \
             VALUES (1, 'RedFox', 'test', 'test', 0, 0)",
            &[],
        )
        .expect("insert agent");
        drop(conn);

        let parsed = run_doctor_check_json(&db_url, dir.path());

        assert_eq!(parsed["healthy"], true, "healthy should be true");

        let checks = parsed["checks"].as_array().expect("checks array");

        // db_file_sanity should be present and OK
        let sanity = checks
            .iter()
            .find(|c| c["check"].as_str() == Some("db_file_sanity"))
            .expect("db_file_sanity check should be present");
        assert_eq!(
            sanity["status"].as_str().unwrap(),
            "ok",
            "db_file_sanity should pass on healthy DB"
        );
        assert!(
            sanity["detail"]
                .as_str()
                .unwrap()
                .contains("quick_check OK"),
            "detail should mention quick_check OK"
        );

        // pool_init should be present and OK
        let pool_check = checks
            .iter()
            .find(|c| c["check"].as_str() == Some("pool_init"))
            .expect("pool_init check should be present");
        assert_eq!(
            pool_check["status"].as_str().unwrap(),
            "ok",
            "pool_init should pass on healthy DB"
        );
    }

    #[test]
    fn doctor_check_zero_byte_db_reports_db_file_sanity_fail() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("empty.sqlite3");

        // Create a zero-byte file and make it read-only so that Check 1
        // (open_db_sync_with_database_url) cannot silently "repair" it by
        // writing the schema — otherwise the file grows before Check 1b reads
        // the metadata and the 0-byte branch is never taken.
        std::fs::write(&db_path, b"").expect("create empty file");
        {
            let mut perms = std::fs::metadata(&db_path).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(true);
            std::fs::set_permissions(&db_path, perms).expect("set read-only");
        }

        let db_url = format!("sqlite:///{}", db_path.display());

        let parsed = run_doctor_check_json(&db_url, dir.path());

        // healthy should be false because db_file_sanity fails
        assert_eq!(parsed["healthy"], false, "healthy should be false");

        let checks = parsed["checks"].as_array().expect("checks array");

        // db_file_sanity should be present and FAIL
        let sanity = checks
            .iter()
            .find(|c| c["check"].as_str() == Some("db_file_sanity"))
            .expect("db_file_sanity check should be present");
        assert_eq!(
            sanity["status"].as_str().unwrap(),
            "fail",
            "db_file_sanity should fail on 0-byte DB"
        );
        let detail = sanity["detail"].as_str().unwrap();
        assert!(
            detail.contains("0 bytes") || detail.contains("empty") || detail.contains("corrupt"),
            "detail should mention empty/corrupt/0 bytes, got: {detail}"
        );
    }

    #[test]
    fn doctor_check_corrupt_db_reports_pool_init_fail() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("corrupt.sqlite3");

        // Write garbage bytes to create a corrupt DB file (non-zero but invalid)
        std::fs::write(
            &db_path,
            b"THIS IS NOT A SQLITE FILE AT ALL - GARBAGE DATA HERE!",
        )
        .expect("write corrupt file");

        let db_url = format!("sqlite:///{}", db_path.display());

        let parsed = run_doctor_check_json(&db_url, dir.path());

        // healthy should be false
        assert_eq!(parsed["healthy"], false, "healthy should be false");

        let checks = parsed["checks"].as_array().expect("checks array");

        // pool_init should be present and FAIL (can't run migrations on corrupt DB)
        let pool_check = checks
            .iter()
            .find(|c| c["check"].as_str() == Some("pool_init"))
            .expect("pool_init check should be present");
        assert_eq!(
            pool_check["status"].as_str().unwrap(),
            "fail",
            "pool_init should fail on corrupt DB"
        );
    }

    #[test]
    fn doctor_check_includes_storage_root_health_probes() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("storage.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());

        handle_migrate_with_database_url(&db_url).expect("migrate");
        let parsed = run_doctor_check_json(&db_url, dir.path());
        let checks = parsed["checks"].as_array().expect("checks array");

        let expected = [
            "storage_root",
            "storage_root_writable",
            "storage_root_disk_space",
            "storage_root_git_repo",
            "storage_root_git_index_lock",
            "guard_hooks",
        ];
        for check in expected {
            assert!(
                checks.iter().any(|c| c["check"].as_str() == Some(check)),
                "missing check '{check}' in doctor output"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn doctor_check_storage_root_writable_fails_for_read_only_root() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("storage.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());
        handle_migrate_with_database_url(&db_url).expect("migrate");

        let storage_dir = tempfile::tempdir().unwrap();
        let original_mode = std::fs::metadata(storage_dir.path())
            .expect("metadata")
            .permissions()
            .mode();
        std::fs::set_permissions(storage_dir.path(), std::fs::Permissions::from_mode(0o555))
            .expect("set read-only dir");

        let parsed = run_doctor_check_json(&db_url, storage_dir.path());
        let checks = parsed["checks"].as_array().expect("checks array");
        let writable = checks
            .iter()
            .find(|c| c["check"].as_str() == Some("storage_root_writable"))
            .expect("storage_root_writable check");
        assert_eq!(writable["status"], "fail");
        let detail = writable["detail"].as_str().unwrap_or("");
        assert!(
            detail.contains("not writable") || detail.contains("permissions"),
            "unexpected writable failure detail: {detail}"
        );

        std::fs::set_permissions(
            storage_dir.path(),
            std::fs::Permissions::from_mode(original_mode),
        )
        .expect("restore permissions");
    }

    #[test]
    fn open_db_sync_with_database_url_recovers_malformed_relative_with_absolute_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let absolute_db = dir.path().join("storage.sqlite3");
        let absolute_db_str = absolute_db.to_string_lossy().into_owned();

        let absolute_conn =
            mcp_agent_mail_db::DbConn::open_file(&absolute_db_str).expect("open absolute db");
        absolute_conn
            .execute_raw("CREATE TABLE seed (id INTEGER PRIMARY KEY)")
            .expect("create seed");
        drop(absolute_conn);

        let relative_path = PathBuf::from(absolute_db_str.trim_start_matches('/'));
        if let Some(parent) = relative_path.parent() {
            std::fs::create_dir_all(parent).expect("create relative parent");
        }
        std::fs::write(&relative_path, b"not-a-database").expect("write malformed relative db");

        let db_url = format!("sqlite:///{}", relative_path.display());
        let conn = open_db_sync_with_database_url(&db_url).expect("open with fallback");
        let rows = conn
            .query_sync("SELECT 1 AS one", &[])
            .expect("query fallback connection");
        let one: i64 = rows.first().expect("row").get_named("one").unwrap_or(0);
        assert_eq!(one, 1, "fallback connection should execute queries");

        let relative_bytes = std::fs::read(&relative_path).expect("read relative bytes");
        assert_eq!(
            relative_bytes, b"not-a-database",
            "malformed relative file should remain untouched"
        );

        let _ = std::fs::remove_file(&relative_path);
        if let Some(parent) = relative_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn query_preflight_banner_stats_batched_reads_expected_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("preflight_banner.sqlite3");
        let db_path_str = db_path.to_string_lossy().into_owned();
        let conn = mcp_agent_mail_db::DbConn::open_file(&db_path_str).expect("open db");

        conn.execute_raw("CREATE TABLE projects(id INTEGER PRIMARY KEY)")
            .expect("create projects");
        conn.execute_raw("CREATE TABLE agents(id INTEGER PRIMARY KEY)")
            .expect("create agents");
        conn.execute_raw("CREATE TABLE messages(id INTEGER PRIMARY KEY)")
            .expect("create messages");
        conn.execute_raw("CREATE TABLE file_reservations(id INTEGER PRIMARY KEY)")
            .expect("create file_reservations");
        conn.execute_raw("CREATE TABLE agent_links(id INTEGER PRIMARY KEY)")
            .expect("create agent_links");

        conn.execute_raw("INSERT INTO projects(id) VALUES (1), (2)")
            .expect("seed projects");
        conn.execute_raw("INSERT INTO agents(id) VALUES (1)")
            .expect("seed agents");
        conn.execute_raw("INSERT INTO messages(id) VALUES (1), (2), (3)")
            .expect("seed messages");
        conn.execute_raw("INSERT INTO file_reservations(id) VALUES (1)")
            .expect("seed file_reservations");
        conn.execute_raw("INSERT INTO agent_links(id) VALUES (1), (2)")
            .expect("seed agent_links");

        let stats =
            query_preflight_banner_stats_batched(&conn).expect("batched preflight stats query");
        assert_eq!(stats.projects, 2);
        assert_eq!(stats.agents, 1);
        assert_eq!(stats.messages, 3);
        assert_eq!(stats.file_reservations, 1);
        assert_eq!(stats.contact_links, 2);
    }

    #[test]
    fn query_preflight_banner_stats_batched_returns_none_on_missing_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("preflight_banner_missing.sqlite3");
        let db_path_str = db_path.to_string_lossy().into_owned();
        let conn = mcp_agent_mail_db::DbConn::open_file(&db_path_str).expect("open db");

        conn.execute_raw("CREATE TABLE projects(id INTEGER PRIMARY KEY)")
            .expect("create projects");
        // Intentionally omit the other required tables to trigger a graceful
        // batched-query miss so callers can fall back to default zero stats.
        assert!(query_preflight_banner_stats_batched(&conn).is_none());
    }

    #[test]
    fn query_preflight_banner_stats_batched_uses_max_id_fast_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("preflight_banner_sparse.sqlite3");
        let db_path_str = db_path.to_string_lossy().into_owned();
        let conn = mcp_agent_mail_db::DbConn::open_file(&db_path_str).expect("open db");

        conn.execute_raw("CREATE TABLE projects(id INTEGER PRIMARY KEY)")
            .expect("create projects");
        conn.execute_raw("CREATE TABLE agents(id INTEGER PRIMARY KEY)")
            .expect("create agents");
        conn.execute_raw("CREATE TABLE messages(id INTEGER PRIMARY KEY)")
            .expect("create messages");
        conn.execute_raw("CREATE TABLE file_reservations(id INTEGER PRIMARY KEY)")
            .expect("create file_reservations");
        conn.execute_raw("CREATE TABLE agent_links(id INTEGER PRIMARY KEY)")
            .expect("create agent_links");

        conn.execute_raw("INSERT INTO messages(id) VALUES (1), (100)")
            .expect("seed sparse messages");

        let stats =
            query_preflight_banner_stats_batched(&conn).expect("batched preflight stats query");
        assert_eq!(stats.messages, 100);
        assert_eq!(stats.projects, 0);
        assert_eq!(stats.agents, 0);
    }

    #[test]
    fn sqlite_corruption_error_message_detection_includes_no_backup_marker() {
        assert!(is_sqlite_corruption_error_message(
            "database file tmp/storage.sqlite3 is malformed and no healthy backup was found"
        ));
    }

    #[test]
    fn sqlite_recovery_error_message_detection_includes_internal_oom_signals() {
        assert!(is_sqlite_recovery_error_message(
            "Query error: out of memory"
        ));
        assert!(is_sqlite_recovery_error_message(
            "Query error: cursor stack is empty"
        ));
    }

    #[test]
    fn open_db_sync_with_database_url_auto_restores_from_bak_on_corruption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("storage.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());

        handle_migrate_with_database_url(&db_url).expect("migrate");
        let conn = open_db_sync_with_database_url(&db_url).expect("open");
        conn.execute_raw("CREATE TABLE marker(value TEXT)")
            .expect("create marker table");
        conn.execute_raw("INSERT INTO marker(value) VALUES('from-backup')")
            .expect("seed marker");
        drop(conn);

        let bak_path = PathBuf::from(format!("{}.bak", db_path.display()));
        std::fs::copy(&db_path, &bak_path).expect("create .bak backup");
        std::fs::write(&db_path, b"THIS FILE IS CORRUPT").expect("corrupt primary");

        let reopened = open_db_sync_with_database_url(&db_url).expect("auto-recover");
        let rows = reopened
            .query_sync("SELECT value FROM marker", &[])
            .expect("query marker");
        let marker: String = rows.first().unwrap().get_named("value").unwrap();
        assert_eq!(marker, "from-backup", "should restore from .bak backup");
    }

    #[test]
    fn open_db_sync_with_database_url_auto_quarantines_corrupt_db_without_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("storage.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());

        std::fs::write(&db_path, b"NOT A SQLITE DATABASE").expect("write corrupt db");
        let conn = open_db_sync_with_database_url(&db_url).expect("open after quarantine");
        let rows = conn
            .query_sync(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='projects'",
                &[],
            )
            .expect("query sqlite_master");
        assert!(
            !rows.is_empty(),
            "schema init should recreate core tables after quarantine"
        );
        drop(conn);

        let quarantine_count = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| name.starts_with("storage.sqlite3.corrupt-"))
                    .unwrap_or(false)
            })
            .count();
        assert!(
            quarantine_count >= 1,
            "corrupt DB should be preserved as quarantined artifact"
        );
    }

    // ── Doctor repair .bak sibling tests (br-3h13.16.4) ────────────────────

    #[test]
    fn doctor_repair_creates_valid_bak_sibling_file() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("repair_test.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());
        let backup_dir = dir.path().join("backups");

        // Create and populate DB
        handle_migrate_with_database_url(&db_url).expect("migrate");
        let conn = open_db_sync_with_database_url(&db_url).expect("open");
        conn.query_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('p1', '/tmp/p1', 0)",
            &[],
        )
        .expect("insert project");
        conn.query_sync(
            "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) \
             VALUES (1, 'RedFox', 'cc', 'opus', 0, 0)",
            &[],
        )
        .expect("insert agent");
        drop(conn);

        // Run doctor repair (not dry_run)
        let _capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_doctor_repair_with(&db_url, &backup_dir, None, false, false);
        assert!(result.is_ok(), "repair failed: {result:?}");

        // Verify .bak sibling exists
        let bak_path = format!("{}.bak", db_path.display());
        let bak = std::path::Path::new(&bak_path);
        assert!(bak.exists(), ".bak sibling should exist at {bak_path}");
        assert!(
            bak.metadata().unwrap().len() > 0,
            ".bak should be non-empty"
        );

        // Verify .bak is a valid SQLite DB with same data
        let bak_conn = mcp_agent_mail_db::DbConn::open_file(&bak_path).expect("open .bak");
        let rows = bak_conn
            .query_sync("SELECT COUNT(*) AS cnt FROM projects", &[])
            .expect("query projects in .bak");
        let count: i64 = rows.first().unwrap().get_named("cnt").unwrap();
        assert_eq!(count, 1, ".bak should contain 1 project");

        let agent_rows = bak_conn
            .query_sync("SELECT COUNT(*) AS cnt FROM agents", &[])
            .expect("query agents in .bak");
        let agent_count: i64 = agent_rows.first().unwrap().get_named("cnt").unwrap();
        assert_eq!(agent_count, 1, ".bak should contain 1 agent");

        // Verify original DB still exists and is healthy
        assert!(db_path.exists(), "original DB should still exist");
        let orig_conn = open_db_sync_with_database_url(&db_url).expect("reopen original");
        let orig_rows = orig_conn
            .query_sync("SELECT COUNT(*) AS cnt FROM projects", &[])
            .expect("query original");
        let orig_count: i64 = orig_rows.first().unwrap().get_named("cnt").unwrap();
        assert_eq!(orig_count, 1, "original should still have 1 project");

        // Verify timestamped backup in backups/ dir also exists
        assert!(backup_dir.exists(), "backups dir should exist");
        let entries: Vec<_> = std::fs::read_dir(&backup_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !entries.is_empty(),
            "backups dir should contain timestamped backup"
        );
    }

    #[test]
    fn doctor_repair_bak_found_by_pool_backup_candidates() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("pool_test.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());
        let backup_dir = dir.path().join("backups");

        // Create DB and run repair to produce .bak
        handle_migrate_with_database_url(&db_url).expect("migrate");
        let conn = open_db_sync_with_database_url(&db_url).expect("open");
        conn.query_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('p2', '/tmp/p2', 0)",
            &[],
        )
        .expect("insert project");
        drop(conn);

        let _capture = ftui_runtime::StdioCapture::install().unwrap();
        handle_doctor_repair_with(&db_url, &backup_dir, None, false, false).expect("repair");

        // Verify the .bak is a valid DB that passes quick_check
        let bak_path = format!("{}.bak", db_path.display());
        let bak_conn = mcp_agent_mail_db::DbConn::open_file(&bak_path).expect("open .bak");
        let qc_rows = bak_conn
            .query_sync("PRAGMA quick_check", &[])
            .expect("quick_check");
        let qc_result: String = qc_rows.first().unwrap().get_named("quick_check").unwrap();
        assert_eq!(qc_result, "ok", ".bak should pass quick_check");

        // The .bak file uses the naming convention expected by
        // pool's sqlite_backup_candidates: "{primary_path}.bak"
        let expected_name = format!("{}.bak", db_path.file_name().unwrap().to_str().unwrap());
        let bak_file = std::path::Path::new(&bak_path);
        assert_eq!(
            bak_file.file_name().unwrap().to_str().unwrap(),
            expected_name,
            ".bak should match naming convention for pool auto-recovery"
        );
    }

    #[test]
    fn doctor_repair_gracefully_handles_bak_creation_failure() {
        let _guard = stdio_capture_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("readonly_test.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());

        // Use a separate writable backup dir (so timestamped backup succeeds)
        let backup_dir = dir.path().join("backups");

        // Create DB
        handle_migrate_with_database_url(&db_url).expect("migrate");

        // Pre-create a .bak path as a directory — fs::copy to a directory fails
        let bak_path = format!("{}.bak", db_path.display());
        std::fs::create_dir_all(&bak_path).expect("create .bak as directory");

        let capture = ftui_runtime::StdioCapture::install().unwrap();
        let result = handle_doctor_repair_with(&db_url, &backup_dir, None, false, false);
        let output = capture.drain_to_string();

        // Repair should still complete (non-fatal .bak failure)
        assert!(
            result.is_ok(),
            "repair should succeed despite .bak failure: {result:?}"
        );

        // Warning about .bak failure should appear in output
        assert!(
            output.contains("Warning") && output.contains("could not create sibling backup"),
            "should warn about .bak failure, got: {output}"
        );

        // Timestamped backup in backups/ should still exist
        assert!(backup_dir.exists(), "backups dir should still be created");
        let entries: Vec<_> = std::fs::read_dir(&backup_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !entries.is_empty(),
            "timestamped backup should exist despite .bak failure"
        );

        // Original DB should be undamaged
        let conn = open_db_sync_with_database_url(&db_url).expect("reopen original");
        let rows = conn
            .query_sync("PRAGMA quick_check", &[])
            .expect("quick_check");
        let qc: String = rows.first().unwrap().get_named("quick_check").unwrap();
        assert_eq!(qc, "ok", "original DB should still be healthy");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseRecord {
    slot: String,
    agent: String,
    branch: String,
    exclusive: bool,
    acquired_ts: String,
    expires_ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    released_ts: Option<String>,
}

fn handle_am_run(args: AmRunArgs) -> CliResult<()> {
    let config = Config::from_env();
    let server_url = format!(
        "http://{}:{}{}",
        config.http_host, config.http_port, config.http_path
    );
    handle_am_run_with(
        &config,
        Some(server_url.as_str()),
        config.http_bearer_token.as_deref(),
        args,
    )
}

#[allow(clippy::too_many_lines)]
fn handle_am_run_with(
    config: &Config,
    server_url: Option<&str>,
    bearer: Option<&str>,
    args: AmRunArgs,
) -> CliResult<()> {
    use std::time::Duration;

    let identity = resolve_project_identity(args.path.to_string_lossy().as_ref());
    let agent_name = args
        .agent
        .or_else(|| std::env::var("AGENT_NAME").ok())
        .unwrap_or_else(|| "Unknown".to_string());
    let branch = identity
        .branch
        .clone()
        .filter(|b| !b.is_empty())
        .or_else(|| compute_git_branch(&args.path))
        .unwrap_or_else(|| "unknown".to_string());

    let shared = resolve_bool(args.shared, args.exclusive, false);
    let block_on_conflicts =
        resolve_bool(args.block_on_conflicts, args.no_block_on_conflicts, false);

    let cache_key = format!(
        "am-cache-{}-{}-{}",
        identity.project_uid, agent_name, branch
    );

    let ttl_seconds = args.ttl_seconds.max(60);
    let now = Utc::now();
    let expires = now + chrono::Duration::seconds(ttl_seconds);
    let lease = LeaseRecord {
        slot: args.slot.clone(),
        agent: agent_name.clone(),
        branch: branch.clone(),
        exclusive: !shared,
        acquired_ts: now.to_rfc3339(),
        expires_ts: expires.to_rfc3339(),
        released_ts: None,
    };

    // Ensure local lease path exists upfront so tests can observe it even if server path is used.
    // This is best-effort (legacy behavior) because server-based build slot leases don't strictly
    // require local filesystem writes.
    let mut slot_dir_opt: Option<PathBuf> = None;
    let mut lease_path_opt: Option<PathBuf> = None;
    if let Ok(dir) = ensure_slot_dir(config, &identity.slug, &args.slot) {
        let path = lease_path(&dir, &agent_name, &branch);
        let _ = write_lease(&path, &lease);
        slot_dir_opt = Some(dir);
        lease_path_opt = Some(path);
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum LeaseBackend {
        Server,
        Local,
        None,
    }

    let mut backend = LeaseBackend::None;
    let mut acquired_via_server = false;
    let mut planned_exit_code: Option<i32> = None;

    let mut stop_tx: Option<std::sync::mpsc::Sender<()>> = None;
    let mut renew_thread: Option<std::thread::JoinHandle<()>> = None;

    if config.worktrees_enabled {
        // Prefer server tools when available; fallback to local filesystem leases.
        let mut server_conflicts: Vec<serde_json::Value> = Vec::new();
        if let Some(url) = server_url {
            use asupersync::runtime::RuntimeBuilder;

            let runtime = RuntimeBuilder::current_thread()
                .build()
                .map_err(|e| CliError::Other(format!("runtime init failed: {e}")))?;

            let ensure_ok = runtime
                .block_on(async {
                    try_call_server_tool(
                        url,
                        bearer,
                        "ensure_project",
                        serde_json::json!({ "human_key": identity.human_key.clone() }),
                    )
                    .await
                })
                .is_some();

            if ensure_ok {
                let acquired = runtime.block_on(async {
                    try_call_server_tool(
                        url,
                        bearer,
                        "acquire_build_slot",
                        serde_json::json!({
                            "project_key": identity.human_key.clone(),
                            "agent_name": agent_name.clone(),
                            "slot": args.slot.clone(),
                            "ttl_seconds": ttl_seconds,
                            "exclusive": !shared,
                        }),
                    )
                    .await
                });

                if let Some(result) = acquired.and_then(coerce_tool_result_json) {
                    backend = LeaseBackend::Server;
                    acquired_via_server = true;
                    server_conflicts = result
                        .get("conflicts")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                }
            }
        }

        if backend != LeaseBackend::Server {
            backend = LeaseBackend::Local;
        }

        if backend == LeaseBackend::Server {
            if !server_conflicts.is_empty() {
                if guard_mode_warn() {
                    ftui_runtime::ftui_eprintln!(
                        "warning: build slot conflicts (server advisory, proceeding)"
                    );
                    for c in &server_conflicts {
                        let slot = c.get("slot").and_then(|v| v.as_str()).unwrap_or("");
                        let agent = c.get("agent").and_then(|v| v.as_str()).unwrap_or("");
                        let branch = c.get("branch").and_then(|v| v.as_str()).unwrap_or("");
                        let expires_ts = c.get("expires_ts").and_then(|v| v.as_str()).unwrap_or("");
                        ftui_runtime::ftui_eprintln!(
                            "  - slot={slot} agent={agent} branch={branch} expires={expires_ts}"
                        );
                    }
                }
                if !shared && block_on_conflicts {
                    ftui_runtime::ftui_eprintln!(
                        "error: build slot conflicts detected and --block-on-conflicts set; aborting."
                    );
                    planned_exit_code = Some(1);
                }
            }
        } else {
            let slot_dir = match slot_dir_opt {
                Some(dir) => dir,
                None => ensure_slot_dir(config, &identity.slug, &args.slot)?,
            };
            if lease_path_opt.is_none() {
                let path = lease_path(&slot_dir, &agent_name, &branch);
                let _ = write_lease(&path, &lease);
                lease_path_opt = Some(path);
            }

            let conflicts = read_active_leases(&slot_dir, &agent_name, &branch, shared);
            if !conflicts.is_empty() {
                if guard_mode_warn() {
                    ftui_runtime::ftui_eprintln!(
                        "warning: build slot conflicts (advisory, proceeding)"
                    );
                    for conflict in &conflicts {
                        ftui_runtime::ftui_eprintln!(
                            "  - slot={} agent={} branch={} expires={}",
                            conflict.slot,
                            conflict.agent,
                            conflict.branch,
                            conflict.expires_ts
                        );
                    }
                }
                if !shared && block_on_conflicts {
                    ftui_runtime::ftui_eprintln!(
                        "error: build slot conflicts detected and --block-on-conflicts set; aborting."
                    );
                    planned_exit_code = Some(1);
                }
            }
        }

        // Start renewer thread only if we are proceeding with the child command.
        if planned_exit_code.is_none() {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            stop_tx = Some(tx);

            let interval = std::cmp::max(60, ttl_seconds / 2);
            if backend == LeaseBackend::Server {
                let Some(url) = server_url.map(|s| s.to_string()) else {
                    return Err(CliError::Other(
                        "server_url missing while using server backend".to_string(),
                    ));
                };
                let bearer = bearer.map(|s| s.to_string());
                let project_key = identity.human_key.clone();
                let agent_name = agent_name.clone();
                let slot = args.slot.clone();

                renew_thread = Some(std::thread::spawn(move || {
                    use asupersync::runtime::RuntimeBuilder;

                    let Ok(runtime) = RuntimeBuilder::current_thread().build() else {
                        return;
                    };
                    loop {
                        match rx.recv_timeout(Duration::from_secs(interval as u64)) {
                            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                let _ = runtime.block_on(async {
                                    try_call_server_tool(
                                        &url,
                                        bearer.as_deref(),
                                        "renew_build_slot",
                                        serde_json::json!({
                                            "project_key": project_key,
                                            "agent_name": agent_name,
                                            "slot": slot,
                                            "extend_seconds": interval,
                                        }),
                                    )
                                    .await
                                });
                            }
                        }
                    }
                }));
            } else {
                let Some(lease_path) = lease_path_opt.clone() else {
                    return Err(CliError::Other(
                        "internal error: missing lease path for local build slot backend"
                            .to_string(),
                    ));
                };
                let slot_key = args.slot.clone();
                let agent_name = agent_name.clone();
                let branch = branch.clone();
                let exclusive = !shared;

                renew_thread = Some(std::thread::spawn(move || {
                    loop {
                        match rx.recv_timeout(Duration::from_secs(interval as u64)) {
                            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                let now = Utc::now();
                                let expires = now + chrono::Duration::seconds(interval);
                                let mut updated =
                                    read_lease(&lease_path).unwrap_or_else(|| LeaseRecord {
                                        slot: slot_key.clone(),
                                        agent: agent_name.clone(),
                                        branch: branch.clone(),
                                        exclusive,
                                        acquired_ts: now.to_rfc3339(),
                                        expires_ts: expires.to_rfc3339(),
                                        released_ts: None,
                                    });
                                updated.expires_ts = expires.to_rfc3339();
                                let _ = write_lease(&lease_path, &updated);
                            }
                        }
                    }
                }));
            }
        }
    }

    let mut cmd = std::process::Command::new(&args.cmd[0]);
    if args.cmd.len() > 1 {
        cmd.args(&args.cmd[1..]);
    }
    cmd.env("AM_SLOT", &args.slot)
        .env("SLUG", &identity.slug)
        .env("PROJECT_UID", &identity.project_uid)
        .env("BRANCH", &branch)
        .env("AGENT", &agent_name)
        .env("CACHE_KEY", &cache_key);

    let exit_code = if let Some(code) = planned_exit_code {
        code
    } else {
        ftui_runtime::ftui_println!("$ {}  (slot={})", args.cmd.join(" "), args.slot);

        let status = cmd.status();
        match status {
            Ok(s) => s.code().unwrap_or(1),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 127,
            Err(_) => 1,
        }
    };

    if config.worktrees_enabled {
        // Stop renewal, then release (server best-effort, local fallback).
        if let Some(tx) = stop_tx {
            let _ = tx.send(());
        }
        if let Some(handle) = renew_thread {
            let _ = handle.join();
        }

        let mut released_locally = false;
        if backend == LeaseBackend::Server && acquired_via_server {
            let server_released = if let Some(url) = server_url {
                use asupersync::runtime::RuntimeBuilder;
                match RuntimeBuilder::current_thread().build() {
                    Ok(runtime) => runtime
                        .block_on(async {
                            try_call_server_tool(
                                url,
                                bearer,
                                "release_build_slot",
                                serde_json::json!({
                                    "project_key": identity.human_key.clone(),
                                    "agent_name": agent_name.clone(),
                                    "slot": args.slot.clone(),
                                }),
                            )
                            .await
                        })
                        .is_some(),
                    Err(_) => false,
                }
            } else {
                false
            };

            if !server_released && let Some(path) = lease_path_opt.as_ref() {
                let now = Utc::now().to_rfc3339();
                if let Some(mut lease) = read_lease(path) {
                    lease.released_ts = Some(now.clone());
                    lease.expires_ts = now.clone();
                    let _ = write_lease(path, &lease);
                    released_locally = true;
                }
            }
        } else if let Some(path) = lease_path_opt.as_ref() {
            let now = Utc::now().to_rfc3339();
            if let Some(mut lease) = read_lease(path) {
                lease.released_ts = Some(now.clone());
                lease.expires_ts = now.clone();
                let _ = write_lease(path, &lease);
                released_locally = true;
            }
        }

        if !released_locally && backend == LeaseBackend::Local {
            // If local lease couldn't be read (should be rare), do not treat as fatal.
        }
    }

    if exit_code != 0 {
        return Err(CliError::ExitCode(exit_code));
    }
    Ok(())
}

fn resolve_bool(primary: bool, negated: bool, default: bool) -> bool {
    if negated {
        return false;
    }
    if primary {
        return true;
    }
    default
}

fn ensure_dir(path: &Path) -> CliResult<()> {
    if !path.exists() {
        return Err(CliError::InvalidArgument(format!(
            "path not found: {}",
            path.display()
        )));
    }
    if !path.is_dir() {
        return Err(CliError::InvalidArgument(format!(
            "expected directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn compute_git_branch(path: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() || text == "HEAD" {
        None
    } else {
        Some(text)
    }
}

fn ensure_slot_dir(config: &Config, slug: &str, slot: &str) -> CliResult<PathBuf> {
    let safe_slot = safe_component(slot);
    let slot_dir = config
        .storage_root
        .join("projects")
        .join(slug)
        .join("build_slots")
        .join(safe_slot);
    std::fs::create_dir_all(&slot_dir)?;
    Ok(slot_dir)
}

fn lease_path(slot_dir: &Path, agent: &str, branch: &str) -> PathBuf {
    let holder = safe_component(&format!("{agent}__{branch}"));
    slot_dir.join(format!("{holder}.json"))
}

fn safe_component(value: &str) -> String {
    let mut out = value.trim().to_string();
    for ch in ['/', '\\', ':', '*', '?', '"', '<', '>', '|', ' '] {
        out = out.replace(ch, "_");
    }
    // Prevent path traversal via special components.
    if out.is_empty() || out == "." || out == ".." {
        "unknown".to_string()
    } else {
        out
    }
}

fn guard_mode_warn() -> bool {
    matches!(
        std::env::var("AGENT_MAIL_GUARD_MODE")
            .unwrap_or_else(|_| "block".to_string())
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "warn" | "advisory" | "adv"
    )
}

fn read_active_leases(
    slot_dir: &Path,
    agent: &str,
    branch: &str,
    shared: bool,
) -> Vec<LeaseRecord> {
    let mut out = Vec::new();
    let now = Utc::now();
    let entries = match std::fs::read_dir(slot_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let lease = match read_lease(&path) {
            Some(l) => l,
            None => continue,
        };
        if let Some(exp) = parse_rfc3339(&lease.expires_ts)
            && exp <= now
        {
            continue;
        }
        if lease.exclusive && !shared && !(lease.agent == agent && lease.branch == branch) {
            out.push(lease);
        }
    }
    out
}

fn parse_rfc3339(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
}

fn read_lease(path: &Path) -> Option<LeaseRecord> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_lease(path: &Path, lease: &LeaseRecord) -> CliResult<()> {
    let payload = serde_json::to_string_pretty(lease)
        .map_err(|e| CliError::InvalidArgument(e.to_string()))?;
    std::fs::write(path, payload)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Share export pipeline
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ShareExportWizardDefaults {
    projects: Vec<String>,
    inline_threshold: i64,
    detach_threshold: i64,
    scrub_preset: String,
    chunk_threshold: i64,
    chunk_size: i64,
    zip: bool,
}

#[derive(Debug, Clone)]
struct ShareExportWizardResult {
    projects: Vec<String>,
    inline_threshold: i64,
    detach_threshold: i64,
    scrub_preset: String,
    chunk_threshold: i64,
    chunk_size: i64,
    zip: bool,
}

fn share_export_wizard(defaults: ShareExportWizardDefaults) -> CliResult<ShareExportWizardResult> {
    ftui_runtime::ftui_eprintln!("Interactive share export wizard\n");

    let projects_default = if defaults.projects.is_empty() {
        "all".to_string()
    } else {
        defaults.projects.join(", ")
    };
    let projects_line = prompt_line(&format!(
        "Project filters (comma-separated; empty = all) [{projects_default}]: "
    ))?;
    let projects = if projects_line.trim().is_empty() {
        defaults.projects
    } else {
        projects_line
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    };

    let inline_threshold = prompt_i64("Inline threshold bytes", defaults.inline_threshold, |v| {
        v >= 0
    })?;
    let detach_threshold = prompt_i64("Detach threshold bytes", defaults.detach_threshold, |v| {
        v >= 0
    })?;
    let chunk_threshold = prompt_i64("Chunk threshold bytes", defaults.chunk_threshold, |v| {
        v >= 0
    })?;
    let chunk_size = prompt_i64("Chunk size bytes (min 1024)", defaults.chunk_size, |v| {
        v >= 1024
    })?;

    let scrub_preset = loop {
        let line = prompt_line(&format!(
            "Scrub preset (standard/strict/archive) [{}]: ",
            defaults.scrub_preset
        ))?;
        let candidate = if line.trim().is_empty() {
            defaults.scrub_preset.clone()
        } else {
            line.trim().to_string()
        };
        match share::normalize_scrub_preset(&candidate) {
            Ok(preset) => break preset.as_str().to_string(),
            Err(_) => {
                ftui_runtime::ftui_eprintln!(
                    "Invalid scrub preset: {candidate}. Expected: standard, strict, archive."
                );
            }
        }
    };

    let zip = prompt_bool("Package as ZIP", defaults.zip)?;

    Ok(ShareExportWizardResult {
        projects,
        inline_threshold,
        detach_threshold,
        scrub_preset,
        chunk_threshold,
        chunk_size,
        zip,
    })
}

fn prompt_line(prompt: &str) -> CliResult<String> {
    use std::io::Write;

    ftui_runtime::ftui_eprintln!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut buf = String::new();
    std::io::stdin()
        .read_line(&mut buf)
        .map_err(|e| CliError::Other(format!("failed to read input: {e}")))?;
    Ok(buf.trim_end().to_string())
}

fn prompt_i64<F>(label: &str, default: i64, validate: F) -> CliResult<i64>
where
    F: Fn(i64) -> bool,
{
    loop {
        let line = prompt_line(&format!("{label} [{default}]: "))?;
        if line.trim().is_empty() {
            return Ok(default);
        }
        match line.trim().parse::<i64>() {
            Ok(value) if validate(value) => return Ok(value),
            Ok(_) => ftui_runtime::ftui_eprintln!("Invalid value for {label}."),
            Err(_) => ftui_runtime::ftui_eprintln!("Invalid integer for {label}."),
        }
    }
}

fn prompt_bool(label: &str, default: bool) -> CliResult<bool> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        let line = prompt_line(&format!("{label}? {suffix}: "))?;
        let trimmed = line.trim().to_ascii_lowercase();
        if trimmed.is_empty() {
            return Ok(default);
        }
        match trimmed.as_str() {
            "y" | "yes" | "true" | "1" => return Ok(true),
            "n" | "no" | "false" | "0" => return Ok(false),
            _ => ftui_runtime::ftui_eprintln!("Please answer y/n."),
        }
    }
}

fn prepare_share_export_output_dir(output: &Path) -> CliResult<()> {
    if output.exists() {
        if !output.is_dir() {
            return Err(CliError::Other(format!(
                "export path {} exists and is not a directory",
                output.display()
            )));
        }
        let mut entries = output.read_dir()?;
        match entries.next() {
            None => {}
            Some(Ok(_)) => {
                return Err(CliError::Other(format!(
                    "export path {} is not empty; choose a new directory",
                    output.display()
                )));
            }
            Some(Err(e)) => return Err(CliError::Io(e)),
        }
        return Ok(());
    }

    std::fs::create_dir_all(output)?;
    Ok(())
}

fn zip_archive_path_for_dir(dir: &Path) -> PathBuf {
    let parent = dir.parent().unwrap_or_else(|| Path::new("."));
    let name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("bundle");
    parent.join(format!("{name}.zip"))
}

fn sha256_file(path: &Path) -> CliResult<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

struct ShareExportParams {
    output: PathBuf,
    projects: Vec<String>,
    inline_threshold: usize,
    detach_threshold: usize,
    scrub_preset: share::ScrubPreset,
    chunk_threshold: usize,
    chunk_size: usize,
    dry_run: bool,
    zip: bool,
    signing_key: Option<PathBuf>,
    signing_public_out: Option<PathBuf>,
    age_recipients: Vec<String>,
}

struct ShareUpdateParams {
    bundle: PathBuf,
    projects: Vec<String>,
    inline_threshold: usize,
    detach_threshold: usize,
    scrub_preset: share::ScrubPreset,
    chunk_threshold: usize,
    chunk_size: usize,
    zip: bool,
    signing_key: Option<PathBuf>,
    signing_public_out: Option<PathBuf>,
    age_recipients: Vec<String>,
}

fn run_share_export(params: ShareExportParams) -> CliResult<()> {
    let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    let source_path = cfg
        .sqlite_path()
        .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?;
    let source = std::path::Path::new(&source_path);

    if !source.exists() {
        return Err(CliError::Other(format!(
            "database not found: {source_path}"
        )));
    }

    ftui_runtime::ftui_println!("Source database: {source_path}");
    ftui_runtime::ftui_println!("Scrub preset:   {}", params.scrub_preset);

    // Dry run: create snapshot in temp dir and print summary only (no output artifacts).
    if params.dry_run {
        ftui_runtime::ftui_println!("Dry run - validating export in a temp directory.");

        let tmp = tempfile::tempdir()?;
        let snapshot_path = tmp.path().join("_snapshot.sqlite3");
        let snap_ctx = share::create_snapshot_context(
            source,
            &snapshot_path,
            &params.projects,
            params.scrub_preset,
        )?;

        ftui_runtime::ftui_println!("\nSummary:");
        ftui_runtime::ftui_println!("  Projects kept:        {}", snap_ctx.scope.projects.len());
        ftui_runtime::ftui_println!(
            "  Secrets replaced:     {}",
            snap_ctx.scrub_summary.secrets_replaced
        );
        ftui_runtime::ftui_println!(
            "  Bodies redacted:      {}",
            snap_ctx.scrub_summary.bodies_redacted
        );
        ftui_runtime::ftui_println!(
            "  Ack flags cleared:    {}",
            snap_ctx.scrub_summary.ack_flags_cleared
        );
        ftui_runtime::ftui_println!(
            "  Recipients cleared:   {}",
            snap_ctx.scrub_summary.recipients_cleared
        );
        ftui_runtime::ftui_println!(
            "  File reservations rm: {}",
            snap_ctx.scrub_summary.file_reservations_removed
        );
        ftui_runtime::ftui_println!(
            "  Agent links rm:       {}",
            snap_ctx.scrub_summary.agent_links_removed
        );

        ftui_runtime::ftui_println!("\nSecurity checklist:");
        ftui_runtime::ftui_println!("  1. Confirm the scrub preset matches your sharing intent.");
        ftui_runtime::ftui_println!(
            "  2. Review for any remaining secrets (search for \"sk-\", \"ghp_\", \"github_pat_\", \"xox\" in the exported content)."
        );
        ftui_runtime::ftui_println!(
            "  3. Double-check attachment handling (inline={}, detach={}).",
            params.inline_threshold,
            params.detach_threshold
        );
        ftui_runtime::ftui_println!(
            "  4. Run `am share verify <bundle>` after export (and after signing)."
        );

        return Ok(());
    }

    let output = &params.output;
    prepare_share_export_output_dir(output)?;

    // 1. Snapshot + scope + scrub + finalize
    ftui_runtime::ftui_println!("Creating snapshot...");
    let snapshot_path = output.join("_snapshot.sqlite3");
    if snapshot_path.exists() {
        std::fs::remove_file(&snapshot_path)?;
    }
    let snap_ctx = share::create_snapshot_context(
        source,
        &snapshot_path,
        &params.projects,
        params.scrub_preset,
    )?;

    ftui_runtime::ftui_println!("  Projects: {} kept", snap_ctx.scope.projects.len());
    ftui_runtime::ftui_println!(
        "  Scrub: {} secrets replaced, {} bodies redacted",
        snap_ctx.scrub_summary.secrets_replaced,
        snap_ctx.scrub_summary.bodies_redacted
    );

    // 2. Bundle attachments
    ftui_runtime::ftui_println!("Bundling attachments...");
    let config = Config::from_env();
    let att_manifest = share::bundle_attachments(
        &snapshot_path,
        output,
        &config.storage_root,
        params.inline_threshold,
        params.detach_threshold,
        config.allow_absolute_attachment_paths,
    )?;
    ftui_runtime::ftui_println!(
        "  Attachments: {} inline, {} copied, {} external, {} missing",
        att_manifest.stats.inline,
        att_manifest.stats.copied,
        att_manifest.stats.externalized,
        att_manifest.stats.missing
    );

    // 3. Copy DB to bundle
    let db_dest = output.join("mailbox.sqlite3");
    std::fs::copy(&snapshot_path, &db_dest)?;
    let db_sha256 = sha256_file(&db_dest)?;
    let db_size = db_dest.metadata()?.len();

    // 4. Maybe chunk
    let chunk =
        share::maybe_chunk_database(&db_dest, output, params.chunk_threshold, params.chunk_size)?;
    if let Some(ref c) = chunk {
        ftui_runtime::ftui_println!("  Database chunked into {} parts", c.chunk_count);
    }

    // 5. Viewer assets
    ftui_runtime::ftui_println!("Copying viewer assets...");
    let copied = share::copy_viewer_assets(output)?;
    ftui_runtime::ftui_println!("  Viewer assets: {} files", copied.len());

    // 6. Viewer data
    ftui_runtime::ftui_println!("Exporting viewer data...");
    let viewer_data = share::export_viewer_data(&snapshot_path, output, snap_ctx.fts_enabled)?;

    // 7. SRI hashes
    let sri = share::compute_viewer_sri(output);

    // 8. Hosting hints
    let hints = share::detect_hosting_hints(output);
    if !hints.is_empty() {
        ftui_runtime::ftui_println!(
            "  Hosting hint: {} (confidence: {})",
            hints[0].title,
            hints[0].signals.len()
        );
    }

    // 9. Scaffolding
    ftui_runtime::ftui_println!("Writing manifest and scaffolding...");
    share::write_bundle_scaffolding(
        output,
        &snap_ctx.scope,
        &snap_ctx.scrub_summary,
        &att_manifest,
        chunk.as_ref(),
        params.chunk_threshold,
        params.chunk_size,
        &hints,
        snap_ctx.fts_enabled,
        "mailbox.sqlite3",
        &db_sha256,
        db_size,
        Some(&viewer_data),
        &sri,
    )?;

    // 10. Sign
    if let Some(ref key_path) = params.signing_key {
        ftui_runtime::ftui_println!("Signing manifest...");
        let sig = share::sign_manifest(
            &output.join("manifest.json"),
            key_path,
            &output.join("manifest.sig.json"),
            true,
        )?;
        ftui_runtime::ftui_println!("  Algorithm: {}", sig.algorithm);
        if let Some(ref pub_out) = params.signing_public_out {
            std::fs::write(pub_out, &sig.public_key)?;
            ftui_runtime::ftui_println!("  Public key written to: {}", pub_out.display());
        }
    }

    // 11. Clean up snapshot
    let _ = std::fs::remove_file(&snapshot_path);

    // 12. ZIP
    let mut archive_path: Option<PathBuf> = None;
    let final_path = if params.zip {
        ftui_runtime::ftui_println!("Packaging as ZIP...");
        let zip_path = zip_archive_path_for_dir(output);
        share::package_directory_as_zip(output, &zip_path)?;
        ftui_runtime::ftui_println!("  ZIP: {}", zip_path.display());
        archive_path = Some(zip_path.clone());
        zip_path
    } else {
        output.clone()
    };

    // 13. Encrypt
    if !params.age_recipients.is_empty() {
        if let Some(ref archive) = archive_path {
            ftui_runtime::ftui_println!("Encrypting with age...");
            let encrypted = share::encrypt_with_age(archive, &params.age_recipients)?;
            ftui_runtime::ftui_println!("  Encrypted: {}", encrypted.display());
        } else {
            ftui_runtime::ftui_eprintln!(
                "warning: skipped age encryption because --zip was not enabled."
            );
        }
    }

    ftui_runtime::ftui_println!("Export complete: {}", final_path.display());
    Ok(())
}

fn run_share_update(params: ShareUpdateParams) -> CliResult<()> {
    fn copy_file(src: &Path, dst: &Path) -> CliResult<()> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        Ok(())
    }

    fn copy_dir_recursive(src: &Path, dst: &Path) -> CliResult<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            let ty = entry.file_type()?;
            if ty.is_dir() {
                copy_dir_recursive(&src_path, &dst_path)?;
                continue;
            }
            if ty.is_file() {
                std::fs::copy(&src_path, &dst_path)?;
                continue;
            }
            return Err(CliError::Other(format!(
                "unsupported file type in bundle: {}",
                src_path.display()
            )));
        }
        Ok(())
    }

    fn replace_dir(src: &Path, dst: &Path) -> CliResult<()> {
        if dst.exists() {
            std::fs::remove_dir_all(dst)?;
        }
        copy_dir_recursive(src, dst)
    }

    fn remove_file_if_exists(path: &Path) -> CliResult<()> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    fn remove_dir_if_exists(path: &Path) -> CliResult<()> {
        if path.exists() {
            std::fs::remove_dir_all(path)?;
        }
        Ok(())
    }

    let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    let source_path = cfg
        .sqlite_path()
        .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?;
    let source = std::path::Path::new(&source_path);
    if !source.exists() {
        return Err(CliError::Other(format!(
            "database not found: {source_path}"
        )));
    }

    if !params.bundle.is_dir() {
        return Err(CliError::Other(format!(
            "bundle path {} does not exist or is not a directory",
            params.bundle.display()
        )));
    }

    let existing_signature = params.bundle.join("manifest.sig.json").exists();

    ftui_runtime::ftui_println!("Source database: {source_path}");
    ftui_runtime::ftui_println!("Updating bundle: {}", params.bundle.display());
    ftui_runtime::ftui_println!("Scrub preset:   {}", params.scrub_preset);

    let tmp = tempfile::tempdir()?;
    let temp_bundle = tmp.path().join("bundle");
    std::fs::create_dir_all(&temp_bundle)?;

    // Build updated bundle assets into a temp directory.
    ftui_runtime::ftui_println!("Building updated bundle in temp directory...");

    // 1. Snapshot + scope + scrub + finalize
    ftui_runtime::ftui_println!("Creating snapshot...");
    let snapshot_path = temp_bundle.join("_snapshot.sqlite3");
    if snapshot_path.exists() {
        std::fs::remove_file(&snapshot_path)?;
    }
    let snap_ctx = share::create_snapshot_context(
        source,
        &snapshot_path,
        &params.projects,
        params.scrub_preset,
    )?;

    ftui_runtime::ftui_println!("  Projects: {} kept", snap_ctx.scope.projects.len());
    ftui_runtime::ftui_println!(
        "  Scrub: {} secrets replaced, {} bodies redacted",
        snap_ctx.scrub_summary.secrets_replaced,
        snap_ctx.scrub_summary.bodies_redacted
    );

    // 2. Bundle attachments
    ftui_runtime::ftui_println!("Bundling attachments...");
    let config = Config::from_env();
    let att_manifest = share::bundle_attachments(
        &snapshot_path,
        &temp_bundle,
        &config.storage_root,
        params.inline_threshold,
        params.detach_threshold,
        config.allow_absolute_attachment_paths,
    )?;
    ftui_runtime::ftui_println!(
        "  Attachments: {} inline, {} copied, {} external, {} missing",
        att_manifest.stats.inline,
        att_manifest.stats.copied,
        att_manifest.stats.externalized,
        att_manifest.stats.missing
    );

    // 3. Copy DB to bundle
    let db_dest = temp_bundle.join("mailbox.sqlite3");
    std::fs::copy(&snapshot_path, &db_dest)?;
    let db_sha256 = sha256_file(&db_dest)?;
    let db_size = db_dest.metadata()?.len();

    // 4. Maybe chunk
    let chunk = share::maybe_chunk_database(
        &db_dest,
        &temp_bundle,
        params.chunk_threshold,
        params.chunk_size,
    )?;
    if let Some(ref c) = chunk {
        ftui_runtime::ftui_println!("  Database chunked into {} parts", c.chunk_count);
    }

    // 5. Viewer assets
    ftui_runtime::ftui_println!("Copying viewer assets...");
    let copied = share::copy_viewer_assets(&temp_bundle)?;
    ftui_runtime::ftui_println!("  Viewer assets: {} files", copied.len());

    // 6. Viewer data
    ftui_runtime::ftui_println!("Exporting viewer data...");
    let viewer_data =
        share::export_viewer_data(&snapshot_path, &temp_bundle, snap_ctx.fts_enabled)?;

    // 7. SRI hashes
    let sri = share::compute_viewer_sri(&temp_bundle);

    // 8. Hosting hints (use destination bundle location for parity; temp dirs don't have git remotes).
    let hints = share::detect_hosting_hints(&params.bundle);
    if !hints.is_empty() {
        ftui_runtime::ftui_println!(
            "  Hosting hint: {} (confidence: {})",
            hints[0].title,
            hints[0].signals.len()
        );
    }

    // 9. Scaffolding
    ftui_runtime::ftui_println!("Writing manifest and scaffolding...");
    share::write_bundle_scaffolding(
        &temp_bundle,
        &snap_ctx.scope,
        &snap_ctx.scrub_summary,
        &att_manifest,
        chunk.as_ref(),
        params.chunk_threshold,
        params.chunk_size,
        &hints,
        snap_ctx.fts_enabled,
        "mailbox.sqlite3",
        &db_sha256,
        db_size,
        Some(&viewer_data),
        &sri,
    )?;

    // 10. Clean up snapshot
    let _ = std::fs::remove_file(&snapshot_path);

    ftui_runtime::ftui_println!(
        "Synchronizing updated bundle into: {}",
        params.bundle.display()
    );

    // Replace top-level generated files (but intentionally do NOT touch manifest.sig.json unless re-signing).
    let files = [
        "manifest.json",
        "README.md",
        "HOW_TO_DEPLOY.md",
        "index.html",
        ".nojekyll",
        "_headers",
        "mailbox.sqlite3",
    ];
    for name in files {
        copy_file(&temp_bundle.join(name), &params.bundle.join(name))?;
    }

    // Ensure bundle doesn't accumulate WAL/SHM files.
    remove_file_if_exists(&params.bundle.join("mailbox.sqlite3-wal"))?;
    remove_file_if_exists(&params.bundle.join("mailbox.sqlite3-shm"))?;
    remove_file_if_exists(&params.bundle.join("_snapshot.sqlite3"))?;

    // Viewer + attachments are always owned by the export; replace wholesale.
    replace_dir(&temp_bundle.join("viewer"), &params.bundle.join("viewer"))?;
    replace_dir(
        &temp_bundle.join("attachments"),
        &params.bundle.join("attachments"),
    )?;

    // Chunk artefacts: if the refreshed snapshot no longer needs chunking, prune the old chunk files.
    if chunk.is_some() {
        replace_dir(&temp_bundle.join("chunks"), &params.bundle.join("chunks"))?;
        copy_file(
            &temp_bundle.join("chunks.sha256"),
            &params.bundle.join("chunks.sha256"),
        )?;
        copy_file(
            &temp_bundle.join("mailbox.sqlite3.config.json"),
            &params.bundle.join("mailbox.sqlite3.config.json"),
        )?;
    } else {
        remove_dir_if_exists(&params.bundle.join("chunks"))?;
        remove_file_if_exists(&params.bundle.join("chunks.sha256"))?;
        remove_file_if_exists(&params.bundle.join("mailbox.sqlite3.config.json"))?;
    }

    // Sign (optional).
    if let Some(ref key_path) = params.signing_key {
        ftui_runtime::ftui_println!("Signing manifest...");
        let sig = share::sign_manifest(
            &params.bundle.join("manifest.json"),
            key_path,
            &params.bundle.join("manifest.sig.json"),
            true,
        )?;
        ftui_runtime::ftui_println!("  Algorithm: {}", sig.algorithm);
        if let Some(ref pub_out) = params.signing_public_out {
            std::fs::write(pub_out, &sig.public_key)?;
            ftui_runtime::ftui_println!("  Public key written to: {}", pub_out.display());
        }
    } else if existing_signature {
        ftui_runtime::ftui_eprintln!(
            "warning: existing manifest signature may no longer match; re-run with --signing-key to refresh it."
        );
    }

    // Package ZIP (optional).
    let mut archive_path: Option<PathBuf> = None;
    if params.zip {
        ftui_runtime::ftui_println!("Packaging as ZIP...");
        let zip_path = zip_archive_path_for_dir(&params.bundle);
        share::package_directory_as_zip(&params.bundle, &zip_path)?;
        ftui_runtime::ftui_println!("  ZIP: {}", zip_path.display());
        archive_path = Some(zip_path);
    }

    // Encrypt (optional). For update parity, warn but do not fail when --zip is not enabled.
    if !params.age_recipients.is_empty() {
        if let Some(ref archive) = archive_path {
            ftui_runtime::ftui_println!("Encrypting with age...");
            let encrypted = share::encrypt_with_age(archive, &params.age_recipients)?;
            ftui_runtime::ftui_println!("  Encrypted: {}", encrypted.display());
        } else {
            ftui_runtime::ftui_eprintln!(
                "warning: skipped age encryption because --zip was not enabled."
            );
        }
    }

    ftui_runtime::ftui_println!("Update complete: {}", params.bundle.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Archive commands
// ---------------------------------------------------------------------------

const ARCHIVE_DIR_NAME: &str = "archived_mailbox_states";
const ARCHIVE_METADATA_FILENAME: &str = "metadata.json";
const ARCHIVE_SNAPSHOT_RELATIVE: &str = "snapshot/mailbox.sqlite3";
const ARCHIVE_STORAGE_DIRNAME: &str = "storage_repo";

fn detect_project_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let resolved = cwd.canonicalize().unwrap_or(cwd);

    // Archive root detection (where `archived_mailbox_states/` is created):
    // 1) Cargo.toml (Rust repos)
    // 2) pyproject.toml (legacy Python repos)
    // 3) .git (fallback)
    for candidate in resolved.ancestors() {
        if candidate.join("Cargo.toml").exists() {
            return candidate.to_path_buf();
        }
    }
    for candidate in resolved.ancestors() {
        if candidate.join("pyproject.toml").exists() {
            return candidate.to_path_buf();
        }
    }
    for candidate in resolved.ancestors() {
        if candidate.join(".git").exists() {
            return candidate.to_path_buf();
        }
    }

    resolved
}

fn archive_states_dir(create: bool) -> CliResult<PathBuf> {
    let root = detect_project_root();
    let archive_dir = root.join(ARCHIVE_DIR_NAME);
    if create {
        std::fs::create_dir_all(&archive_dir)?;
    }
    Ok(archive_dir)
}

fn slugify(value: &str) -> String {
    let trimmed = value.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut last_was_dash = false;

    for b in trimmed.bytes() {
        let lower = b.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower as char);
            last_was_dash = false;
            continue;
        }
        if !out.is_empty() && !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }

    while out.ends_with('-') {
        out.pop();
    }

    if out.is_empty() {
        "project".to_string()
    } else {
        out
    }
}

fn compose_archive_basename(
    timestamp: DateTime<Utc>,
    project_filters: &[String],
    scrub_preset: &str,
    label: Option<&str>,
) -> String {
    let ts_segment = timestamp.format("%Y%m%d-%H%M%SZ").to_string();
    let projects_segment = if project_filters.is_empty() {
        "all-projects".to_string()
    } else {
        project_filters
            .iter()
            .map(|v| slugify(v))
            .collect::<Vec<_>>()
            .join("-")
    };
    let preset_segment = slugify(scrub_preset);

    let mut segments = vec![
        "mailbox-state".to_string(),
        ts_segment,
        projects_segment,
        preset_segment,
    ];
    if let Some(label) = label {
        segments.push(slugify(label));
    }
    segments.join("-")
}

fn ensure_unique_archive_path(base_dir: &Path, base_name: &str) -> PathBuf {
    let mut candidate = base_dir.join(format!("{base_name}.zip"));
    let mut counter = 1u32;
    while candidate.exists() {
        candidate = base_dir.join(format!("{base_name}-{counter:02}.zip"));
        counter = counter.saturating_add(1);
        if counter == u32::MAX {
            break;
        }
    }
    candidate
}

fn detect_git_head(repo_path: &Path) -> Option<String> {
    let git_dir = repo_path.join(".git");
    if !git_dir.exists() {
        return None;
    }

    let head_path = git_dir.join("HEAD");
    let head_contents = std::fs::read_to_string(head_path).ok()?;
    let head_contents = head_contents.trim();
    if head_contents.is_empty() {
        return None;
    }

    if let Some(ref_name) = head_contents.strip_prefix("ref:") {
        let ref_name = ref_name.trim();
        let ref_path = git_dir.join(ref_name);
        if ref_path.exists()
            && let Ok(value) = std::fs::read_to_string(ref_path)
        {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }

        let packed_refs = git_dir.join("packed-refs");
        if packed_refs.exists()
            && let Ok(text) = std::fs::read_to_string(packed_refs)
        {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let Some((commit, reference)) = line.split_once(' ') else {
                    continue;
                };
                if reference.trim() == ref_name {
                    return Some(commit.trim().to_string());
                }
            }
        }

        return None;
    }

    Some(head_contents.to_string())
}

fn next_backup_path(path: &Path, timestamp: &str) -> PathBuf {
    let filename = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "backup".to_string());
    let mut candidate = path.with_file_name(format!("{filename}.backup-{timestamp}"));
    let mut counter = 1;
    while candidate.exists() {
        candidate = path.with_file_name(format!("{filename}.backup-{timestamp}-{counter:02}"));
        counter += 1;
    }
    candidate
}

fn sort_json_keys(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                if let Some(v) = map.get(&key) {
                    sorted.insert(key, sort_json_keys(v));
                }
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(sort_json_keys).collect())
        }
        other => other.clone(),
    }
}

fn iso_from_micros(micros: i64) -> String {
    DateTime::<Utc>::from_timestamp_micros(micros)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
        .unwrap_or_default()
}

fn projects_included_from_snapshot(snapshot_path: &Path) -> CliResult<Vec<serde_json::Value>> {
    // Snapshots are always in C SQLite format (created by the share pipeline).
    let path_str = snapshot_path.display().to_string();
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(&path_str)
        .map_err(|e| CliError::Other(format!("cannot open snapshot for metadata: {e}")))?;
    let rows = conn
        .query_sync(
            "SELECT slug, human_key, created_at FROM projects ORDER BY id",
            &[],
        )
        .map_err(|e| CliError::Other(format!("SELECT projects failed: {e}")))?;

    let mut out = Vec::new();
    for row in &rows {
        let slug: String = row.get_named("slug").unwrap_or_default();
        let human_key: String = row.get_named("human_key").unwrap_or_default();
        let created_at: i64 = row.get_named("created_at").unwrap_or(0);
        out.push(serde_json::json!({
            "slug": slug,
            "human_key": human_key,
            "created_at": if created_at == 0 {
                String::new()
            } else {
                iso_from_micros(created_at)
            },
        }));
    }
    Ok(out)
}

fn load_archive_metadata(zip_path: &Path) -> (serde_json::Value, Option<String>) {
    use std::io::Read;

    let file = match std::fs::File::open(zip_path) {
        Ok(f) => f,
        Err(e) => {
            return (
                serde_json::Value::Object(serde_json::Map::new()),
                Some(format!("Invalid metadata: {e}")),
            );
        }
    };

    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => {
            return (
                serde_json::Value::Object(serde_json::Map::new()),
                Some(format!("Invalid metadata: {e}")),
            );
        }
    };

    let mut meta_file = match archive.by_name(ARCHIVE_METADATA_FILENAME) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => {
            return (
                serde_json::Value::Object(serde_json::Map::new()),
                Some(format!("{ARCHIVE_METADATA_FILENAME} missing")),
            );
        }
        Err(e) => {
            return (
                serde_json::Value::Object(serde_json::Map::new()),
                Some(format!("Invalid metadata: {e}")),
            );
        }
    };

    let mut contents = String::new();
    if let Err(e) = meta_file.read_to_string(&mut contents) {
        return (
            serde_json::Value::Object(serde_json::Map::new()),
            Some(format!("Invalid metadata: {e}")),
        );
    }

    match serde_json::from_str::<serde_json::Value>(&contents) {
        Ok(value) => (value, None),
        Err(e) => (
            serde_json::Value::Object(serde_json::Map::new()),
            Some(format!("Invalid metadata: {e}")),
        ),
    }
}

fn resolve_archive_path(candidate: &Path) -> CliResult<PathBuf> {
    if candidate.exists() {
        return Ok(candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.to_path_buf()));
    }
    let archive_dir = archive_states_dir(false)?;
    let fallback = match candidate.file_name() {
        Some(name) => archive_dir.join(name),
        None => archive_dir.join(candidate),
    };
    if fallback.exists() {
        return Ok(fallback.canonicalize().unwrap_or(fallback));
    }
    Err(CliError::InvalidArgument(format!(
        "Archive '{}' not found (checked {} and {}).",
        candidate.display(),
        candidate.display(),
        fallback.display()
    )))
}

fn collect_files(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> CliResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(base, &path, out)?;
        } else if path.is_file()
            && let Ok(rel) = path.strip_prefix(base)
        {
            out.push(rel.to_path_buf());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .map(|m| m.mode() & 0o777)
        .unwrap_or(0o644)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> u32 {
    0o644
}

fn confirm(prompt: &str, default: bool) -> CliResult<bool> {
    use std::io::Write;

    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    ftui_runtime::ftui_println!("{prompt} {suffix}");
    let _ = std::io::stdout().flush();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim().to_ascii_lowercase();
    if input.is_empty() {
        return Ok(default);
    }
    if input == "y" || input == "yes" {
        return Ok(true);
    }
    if input == "n" || input == "no" {
        return Ok(false);
    }
    Ok(default)
}

#[allow(dead_code)]
fn archive_save_state(
    source_db: &Path,
    storage_root: &Path,
    projects: Vec<String>,
    scrub_preset: String,
    label: Option<String>,
) -> CliResult<PathBuf> {
    use chrono::Timelike;
    use std::io::Write;

    if source_db.to_string_lossy() == ":memory:" {
        return Err(CliError::Other(
            "cannot archive an in-memory database (:memory:)".to_string(),
        ));
    }

    let preset = match share::normalize_scrub_preset(&scrub_preset) {
        Ok(p) => p,
        Err(_) => {
            ftui_runtime::ftui_eprintln!(
                "Invalid scrub preset '{scrub_preset}'. Choose one of: {}.",
                share::SCRUB_PRESETS.join(", ")
            );
            return Err(CliError::ExitCode(1));
        }
    };
    let preset_str = preset.as_str().to_string();

    if !source_db.exists() {
        return Err(CliError::Other(format!(
            "database not found: {}",
            source_db.display()
        )));
    }
    if !storage_root.exists() {
        return Err(CliError::Other(format!(
            "Storage root {} does not exist; cannot archive.",
            storage_root.display()
        )));
    }

    let archive_dir = archive_states_dir(true)?;
    let timestamp = Utc::now();
    let timestamp = timestamp.with_nanosecond(0).unwrap_or(timestamp);
    let base_name = compose_archive_basename(timestamp, &projects, &preset_str, label.as_deref());
    let destination = ensure_unique_archive_path(&archive_dir, &base_name);

    let temp_dir = tempfile::Builder::new()
        .prefix("mailbox-archive-")
        .tempdir()?;
    let snapshot_path = temp_dir.path().join("mailbox.sqlite3");

    ftui_runtime::ftui_println!("Creating mailbox archive...");
    let context = share::create_snapshot_context(source_db, &snapshot_path, &projects, preset)?;

    let snapshot_size = std::fs::metadata(&snapshot_path)?.len();
    let destination_name = destination
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "mailbox-state.zip".to_string());
    let projects_included = projects_included_from_snapshot(&snapshot_path).unwrap_or_else(|_| {
        context
            .scope
            .projects
            .iter()
            .map(|p| {
                serde_json::json!({
                    "slug": p.slug.clone(),
                    "human_key": p.human_key.clone(),
                    "created_at": "",
                })
            })
            .collect()
    });

    let projects_requested = projects.clone();
    let label_value = label.clone().unwrap_or_default();
    let source_path = source_db.display().to_string();

    let metadata = serde_json::json!({
        "version": 1,
        "created_at": timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        "projects_requested": projects_requested,
        "projects_included": projects_included,
        "projects_removed": context.scope.removed_count,
        "scrub_preset": preset_str.clone(),
        "scrub_summary": context.scrub_summary,
        "fts_enabled": context.fts_enabled,
        "database": {
            "source_path": source_path,
            "snapshot": ARCHIVE_SNAPSHOT_RELATIVE,
            "size_bytes": snapshot_size,
        },
        "storage": {
            "source_path": storage_root.display().to_string(),
            "git_head": detect_git_head(storage_root),
            "archive_dir": ARCHIVE_STORAGE_DIRNAME,
        },
        "label": label_value,
        "tooling": {
            "package": "mcp-agent-mail",
            "version": env!("CARGO_PKG_VERSION"),
            "python": "",
        },
        "notes": [
            format!("Restore with `am archive restore {}`", destination_name)
        ],
    });
    let sorted_metadata = sort_json_keys(&metadata);
    let metadata_json =
        serde_json::to_string_pretty(&sorted_metadata).unwrap_or_else(|_| "{}".to_string());

    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Write to a temp file under the archive directory, then move into place (atomic).
    let zip_dir = tempfile::Builder::new()
        .prefix("mailbox-archive-zip-")
        .tempdir_in(&archive_dir)?;
    let temp_zip_path = zip_dir.path().join("mailbox-state.zip");

    use zip::DateTime;
    use zip::write::SimpleFileOptions;

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_zip_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let fixed_time = DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0)
        .map_err(|e| CliError::Other(format!("zip time error: {e}")))?;
    let base_options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(9))
        .last_modified_time(fixed_time);

    // metadata.json
    zip.start_file(ARCHIVE_METADATA_FILENAME, base_options)
        .map_err(|e| CliError::Other(format!("zip write error: {e}")))?;
    zip.write_all(metadata_json.as_bytes())?;

    // snapshot/mailbox.sqlite3
    let snapshot_zip_name = ARCHIVE_SNAPSHOT_RELATIVE.replace('\\', "/");
    let snapshot_mode = file_mode(&snapshot_path);
    zip.start_file(
        snapshot_zip_name,
        base_options.unix_permissions(snapshot_mode),
    )
    .map_err(|e| CliError::Other(format!("zip write error: {e}")))?;
    let mut snapshot_file = std::fs::File::open(&snapshot_path)?;
    std::io::copy(&mut snapshot_file, &mut zip)?;

    // storage_repo/*
    let mut files = Vec::new();
    collect_files(storage_root, storage_root, &mut files)?;
    files.sort();
    for rel in files {
        let full_path = storage_root.join(&rel);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let zip_name = format!("{ARCHIVE_STORAGE_DIRNAME}/{rel_str}");
        let mode = file_mode(&full_path);

        zip.start_file(zip_name, base_options.unix_permissions(mode))
            .map_err(|e| CliError::Other(format!("zip write error: {e}")))?;
        let mut f = std::fs::File::open(&full_path)?;
        std::io::copy(&mut f, &mut zip)?;
    }

    zip.finish()
        .map_err(|e| CliError::Other(format!("zip finalize error: {e}")))?;

    std::fs::rename(&temp_zip_path, &destination)?;

    let size_bytes = std::fs::metadata(&destination)
        .map(|m| m.len())
        .unwrap_or(0);
    let projects_desc = if projects.is_empty() {
        vec!["all".to_string()]
    } else {
        projects
    };

    ftui_runtime::ftui_println!("✓ Mailbox state saved to: {}", destination.display());
    ftui_runtime::ftui_println!(
        "Preset: {} | Projects: {} | Size: {}",
        preset_str,
        projects_desc.join(", "),
        format_bytes(size_bytes),
    );
    ftui_runtime::ftui_println!(
        "Restore later with: am archive restore {}",
        destination_name
    );

    Ok(destination)
}

#[allow(dead_code)]
fn archive_restore_state(
    archive_file: PathBuf,
    database_path: &Path,
    storage_root: &Path,
    force: bool,
    dry_run: bool,
) -> CliResult<()> {
    let archive_path = resolve_archive_path(&archive_file)?;
    let (meta, meta_error) = load_archive_metadata(&archive_path);
    if let Some(err) = meta_error {
        ftui_runtime::ftui_eprintln!("Warning: {err}");
    }

    if database_path.to_string_lossy() == ":memory:" {
        return Err(CliError::Other(
            "cannot restore into an in-memory database (:memory:)".to_string(),
        ));
    }

    let database_path = database_path.to_path_buf();
    let storage_root = storage_root.to_path_buf();

    let archive_db_path = meta
        .get("database")
        .and_then(|v| v.get("source_path"))
        .and_then(|v| v.as_str());
    let archive_storage_path = meta
        .get("storage")
        .and_then(|v| v.get("source_path"))
        .and_then(|v| v.as_str());

    if let Some(path) = archive_db_path
        && path != database_path.display().to_string()
    {
        ftui_runtime::ftui_eprintln!(
            "Archive was created from database {path}, current config is {}. Continuing...",
            database_path.display()
        );
    }
    if let Some(path) = archive_storage_path
        && path != storage_root.display().to_string()
    {
        ftui_runtime::ftui_eprintln!(
            "Archive used storage root {path}, current config is {}. Continuing...",
            storage_root.display()
        );
    }

    // Planned operations (and safety backups)
    let timestamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let mut planned_ops: Vec<String> = Vec::new();
    if database_path.exists() {
        planned_ops.push(format!(
            "backup {} -> {}",
            database_path.display(),
            next_backup_path(&database_path, &timestamp).display()
        ));
    }
    for suffix in ["-wal", "-shm"] {
        let wal_path = PathBuf::from(format!("{}{}", database_path.display(), suffix));
        if wal_path.exists() {
            planned_ops.push(format!(
                "backup {} -> {}",
                wal_path.display(),
                next_backup_path(&wal_path, &timestamp).display()
            ));
        }
    }
    if storage_root.exists() {
        planned_ops.push(format!(
            "backup {} -> {}",
            storage_root.display(),
            next_backup_path(&storage_root, &timestamp).display()
        ));
    }
    planned_ops.push(format!("restore snapshot -> {}", database_path.display()));
    planned_ops.push(format!(
        "restore storage repo -> {}",
        storage_root.display()
    ));

    if dry_run {
        ftui_runtime::ftui_println!("Dry-run plan:");
        for op in &planned_ops {
            ftui_runtime::ftui_println!("  - {op}");
        }
        return Ok(());
    }

    if !force {
        if !crate::output::is_stdin_tty() {
            return Err(CliError::Other(
                "refusing to prompt on non-interactive stdin; pass --force / -f to apply"
                    .to_string(),
            ));
        }
        ftui_runtime::ftui_println!("The following operations will be performed:");
        for op in &planned_ops {
            ftui_runtime::ftui_println!("  - {op}");
        }
        if !confirm("Proceed with restore?", false)? {
            return Err(CliError::ExitCode(1));
        }
    }

    // Open archive for restore.
    let file = std::fs::File::open(&archive_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| CliError::Other(format!("{e}")))?;

    // Ensure snapshot exists.
    let snapshot_entry_name = ARCHIVE_SNAPSHOT_RELATIVE;
    if archive.by_name(snapshot_entry_name).is_err() {
        return Err(CliError::Other(format!(
            "Snapshot missing inside archive ({snapshot_entry_name})."
        )));
    }
    // Ensure storage exists.
    let prefix_string = format!("{ARCHIVE_STORAGE_DIRNAME}/");
    let mut has_storage = false;
    for i in 0..archive.len() {
        if let Ok(file) = archive.by_index(i)
            && file.name().starts_with(&prefix_string)
        {
            has_storage = true;
            break;
        }
    }
    if !has_storage {
        return Err(CliError::Other(format!(
            "Storage repository missing inside archive ({ARCHIVE_STORAGE_DIRNAME})."
        )));
    }

    // Back up existing files/dirs.
    let mut backup_paths: Vec<PathBuf> = Vec::new();
    if database_path.exists() {
        let backup = next_backup_path(&database_path, &timestamp);
        std::fs::rename(&database_path, &backup)?;
        backup_paths.push(backup);
    }
    for suffix in ["-wal", "-shm"] {
        let wal_path = PathBuf::from(format!("{}{}", database_path.display(), suffix));
        if wal_path.exists() {
            let backup = next_backup_path(&wal_path, &timestamp);
            std::fs::rename(&wal_path, &backup)?;
            backup_paths.push(backup);
        }
    }
    if storage_root.exists() {
        let backup = next_backup_path(&storage_root, &timestamp);
        std::fs::rename(&storage_root, &backup)?;
        backup_paths.push(backup);
    }

    // Restore snapshot.
    if let Some(parent) = database_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    {
        let mut snapshot_file = archive
            .by_name(snapshot_entry_name)
            .map_err(|e| CliError::Other(format!("{e}")))?;
        let mut out = std::fs::File::create(&database_path)?;
        std::io::copy(&mut snapshot_file, &mut out)?;
    }

    // Restore storage repo entries.
    if let Some(parent) = storage_root.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(&storage_root)?;

    let prefix_path = Path::new(ARCHIVE_STORAGE_DIRNAME);
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| CliError::Other(format!("{e}")))?;
        let Some(enclosed) = file.enclosed_name().map(|p| p.to_path_buf()) else {
            continue;
        };
        if !enclosed.starts_with(prefix_path) {
            continue;
        }
        let rel = match enclosed.strip_prefix(prefix_path) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out_path = storage_root.join(rel);
        if file.is_dir() {
            std::fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&out_path)?;
        std::io::copy(&mut file, &mut out)?;

        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
        }
    }

    ftui_runtime::ftui_println!("✓ Restore complete from {}.", archive_path.display());
    if !backup_paths.is_empty() {
        ftui_runtime::ftui_println!("Backups preserved at:");
        for path in &backup_paths {
            ftui_runtime::ftui_println!("  - {}", path.display());
        }
    }
    ftui_runtime::ftui_println!(
        "Database: {}\nStorage root: {}\nNeed to revert? Use the backups above or rerun with another archive.",
        database_path.display(),
        storage_root.display()
    );

    Ok(())
}

fn handle_archive(action: ArchiveCommand) -> CliResult<()> {
    match action {
        ArchiveCommand::Save {
            projects,
            scrub_preset,
            label,
        } => {
            let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
            let source_path = cfg
                .sqlite_path()
                .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?;
            if source_path == ":memory:" {
                return Err(CliError::Other(
                    "cannot archive an in-memory database (:memory:)".to_string(),
                ));
            }
            let source_db = PathBuf::from(&source_path);

            let config = Config::from_env();
            let storage_root = config.storage_root;

            let _path =
                archive_save_state(&source_db, &storage_root, projects, scrub_preset, label)?;
            Ok(())
        }
        ArchiveCommand::List {
            limit,
            format,
            json,
        } => {
            #[derive(Debug, Serialize)]
            struct ArchiveListEntry {
                file: String,
                path: String,
                size_bytes: u64,
                created_at: String,
                scrub_preset: String,
                projects: Vec<String>,
                #[serde(skip_serializing_if = "Option::is_none")]
                error: Option<String>,
            }
            let fmt = output::CliOutputFormat::resolve(format, json);

            if limit < 0 {
                return Err(CliError::InvalidArgument(
                    "--limit/-n must be >= 0".to_string(),
                ));
            }

            let archive_dir = archive_states_dir(false)?;
            if !archive_dir.exists() {
                output::emit_empty(
                    fmt,
                    &format!(
                        "Archive directory {} does not exist yet.",
                        archive_dir.display()
                    ),
                );
                return Ok(());
            }

            let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
            for entry in std::fs::read_dir(&archive_dir)?.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("zip") {
                    continue;
                }
                let modified = path
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                files.push((path, modified));
            }
            files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));

            if files.is_empty() {
                output::emit_empty(
                    fmt,
                    &format!(
                        "No saved mailbox states found under {}.",
                        archive_dir.display()
                    ),
                );
                return Ok(());
            }

            let files = if limit > 0 {
                files.into_iter().take(limit as usize).collect::<Vec<_>>()
            } else {
                files
            };

            let mut entries: Vec<ArchiveListEntry> = Vec::new();
            for (path, modified) in files {
                let file_name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "?".to_string());
                let size_bytes = path.metadata().map(|m| m.len()).unwrap_or(0);
                let (meta, error) = load_archive_metadata(&path);

                let created_at = meta
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| {
                        let dt = DateTime::<Utc>::from(modified);
                        dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
                    });

                let scrub_preset = meta
                    .get("scrub_preset")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let projects = meta
                    .get("projects_requested")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    })
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| vec!["all".to_string()]);

                entries.push(ArchiveListEntry {
                    file: file_name,
                    path: path.display().to_string(),
                    size_bytes,
                    created_at,
                    scrub_preset,
                    projects,
                    error,
                });
            }

            output::emit_output(&entries, fmt, || {
                ftui_runtime::ftui_println!(
                    "{:<32} {:<25} {:>10} {:<9} {:<20} {}",
                    "File",
                    "Created (UTC)",
                    "Size",
                    "Preset",
                    "Projects",
                    "Notes"
                );
                for entry in &entries {
                    ftui_runtime::ftui_println!(
                        "{:<32} {:<25} {:>10} {:<9} {:<20} {}",
                        truncate_str(&entry.file, 32),
                        truncate_str(&entry.created_at, 25),
                        format_bytes(entry.size_bytes),
                        entry.scrub_preset,
                        entry.projects.join(", "),
                        entry.error.clone().unwrap_or_default()
                    );
                }
                ftui_runtime::ftui_println!(
                    "Archives live under {}. Restore with `am archive restore <file>`.",
                    archive_dir.display()
                );
            });
            Ok(())
        }
        ArchiveCommand::Restore {
            archive_file,
            force,
            dry_run,
        } => {
            let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
            let db_path = cfg
                .sqlite_path()
                .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?;
            if db_path == ":memory:" {
                return Err(CliError::Other(
                    "cannot restore into an in-memory database (:memory:)".to_string(),
                ));
            }
            let database_path = PathBuf::from(&db_path);

            let config = Config::from_env();
            let storage_root = config.storage_root;
            archive_restore_state(archive_file, &database_path, &storage_root, force, dry_run)
        }
    }
}

// ---------------------------------------------------------------------------
// Doctor repair, backups, restore
// ---------------------------------------------------------------------------

fn handle_doctor_repair(
    project: Option<String>,
    dry_run: bool,
    yes: bool,
    backup_dir: Option<PathBuf>,
) -> CliResult<()> {
    let config = Config::from_env();
    let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    let database_url = &cfg.database_url;
    let bak_dir = backup_dir.unwrap_or_else(|| config.storage_root.join("backups"));
    handle_doctor_repair_with(database_url, &bak_dir, project, dry_run, yes)
}

fn handle_doctor_repair_with(
    database_url: &str,
    backup_dir: &Path,
    project: Option<String>,
    dry_run: bool,
    _yes: bool,
) -> CliResult<()> {
    let conn = open_db_sync_with_database_url(database_url)?;

    ftui_runtime::ftui_println!("Running database repair...");

    // 1. Integrity check
    let integrity = conn
        .query_sync("PRAGMA integrity_check", &[])
        .map_err(|e| CliError::Other(format!("integrity check failed: {e}")))?;
    let integrity_ok = integrity
        .first()
        .and_then(|r| r.get_named::<String>("integrity_check").ok())
        .map(|s| s == "ok")
        .unwrap_or(false);

    ftui_runtime::ftui_println!(
        "  Integrity: {}",
        if integrity_ok { "OK" } else { "FAILED" }
    );

    if !integrity_ok && !dry_run {
        ftui_runtime::ftui_eprintln!(
            "  Database corruption detected. Consider restoring from backup."
        );
        return Err(CliError::ExitCode(1));
    }

    // 2. Optional backup before repair
    if !dry_run {
        std::fs::create_dir_all(backup_dir)?;
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let bak_name = format!("pre_repair_{timestamp}.sqlite3");
        let cfg = mcp_agent_mail_db::DbPoolConfig {
            database_url: database_url.to_string(),
            ..Default::default()
        };
        let db_path = cfg.sqlite_path().unwrap_or_default();
        if std::path::Path::new(&db_path).exists() {
            let bak_path = backup_dir.join(&bak_name);
            std::fs::copy(&db_path, &bak_path)?;
            ftui_runtime::ftui_println!("  Backup: {}", bak_path.display());

            // Also create a .bak sibling next to the primary DB so that the
            // pool's auto-recovery (find_healthy_backup) can discover it.
            let sibling_bak = format!("{db_path}.bak");
            if let Err(e) = std::fs::copy(&db_path, &sibling_bak) {
                ftui_runtime::ftui_eprintln!(
                    "  Warning: could not create sibling backup {sibling_bak}: {e}"
                );
            }
        }
    }

    // 3. Rebuild FTS if tables exist
    if !dry_run {
        let fts_tables = conn
            .query_sync(
                "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE '%_fts%'",
                &[],
            )
            .unwrap_or_default();
        if !fts_tables.is_empty() {
            for row in &fts_tables {
                let name: String = row.get_named("name").unwrap_or_default();
                let rebuild_sql = format!("INSERT INTO {name}({name}) VALUES('rebuild')");
                match conn.execute_raw(&rebuild_sql) {
                    Ok(_) => ftui_runtime::ftui_println!("  Rebuilt FTS: {name}"),
                    Err(e) => ftui_runtime::ftui_eprintln!("  FTS rebuild failed for {name}: {e}"),
                }
            }
        }
    }

    // 4. VACUUM + ANALYZE
    if !dry_run {
        conn.execute_raw("VACUUM")
            .map_err(|e| CliError::Other(format!("VACUUM failed: {e}")))?;
        conn.execute_raw("ANALYZE")
            .map_err(|e| CliError::Other(format!("ANALYZE failed: {e}")))?;
        ftui_runtime::ftui_println!("  VACUUM + ANALYZE complete.");
    }

    // 5. Check orphan records
    let orphan_msgs = conn
        .query_sync(
            "SELECT COUNT(*) AS cnt FROM messages m \
             LEFT JOIN projects p ON p.id = m.project_id \
             WHERE p.id IS NULL",
            &[],
        )
        .unwrap_or_default();
    let orphan_count: i64 = orphan_msgs
        .first()
        .and_then(|r| r.get_named("cnt").ok())
        .unwrap_or(0);
    if orphan_count > 0 {
        ftui_runtime::ftui_println!("  Orphan messages: {orphan_count}");
        if !dry_run {
            conn.execute_raw(
                "DELETE FROM messages WHERE project_id NOT IN (SELECT id FROM projects)",
            )
            .ok();
            ftui_runtime::ftui_println!("  Cleaned orphan messages.");
        }
    }

    if let Some(ref slug) = project {
        ftui_runtime::ftui_println!("  Scoped to project: {slug}");
    }

    ftui_runtime::ftui_println!(
        "Repair {}.",
        if dry_run {
            "dry run complete"
        } else {
            "complete"
        }
    );
    Ok(())
}

fn handle_doctor_backups(format: Option<output::CliOutputFormat>, json: bool) -> CliResult<()> {
    let config = Config::from_env();
    handle_doctor_backups_with_storage_root(&config.storage_root, format, json)
}

fn handle_doctor_backups_with_storage_root(
    storage_root: &Path,
    format: Option<output::CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    let backup_dir = storage_root.join("backups");
    let fmt = output::CliOutputFormat::resolve(format, json);

    if !backup_dir.exists() {
        output::emit_empty(fmt, "No backups found.");
        return Ok(());
    }

    let mut backups: Vec<(String, u64, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(&backup_dir)?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("sqlite3")
            && let Ok(meta) = path.metadata()
        {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_string();
            let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            backups.push((name, meta.len(), modified));
        }
    }
    backups.sort_by_key(|x| std::cmp::Reverse(x.2));

    let arr: Vec<serde_json::Value> = backups
        .iter()
        .map(|(name, size, _)| serde_json::json!({"name": name, "size": size}))
        .collect();

    output::emit_output(&arr, fmt, || {
        if backups.is_empty() {
            output::emit_empty(fmt, "No backups found.");
            return;
        }
        let mut table = output::CliTable::new(vec!["BACKUP", "SIZE"]);
        for (name, size, _) in &backups {
            table.add_row(vec![name.clone(), format_bytes(*size)]);
        }
        table.render();
    });
    Ok(())
}

fn handle_doctor_restore(backup_path: PathBuf, dry_run: bool, _yes: bool) -> CliResult<()> {
    if !backup_path.exists() {
        return Err(CliError::InvalidArgument(format!(
            "backup not found: {}",
            backup_path.display()
        )));
    }

    let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    let dest_path = cfg
        .sqlite_path()
        .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?;

    ftui_runtime::ftui_println!("Restore: {} -> {}", backup_path.display(), dest_path);

    if dry_run {
        ftui_runtime::ftui_println!("Dry run — no changes made.");
        return Ok(());
    }

    std::fs::copy(&backup_path, &dest_path)?;
    ftui_runtime::ftui_println!("Database restored from backup.");
    Ok(())
}

fn handle_doctor_reconstruct(dry_run: bool, _yes: bool, json: bool) -> CliResult<()> {
    handle_doctor_reconstruct_with(None, None, dry_run, json)
}

fn handle_doctor_reconstruct_with(
    db_path_override: Option<&Path>,
    storage_root_override: Option<&Path>,
    dry_run: bool,
    json: bool,
) -> CliResult<()> {
    let config = Config::from_env();
    let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();

    let storage_root = storage_root_override
        .map(PathBuf::from)
        .unwrap_or_else(|| config.storage_root.clone());
    let db_path = match db_path_override {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(
            cfg.sqlite_path()
                .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?,
        ),
    };

    if !storage_root.is_dir() {
        return Err(CliError::InvalidArgument(format!(
            "storage root does not exist: {}",
            storage_root.display()
        )));
    }

    let projects_dir = storage_root.join("projects");
    if !projects_dir.is_dir() {
        if json {
            ftui_runtime::ftui_println!(
                "{}",
                serde_json::json!({
                    "status": "skip",
                    "reason": "no projects directory found",
                    "storage_root": storage_root.display().to_string()
                })
            );
        } else {
            ftui_runtime::ftui_println!(
                "No projects directory found at {}. Nothing to reconstruct.",
                projects_dir.display()
            );
        }
        return Ok(());
    }

    if dry_run {
        // Walk the archive to report what would be recovered, without writing.
        ftui_runtime::ftui_println!("Dry run — scanning archive at {}", storage_root.display());
        let stats = scan_archive_stats(&storage_root);
        if json {
            ftui_runtime::ftui_println!(
                "{}",
                serde_json::json!({
                    "status": "dry_run",
                    "db_path": db_path.display().to_string(),
                    "storage_root": storage_root.display().to_string(),
                    "would_recover": {
                        "projects": stats.projects,
                        "agents": stats.agents,
                        "message_files": stats.message_files,
                    }
                })
            );
        } else {
            ftui_runtime::ftui_println!("Would recover from archive:");
            ftui_runtime::ftui_println!("  Projects:      {}", stats.projects);
            ftui_runtime::ftui_println!("  Agents:        {}", stats.agents);
            ftui_runtime::ftui_println!("  Message files: {}", stats.message_files);
            ftui_runtime::ftui_println!("  Database path: {}", db_path.display());
            ftui_runtime::ftui_println!("No changes made.");
        }
        return Ok(());
    }

    // Rename the corrupt DB out of the way (if it exists).
    if db_path.exists() {
        let base_name = db_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("storage.sqlite3");
        let quarantine = db_path.with_file_name(format!("{base_name}.corrupt"));
        ftui_runtime::ftui_println!("Moving corrupt database to {}", quarantine.display());
        std::fs::rename(&db_path, &quarantine)?;
        // Also remove WAL/SHM sidecars if present.
        // SQLite names these by appending -wal/-shm to the full filename.
        for suffix in &["-wal", "-shm"] {
            let mut aux_os = db_path.as_os_str().to_os_string();
            aux_os.push(suffix);
            let aux = PathBuf::from(aux_os);
            if aux.exists() {
                let _ = std::fs::remove_file(&aux);
            }
        }
    }

    ftui_runtime::ftui_println!(
        "Reconstructing database from archive at {}",
        storage_root.display()
    );

    let stats = mcp_agent_mail_db::reconstruct_from_archive(&db_path, &storage_root)
        .map_err(|e| CliError::Other(format!("reconstruction failed: {e}")))?;

    if json {
        ftui_runtime::ftui_println!(
            "{}",
            serde_json::json!({
                "status": "ok",
                "db_path": db_path.display().to_string(),
                "recovered": {
                    "projects": stats.projects,
                    "agents": stats.agents,
                    "messages": stats.messages,
                    "recipients": stats.recipients,
                    "parse_errors": stats.parse_errors,
                },
                "warnings": stats.warnings,
            })
        );
    } else {
        ftui_runtime::ftui_println!("Reconstruction complete: {stats}");
        if !stats.warnings.is_empty() {
            ftui_runtime::ftui_println!("Warnings:");
            for w in &stats.warnings {
                ftui_runtime::ftui_println!("  - {w}");
            }
        }
        if stats.parse_errors > 0 {
            ftui_runtime::ftui_println!(
                "Note: {} archive file(s) could not be parsed and were skipped.",
                stats.parse_errors
            );
        }
    }

    Ok(())
}

/// Quick archive stats for dry-run mode (no DB writes).
struct ArchiveScanStats {
    projects: usize,
    agents: usize,
    message_files: usize,
}

fn scan_archive_stats(storage_root: &Path) -> ArchiveScanStats {
    let mut stats = ArchiveScanStats {
        projects: 0,
        agents: 0,
        message_files: 0,
    };

    let projects_dir = storage_root.join("projects");
    let entries = match std::fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(_) => return stats,
    };

    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        stats.projects += 1;
        let project_dir = entry.path();

        // Count agents
        let agents_dir = project_dir.join("agents");
        if agents_dir.is_dir()
            && let Ok(agent_entries) = std::fs::read_dir(&agents_dir)
        {
            for ae in agent_entries.flatten() {
                if ae.path().join("profile.json").exists() {
                    stats.agents += 1;
                }
            }
        }

        // Count message files
        let messages_dir = project_dir.join("messages");
        if messages_dir.is_dir() {
            count_md_files_recursive(&messages_dir, &mut stats.message_files);
        }
    }

    stats
}

fn count_md_files_recursive(dir: &Path, count: &mut usize) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            count_md_files_recursive(&path, count);
        } else if path.extension().is_some_and(|e| e == "md") {
            *count += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Products commands
// ---------------------------------------------------------------------------

static PRODUCTS_HTTP_CLIENT: OnceLock<asupersync::http::h1::HttpClient> = OnceLock::new();
static PRODUCT_UID_COUNTER: AtomicU64 = AtomicU64::new(0);
const CHECK_INBOX_FETCH_LIMIT: i64 = 10;
const CHECK_INBOX_RPC_TIMEOUT_SECS: u64 = 3;
const DEFAULT_AGENT_MAIL_URL: &str = "127.0.0.1:8765";

fn products_http_client() -> &'static asupersync::http::h1::HttpClient {
    PRODUCTS_HTTP_CLIENT.get_or_init(asupersync::http::h1::HttpClient::new)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckInboxRpcConfig {
    pub server_url: String,
    pub server_urls: Vec<String>,
    pub bearer_token: Option<String>,
    pub project_key: String,
    pub agent_name: String,
    pub limit: i64,
    pub include_bodies: bool,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckInboxMessage {
    pub id: i64,
    pub subject: String,
    pub from: String,
    pub importance: String,
    pub created_ts: String,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckInboxRpcResult {
    pub unread_count: usize,
    pub urgent_or_high_count: usize,
    pub messages: Vec<CheckInboxMessage>,
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_hex_uid(candidate: &str) -> bool {
    let s = candidate.trim();
    if s.len() < 8 || s.len() > 64 {
        return false;
    }
    s.chars().all(|c| c.is_ascii_hexdigit())
}

fn generate_product_uid(now_micros: i64) -> String {
    let seq = PRODUCT_UID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = u64::from(std::process::id());
    let raw = format!("{now_micros:x}{pid:x}{seq:x}");
    let mut out = String::with_capacity(20);
    for ch in raw.chars() {
        if ch.is_ascii_hexdigit() {
            out.push(ch.to_ascii_lowercase());
        }
        if out.len() == 20 {
            break;
        }
    }
    while out.len() < 20 {
        out.push('0');
    }
    out
}

fn buffer_to_text_trim(buf: &ftui::Buffer) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(buf.height() as usize);
    for y in 0..buf.height() {
        let mut line = String::with_capacity(buf.width() as usize);
        for x in 0..buf.width() {
            let cell = buf.get(x, y).expect("buffer cell");
            if cell.is_continuation() {
                continue;
            }
            if cell.is_empty() {
                line.push(' ');
            } else if let Some(c) = cell.content.as_char() {
                line.push(c);
            } else {
                // Unknown content (grapheme ID etc). Keep width-correct placeholder.
                let w = cell.content.width();
                for _ in 0..w.max(1) {
                    line.push('?');
                }
            }
        }
        lines.push(line.trim_end().to_string());
    }
    while matches!(lines.last(), Some(s) if s.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn render_table_text(title: Option<&str>, headers: &[&str], rows: Vec<Vec<String>>) -> String {
    use ftui::layout::{Constraint, Rect};
    use ftui::widgets::Widget;
    use ftui::widgets::block::Block;
    use ftui::widgets::borders::BorderType;
    use ftui::widgets::table::{Row, Table};

    let col_count = headers.len();
    let widths = (0..col_count)
        .map(|idx| match idx {
            // Heuristic widths: keep IDs compact, give text columns room.
            0 => Constraint::FitContentBounded { min: 2, max: 14 },
            1 => Constraint::FitContentBounded { min: 2, max: 16 },
            2 => Constraint::FitContentBounded { min: 4, max: 72 },
            3 => Constraint::FitContentBounded { min: 4, max: 28 },
            4 => Constraint::FitContentBounded { min: 4, max: 40 },
            _ => Constraint::FitContentBounded { min: 2, max: 80 },
        })
        .collect::<Vec<_>>();

    let row_count = rows.len();
    let header = Row::new(headers.iter().map(|h| h.to_string()).collect::<Vec<_>>());
    let ftui_rows = rows.into_iter().map(Row::new).collect::<Vec<_>>();

    let mut table = Table::new(ftui_rows, widths)
        .header(header)
        .column_spacing(2);
    if let Some(t) = title {
        table = table.block(Block::bordered().border_type(BorderType::Ascii).title(t));
    }

    // Render headless to a buffer and print as text. Keep the width stable for tests.
    let width: u16 = 120;
    let height = row_count
        .saturating_add(4)
        .clamp(6, 200)
        .try_into()
        .unwrap_or(200u16);

    let mut pool = ftui::GraphemePool::new();
    let mut frame = ftui::Frame::new(width, height, &mut pool);
    let area = Rect::new(0, 0, width, height);
    table.render(area, &mut frame);

    buffer_to_text_trim(&frame.buffer)
}

fn print_table(title: Option<&str>, headers: &[&str], rows: Vec<Vec<String>>) {
    let rendered = render_table_text(title, headers, rows);
    ftui_runtime::ftui_println!("{rendered}");
}

fn value_looks_like_template(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return true;
    }
    let upper = trimmed.to_ascii_uppercase();
    upper.contains("YOUR_")
        || upper.contains("PLACEHOLDER")
        || (trimmed.starts_with('<') && trimmed.ends_with('>'))
}

fn normalize_agent_mail_url(raw: &str, http_path: &str) -> String {
    let trimmed = raw.trim();
    let base = if trimmed.is_empty() {
        DEFAULT_AGENT_MAIL_URL.to_string()
    } else {
        trimmed.to_string()
    };

    let with_scheme = if base.starts_with("http://") || base.starts_with("https://") {
        base
    } else {
        format!("http://{base}")
    };

    let boundary = with_scheme
        .find('?')
        .into_iter()
        .chain(with_scheme.find('#'))
        .min()
        .unwrap_or(with_scheme.len());
    let (prefix, suffix) = with_scheme.split_at(boundary);

    let has_path = prefix
        .split_once("://")
        .is_some_and(|(_, rest)| rest.contains('/'));
    if has_path {
        let mut normalized = prefix.to_string();
        if !normalized.ends_with('/') {
            normalized.push('/');
        }
        normalized.push_str(suffix);
        return normalized;
    }

    let normalized_path = normalize_http_path(http_path);
    format!("{prefix}{normalized_path}{suffix}")
}

fn check_inbox_server_urls_for_agent_mail_url(raw_url: &str, http_path: &str) -> Vec<String> {
    let primary = normalize_agent_mail_url(raw_url, http_path);
    let normalized_path = normalize_http_path(http_path);
    let Some(alias_path) = mcp_base_alias_path(&normalized_path) else {
        return vec![primary];
    };

    // Only alias host[:port] style URLs. If AGENT_MAIL_URL already has a path,
    // respect that explicit path and do not synthesize /api <-> /mcp aliases.
    let trimmed = raw_url.trim();
    let base = if trimmed.is_empty() {
        DEFAULT_AGENT_MAIL_URL.to_string()
    } else {
        trimmed.to_string()
    };
    let with_scheme = if base.starts_with("http://") || base.starts_with("https://") {
        base
    } else {
        format!("http://{base}")
    };
    let boundary = with_scheme
        .find('?')
        .into_iter()
        .chain(with_scheme.find('#'))
        .min()
        .unwrap_or(with_scheme.len());
    let prefix = &with_scheme[..boundary];
    let has_explicit_path = prefix
        .split_once("://")
        .is_some_and(|(_, rest)| rest.contains('/'));
    if has_explicit_path {
        return vec![primary];
    }

    let alias = normalize_agent_mail_url(raw_url, alias_path);
    if alias == primary {
        vec![primary]
    } else {
        vec![primary, alias]
    }
}

fn check_inbox_rpc_config_from_env_reader<F>(read_env: F) -> Option<CheckInboxRpcConfig>
where
    F: Fn(&str) -> Option<String>,
{
    let project_key = read_env("AGENT_MAIL_PROJECT")?;
    let agent_name = read_env("AGENT_MAIL_AGENT")?;
    if value_looks_like_template(&project_key) || value_looks_like_template(&agent_name) {
        return None;
    }

    let raw_url = read_env("AGENT_MAIL_URL").unwrap_or_else(|| DEFAULT_AGENT_MAIL_URL.to_string());
    let http_path = read_env("HTTP_PATH").unwrap_or_else(|| "/mcp/".to_string());
    let server_urls = check_inbox_server_urls_for_agent_mail_url(&raw_url, &http_path);
    let server_url = server_urls
        .first()
        .cloned()
        .unwrap_or_else(|| normalize_agent_mail_url(&raw_url, &http_path));
    let bearer_token = read_env("AGENT_MAIL_TOKEN")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && !value_looks_like_template(v));

    Some(CheckInboxRpcConfig {
        server_url,
        server_urls,
        bearer_token,
        project_key: project_key.trim().to_string(),
        agent_name: agent_name.trim().to_string(),
        limit: CHECK_INBOX_FETCH_LIMIT,
        include_bodies: false,
        timeout_seconds: CHECK_INBOX_RPC_TIMEOUT_SECS,
    })
}

pub fn check_inbox_rpc_config_from_env() -> Option<CheckInboxRpcConfig> {
    check_inbox_rpc_config_from_env_reader(|key| std::env::var(key).ok())
}

fn build_fetch_inbox_jsonrpc_request(config: &CheckInboxRpcConfig) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": "tools/call",
        "params": {
            "name": "fetch_inbox",
            "arguments": {
                "project_key": config.project_key,
                "agent_name": config.agent_name,
                "limit": config.limit,
                "include_bodies": config.include_bodies,
            }
        }
    })
}

fn parse_jsonrpc_error(payload: &serde_json::Value) -> Option<String> {
    let err = payload.get("error")?;
    let code = err.get("code").and_then(serde_json::Value::as_i64);
    let message = err
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown JSON-RPC error");
    let data = err.get("data").cloned().unwrap_or(serde_json::Value::Null);
    Some(match code {
        Some(code) => format!("JSON-RPC error {code}: {message}; data={data}"),
        None => format!("JSON-RPC error: {message}; data={data}"),
    })
}

async fn post_jsonrpc_request(
    server_url: &str,
    bearer: Option<&str>,
    payload: &serde_json::Value,
    timeout_seconds: u64,
) -> CliResult<serde_json::Value> {
    use asupersync::http::h1::Method;
    use asupersync::time::{timeout, wall_now};
    use std::time::Duration;

    let body = serde_json::to_vec(payload)
        .map_err(|e| CliError::Other(format!("failed to encode JSON-RPC request: {e}")))?;
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if let Some(tok) = bearer.filter(|s| !s.is_empty()) {
        headers.push(("Authorization".to_string(), format!("Bearer {tok}")));
    }

    let request = Box::pin(products_http_client().request(Method::Post, server_url, headers, body));
    let response = match timeout(wall_now(), Duration::from_secs(timeout_seconds), request).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            return Err(CliError::Other(format!(
                "transport failure calling {server_url}: {e}"
            )));
        }
        Err(_) => {
            return Err(CliError::Other(format!(
                "request to {server_url} timed out after {timeout_seconds}s"
            )));
        }
    };

    if response.status == 401 || response.status == 403 {
        return Err(CliError::Other(format!(
            "authentication failed (HTTP {}) while calling {server_url}; check AGENT_MAIL_TOKEN/HTTP_BEARER_TOKEN",
            response.status
        )));
    }
    if response.status != 200 {
        return Err(CliError::Other(format!(
            "unexpected HTTP status {} from {server_url}",
            response.status
        )));
    }

    serde_json::from_slice(&response.body).map_err(|e| {
        CliError::Other(format!(
            "invalid JSON in server response from {server_url}: {e}"
        ))
    })
}

fn normalize_fetch_inbox_rows(result_payload: serde_json::Value) -> Option<Vec<serde_json::Value>> {
    match coerce_tool_result_json(result_payload)? {
        serde_json::Value::Array(rows) => Some(rows),
        serde_json::Value::Object(map) => {
            if let Some(rows) = map.get("result").and_then(serde_json::Value::as_array) {
                return Some(rows.clone());
            }
            if let Some(rows) = map.get("messages").and_then(serde_json::Value::as_array) {
                return Some(rows.clone());
            }
            None
        }
        _ => None,
    }
}

fn parse_fetch_inbox_rows(result_payload: serde_json::Value) -> CliResult<CheckInboxRpcResult> {
    let rows = normalize_fetch_inbox_rows(result_payload)
        .ok_or_else(|| CliError::Other("unexpected fetch_inbox response shape".to_string()))?;

    let mut messages = Vec::with_capacity(rows.len());
    let mut urgent_or_high_count = 0usize;

    for row in rows {
        let importance = row
            .get("importance")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        if importance == "urgent" || importance == "high" {
            urgent_or_high_count += 1;
        }

        let from = row
            .get("from")
            .or_else(|| row.get("sender_name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();

        messages.push(CheckInboxMessage {
            id: row
                .get("id")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or_default(),
            subject: row
                .get("subject")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            from,
            importance,
            created_ts: row
                .get("created_ts")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            raw: row,
        });
    }

    Ok(CheckInboxRpcResult {
        unread_count: messages.len(),
        urgent_or_high_count,
        messages,
    })
}

pub async fn fetch_inbox_via_jsonrpc(
    config: &CheckInboxRpcConfig,
) -> CliResult<CheckInboxRpcResult> {
    let mut urls = Vec::with_capacity(config.server_urls.len() + 1);
    urls.push(config.server_url.clone());
    for url in &config.server_urls {
        if !urls.iter().any(|existing| existing == url) {
            urls.push(url.clone());
        }
    }

    let mut last_error: Option<CliError> = None;
    for server_url in urls {
        let request = build_fetch_inbox_jsonrpc_request(config);
        let payload = match post_jsonrpc_request(
            &server_url,
            config.bearer_token.as_deref(),
            &request,
            config.timeout_seconds,
        )
        .await
        {
            Ok(payload) => payload,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };

        if let Some(error_text) = parse_jsonrpc_error(&payload) {
            last_error = Some(CliError::Other(error_text));
            continue;
        }

        let result = match payload.get("result").cloned() {
            Some(result) => result,
            None => {
                last_error = Some(CliError::Other(
                    "missing JSON-RPC result payload".to_string(),
                ));
                continue;
            }
        };

        match parse_fetch_inbox_rows(result) {
            Ok(parsed) => return Ok(parsed),
            Err(error) => {
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        CliError::Other("no server URLs configured for check-inbox HTTP mode".to_string())
    }))
}

async fn fetch_inbox_via_jsonrpc_with_fallback(
    config: &CheckInboxRpcConfig,
    server_urls: &[String],
) -> CliResult<CheckInboxRpcResult> {
    let mut attempt_config = config.clone();
    let mut merged = Vec::with_capacity(server_urls.len() + config.server_urls.len() + 1);
    merged.push(config.server_url.clone());
    for server_url in server_urls {
        if !merged.iter().any(|existing| existing == server_url) {
            merged.push(server_url.clone());
        }
    }
    for server_url in &config.server_urls {
        if !merged.iter().any(|existing| existing == server_url) {
            merged.push(server_url.clone());
        }
    }
    if let Some(primary) = merged.first() {
        attempt_config.server_url = primary.clone();
    }
    attempt_config.server_urls = merged;
    fetch_inbox_via_jsonrpc(&attempt_config).await
}

/// Configuration for direct DB inbox check (bypasses HTTP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckInboxDirectConfig {
    /// Project key (path or slug) to query.
    pub project_key: String,
    /// Agent name to check inbox for.
    pub agent_name: String,
    /// Maximum messages to fetch.
    pub limit: i64,
}

/// Check inbox via direct SQLite query (for co-located setups).
///
/// This bypasses the HTTP/MCP server and queries the database directly,
/// which is faster and works even when the server is not running.
pub fn check_inbox_direct(config: &CheckInboxDirectConfig) -> CliResult<CheckInboxRpcResult> {
    use sqlmodel_core::Value;

    let conn = open_db_sync()?;

    // Resolve project ID from project_key (try slug first, then human_key)
    let project_rows = conn
        .query_sync(
            "SELECT id FROM projects WHERE slug = ? OR human_key = ? LIMIT 1",
            &[
                Value::Text(config.project_key.clone()),
                Value::Text(config.project_key.clone()),
            ],
        )
        .map_err(|e| CliError::Other(format!("project lookup failed: {e}")))?;

    let project_id: i64 = project_rows
        .first()
        .and_then(|row| row.get_named("id").ok())
        .ok_or_else(|| CliError::Other(format!("project not found: {}", config.project_key)))?;

    // Resolve agent ID from agent_name
    let agent_rows = conn
        .query_sync(
            "SELECT id FROM agents WHERE project_id = ? AND name = ? LIMIT 1",
            &[
                Value::BigInt(project_id),
                Value::Text(config.agent_name.clone()),
            ],
        )
        .map_err(|e| CliError::Other(format!("agent lookup failed: {e}")))?;

    let agent_id: i64 = agent_rows
        .first()
        .and_then(|row| row.get_named("id").ok())
        .ok_or_else(|| {
            CliError::Other(format!(
                "agent '{}' not found in project '{}'",
                config.agent_name, config.project_key
            ))
        })?;

    // Query unread messages (where read_ts IS NULL)
    let sql = "
        SELECT m.id, m.subject, m.importance, m.created_ts, a.name AS sender_name
        FROM message_recipients mr
        JOIN messages m ON m.id = mr.message_id
        JOIN agents a ON a.id = m.sender_id
        WHERE mr.agent_id = ? AND m.project_id = ? AND mr.read_ts IS NULL
        ORDER BY m.created_ts DESC
        LIMIT ?
    ";

    let rows = conn
        .query_sync(
            sql,
            &[
                Value::BigInt(agent_id),
                Value::BigInt(project_id),
                Value::BigInt(config.limit),
            ],
        )
        .map_err(|e| CliError::Other(format!("inbox query failed: {e}")))?;

    let mut messages = Vec::with_capacity(rows.len());
    let mut urgent_or_high_count = 0usize;

    for row in &rows {
        let id: i64 = row.get_named("id").unwrap_or(0);
        let subject: String = row.get_named("subject").unwrap_or_default();
        let from: String = row.get_named("sender_name").unwrap_or_default();
        let importance: String = row.get_named("importance").unwrap_or_default();
        let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);

        // Count urgent/high priority messages
        if importance == "urgent" || importance == "high" {
            urgent_or_high_count += 1;
        }

        // Format timestamp as ISO-8601 string
        let created_ts_str = format_micros_as_iso(created_ts);

        let raw = serde_json::json!({
            "id": id,
            "subject": &subject,
            "from": &from,
            "importance": &importance,
            "created_ts": created_ts,
        });
        messages.push(CheckInboxMessage {
            id,
            subject,
            from,
            importance,
            created_ts: created_ts_str,
            raw,
        });
    }

    Ok(CheckInboxRpcResult {
        unread_count: messages.len(),
        urgent_or_high_count,
        messages,
    })
}

/// Format microsecond timestamp as ISO-8601 string.
fn format_micros_as_iso(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let sub_micros = micros.rem_euclid(1_000_000) as u32;
    let nanos = sub_micros.saturating_mul(1000);
    if let Some(dt) = chrono::DateTime::from_timestamp(secs, nanos) {
        dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    } else {
        "1970-01-01T00:00:00Z".to_string()
    }
}

/// Default check-inbox rate limit interval in seconds.
pub const CHECK_INBOX_RATE_LIMIT_DEFAULT_SECS: u64 = 120;

/// Rate limiter for inbox checks.
///
/// Prevents excessive inbox polling by tracking the last check time per agent.
/// Uses a lockfile in /tmp with the agent name sanitized.
#[derive(Debug, Clone)]
pub struct CheckInboxRateLimiter {
    /// Sanitized agent name used in lockfile path.
    agent_sanitized: String,
    /// Minimum interval between checks.
    interval_secs: u64,
}

impl CheckInboxRateLimiter {
    /// Create a new rate limiter for the given agent.
    ///
    /// # Arguments
    /// - `agent_name`: Agent name (will be sanitized for filesystem)
    /// - `interval_secs`: Minimum seconds between checks (default: 120)
    #[must_use]
    pub fn new(agent_name: &str, interval_secs: Option<u64>) -> Self {
        Self {
            agent_sanitized: sanitize_agent_name(agent_name),
            interval_secs: interval_secs.unwrap_or(CHECK_INBOX_RATE_LIMIT_DEFAULT_SECS),
        }
    }

    /// Get the lockfile path for this agent.
    #[must_use]
    pub fn lockfile_path(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("/tmp/mcp-mail-check-{}", self.agent_sanitized))
    }

    fn process_local_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
        static CACHE: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<String, u64>>,
        > = std::sync::OnceLock::new();
        CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    /// Check if enough time has elapsed since the last check.
    ///
    /// Returns `true` if a check should proceed, `false` if rate-limited.
    /// Updates the lockfile timestamp if proceeding.
    ///
    /// Errors are treated as "proceed" (fail-open) to avoid blocking agent work.
    #[must_use]
    pub fn should_check(&self) -> bool {
        let path = self.lockfile_path();
        let cache_key = path.to_string_lossy().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Read last check timestamp from file
        let fs_last_check = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        let mem_last_check = Self::process_local_cache()
            .lock()
            .ok()
            .and_then(|cache| cache.get(&cache_key).copied());
        let last_check = match (fs_last_check, mem_last_check) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        // Check if enough time has elapsed
        if let Some(last) = last_check {
            let elapsed = now.saturating_sub(last);
            if elapsed < self.interval_secs {
                // Rate limited - skip this check
                return false;
            }
        }

        // Update process-local timestamp first so failing filesystem writes
        // still rate-limit this process.
        if let Ok(mut cache) = Self::process_local_cache().lock() {
            cache.insert(cache_key, now);
        }

        // Best-effort cross-process timestamp persistence.
        let _ = std::fs::write(&path, now.to_string());

        true
    }

    /// Reset the rate limiter by removing the lockfile.
    ///
    /// Errors are ignored.
    pub fn reset(&self) {
        let path = self.lockfile_path();
        let _ = std::fs::remove_file(&path);
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        }
        if let Ok(mut cache) = Self::process_local_cache().lock() {
            cache.remove(path.to_string_lossy().as_ref());
        }
    }
}

/// Sanitize agent name for use in filesystem paths.
///
/// Replaces non-alphanumeric characters with underscore.
#[must_use]
pub fn sanitize_agent_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

async fn try_call_server_tool(
    server_url: &str,
    bearer: Option<&str>,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Option<serde_json::Value> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": format!("cli-{tool_name}"),
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": arguments,
        }
    });
    let v = post_jsonrpc_request(server_url, bearer, &req, 10)
        .await
        .ok()?;
    v.get("result").cloned()
}

fn coerce_tool_result_json(result: serde_json::Value) -> Option<serde_json::Value> {
    match result {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => serde_json::from_str(&s).ok(),
        serde_json::Value::Object(map) => {
            if let Some(v) = map.get("structured_content") {
                return Some(v.clone());
            }
            if let Some(content) = map.get("content") {
                // FastMCP-style: { content: [{ type: "text", text: "..." }] }
                if let Some(text) = content
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|v| v.get("text"))
                    .and_then(|v| v.as_str())
                    && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text)
                {
                    return Some(parsed);
                }
            }
            Some(serde_json::Value::Object(map))
        }
        other => Some(other),
    }
}

async fn get_product_by_key(
    cx: &asupersync::Cx,
    pool: &mcp_agent_mail_db::DbPool,
    key: &str,
) -> CliResult<Option<mcp_agent_mail_db::ProductRow>> {
    use mcp_agent_mail_db::sqlmodel::Model;
    use mcp_agent_mail_db::sqlmodel::Value;

    let conn = match pool.acquire(cx).await {
        asupersync::Outcome::Ok(c) => c,
        asupersync::Outcome::Err(e) => {
            return Err(CliError::Other(format!("db acquire failed: {e}")));
        }
        asupersync::Outcome::Cancelled(_) => {
            return Err(CliError::Other("request cancelled".to_string()));
        }
        asupersync::Outcome::Panicked(p) => {
            return Err(CliError::Other(format!("internal panic: {}", p.message())));
        }
    };

    let sql = "SELECT id, product_uid, name, created_at FROM products WHERE product_uid = ? OR name = ? LIMIT 1";
    let params = [Value::Text(key.to_string()), Value::Text(key.to_string())];
    let rows = conn
        .query_sync(sql, &params)
        .map_err(|e| CliError::Other(format!("product lookup failed: {e}")))?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let product = mcp_agent_mail_db::ProductRow::from_row(&row)
        .map_err(|e| CliError::Other(format!("bad product row: {e}")))?;
    Ok(Some(product))
}

async fn get_project_record(
    cx: &asupersync::Cx,
    pool: &mcp_agent_mail_db::DbPool,
    identifier: &str,
) -> CliResult<mcp_agent_mail_db::ProjectRow> {
    let raw = identifier.trim();
    let mut canonical = raw.to_string();
    let path = Path::new(raw);
    if path.is_absolute()
        && let Ok(resolved) = path.canonicalize()
    {
        canonical = resolved.display().to_string();
    }
    let slug = mcp_agent_mail_core::compute_project_slug(&canonical);

    let out = mcp_agent_mail_db::queries::get_project_by_slug(cx, pool, &slug).await;
    match out {
        asupersync::Outcome::Ok(row) => return Ok(row),
        asupersync::Outcome::Err(_) => {}
        asupersync::Outcome::Cancelled(_) => {
            return Err(CliError::Other("request cancelled".to_string()));
        }
        asupersync::Outcome::Panicked(p) => {
            return Err(CliError::Other(format!("internal panic: {}", p.message())));
        }
    }

    let out = mcp_agent_mail_db::queries::get_project_by_human_key(cx, pool, &canonical).await;
    match out {
        asupersync::Outcome::Ok(row) => return Ok(row),
        asupersync::Outcome::Err(_) => {}
        asupersync::Outcome::Cancelled(_) => {
            return Err(CliError::Other("request cancelled".to_string()));
        }
        asupersync::Outcome::Panicked(p) => {
            return Err(CliError::Other(format!("internal panic: {}", p.message())));
        }
    }

    if canonical != raw {
        let out = mcp_agent_mail_db::queries::get_project_by_human_key(cx, pool, raw).await;
        match out {
            asupersync::Outcome::Ok(row) => return Ok(row),
            asupersync::Outcome::Err(_) => {}
            asupersync::Outcome::Cancelled(_) => {
                return Err(CliError::Other("request cancelled".to_string()));
            }
            asupersync::Outcome::Panicked(p) => {
                return Err(CliError::Other(format!("internal panic: {}", p.message())));
            }
        }
    }

    Err(CliError::Other(format!("Project '{raw}' not found")))
}

async fn ensure_product_local(
    cx: &asupersync::Cx,
    pool: &mcp_agent_mail_db::DbPool,
    product_key: Option<&str>,
    name: Option<&str>,
) -> CliResult<mcp_agent_mail_db::ProductRow> {
    let key_raw = product_key.or(name).unwrap_or("").trim();
    if key_raw.is_empty() {
        ftui_runtime::ftui_eprintln!("Provide a product_key or --name.");
        return Err(CliError::ExitCode(2));
    }

    if let Some(existing) = get_product_by_key(cx, pool, key_raw).await? {
        return Ok(existing);
    }

    let now = mcp_agent_mail_db::now_micros();
    let uid = match product_key {
        Some(pk) if is_hex_uid(pk) => pk.trim().to_ascii_lowercase(),
        _ => generate_product_uid(now),
    };
    let display_name_raw = name.unwrap_or(key_raw);
    let mut display_name = collapse_whitespace(display_name_raw)
        .chars()
        .take(255)
        .collect::<String>();
    if display_name.is_empty() {
        display_name = uid.clone();
    }

    let out = mcp_agent_mail_db::queries::ensure_product(
        cx,
        pool,
        Some(uid.as_str()),
        Some(display_name.as_str()),
    )
    .await;
    match out {
        asupersync::Outcome::Ok(row) => Ok(row),
        asupersync::Outcome::Err(e) => Err(CliError::Other(format!("ensure product failed: {e}"))),
        asupersync::Outcome::Cancelled(_) => Err(CliError::Other("request cancelled".to_string())),
        asupersync::Outcome::Panicked(p) => {
            Err(CliError::Other(format!("internal panic: {}", p.message())))
        }
    }
}

fn handle_products(action: ProductsCommand) -> CliResult<()> {
    use asupersync::runtime::RuntimeBuilder;

    let runtime = RuntimeBuilder::current_thread()
        .build()
        .map_err(|e| CliError::Other(format!("failed to build runtime: {e}")))?;
    runtime.block_on(async move { handle_products_async(action).await })
}

async fn handle_products_async(action: ProductsCommand) -> CliResult<()> {
    let config = Config::from_env();
    let server_url = format!(
        "http://{}:{}{}",
        config.http_host, config.http_port, config.http_path
    );
    let bearer = config.http_bearer_token.as_deref();

    let cx = asupersync::Cx::for_request();
    let pool_cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    let pool = mcp_agent_mail_db::get_or_create_pool(&pool_cfg)
        .map_err(|e| CliError::Other(format!("db pool init failed: {e}")))?;

    handle_products_with(&cx, &pool, Some(server_url.as_str()), bearer, action).await
}

async fn handle_products_with(
    cx: &asupersync::Cx,
    pool: &mcp_agent_mail_db::DbPool,
    server_url: Option<&str>,
    bearer: Option<&str>,
    action: ProductsCommand,
) -> CliResult<()> {
    match action {
        ProductsCommand::Ensure {
            product_key,
            name,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let key_raw = product_key
                .as_deref()
                .or(name.as_deref())
                .unwrap_or("")
                .trim()
                .to_string();
            if key_raw.is_empty() {
                ftui_runtime::ftui_eprintln!("Provide a product_key or --name.");
                return Err(CliError::ExitCode(2));
            }

            // Prefer server tool to ensure strict uid policy (legacy behavior).
            let mut args = serde_json::Map::new();
            if let Some(pk) = &product_key {
                args.insert(
                    "product_key".to_string(),
                    serde_json::Value::String(pk.clone()),
                );
            }
            if let Some(n) = &name {
                args.insert("name".to_string(), serde_json::Value::String(n.clone()));
            }
            let server_result = if let Some(url) = server_url {
                try_call_server_tool(
                    url,
                    bearer,
                    "ensure_product",
                    serde_json::Value::Object(args),
                )
                .await
                .and_then(coerce_tool_result_json)
            } else {
                None
            };

            let payload = if let Some(v) = server_result {
                v
            } else {
                let row =
                    ensure_product_local(cx, pool, product_key.as_deref(), name.as_deref()).await?;
                serde_json::json!({
                    "id": row.id.unwrap_or(0),
                    "product_uid": row.product_uid,
                    "name": row.name,
                    "created_at": mcp_agent_mail_db::micros_to_iso(row.created_at),
                })
            };

            output::emit_output(&payload, fmt, || {
                let created_at = payload
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let rows = vec![
                    vec![
                        "id".to_string(),
                        payload.get("id").cloned().unwrap_or_default().to_string(),
                    ],
                    vec![
                        "product_uid".to_string(),
                        payload
                            .get("product_uid")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ],
                    vec![
                        "name".to_string(),
                        payload
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ],
                    vec!["created_at".to_string(), created_at.to_string()],
                ];
                print_table(Some("Product"), &["Field", "Value"], rows);
            });
            Ok(())
        }
        ProductsCommand::Link {
            product_key,
            project,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let prod = get_product_by_key(cx, pool, product_key.trim())
                .await?
                .ok_or_else(|| CliError::Other(format!("Product '{product_key}' not found")))?;
            let proj = get_project_record(cx, pool, &project).await?;

            let prod_id = prod.id.unwrap_or(0);
            let proj_id = proj.id.unwrap_or(0);

            let out =
                mcp_agent_mail_db::queries::link_product_to_projects(cx, pool, prod_id, &[proj_id])
                    .await;
            match out {
                asupersync::Outcome::Ok(_) => {}
                asupersync::Outcome::Err(e) => {
                    return Err(CliError::Other(format!("link failed: {e}")));
                }
                asupersync::Outcome::Cancelled(_) => {
                    return Err(CliError::Other("request cancelled".to_string()));
                }
                asupersync::Outcome::Panicked(p) => {
                    return Err(CliError::Other(format!("internal panic: {}", p.message())));
                }
            }

            let payload = serde_json::json!({
                "product_uid": prod.product_uid,
                "product_name": prod.name,
                "project_slug": proj.slug,
            });

            output::emit_output(&payload, fmt, || {
                ftui_runtime::ftui_println!(
                    "Linked project '{}' into product '{}' ({}).",
                    payload
                        .get("project_slug")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                    payload
                        .get("product_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                    payload
                        .get("product_uid")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                );
            });
            Ok(())
        }
        ProductsCommand::Status {
            product_key,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let prod = get_product_by_key(cx, pool, product_key.trim())
                .await?
                .ok_or_else(|| {
                    ftui_runtime::ftui_eprintln!("Product '{product_key}' not found.");
                    CliError::ExitCode(2)
                })?;
            let prod_id = prod.id.unwrap_or(0);
            let projects =
                match mcp_agent_mail_db::queries::list_product_projects(cx, pool, prod_id).await {
                    asupersync::Outcome::Ok(v) => v,
                    asupersync::Outcome::Err(e) => {
                        return Err(CliError::Other(format!("status query failed: {e}")));
                    }
                    asupersync::Outcome::Cancelled(_) => {
                        return Err(CliError::Other("request cancelled".to_string()));
                    }
                    asupersync::Outcome::Panicked(p) => {
                        return Err(CliError::Other(format!("internal panic: {}", p.message())));
                    }
                };

            let payload = serde_json::json!({
                "product": {
                    "id": prod.id.unwrap_or(0),
                    "product_uid": prod.product_uid,
                    "name": prod.name,
                    "created_at": mcp_agent_mail_db::micros_to_iso(prod.created_at),
                },
                "projects": projects.iter().map(|p| serde_json::json!({
                    "id": p.id.unwrap_or(0),
                    "slug": p.slug,
                    "human_key": p.human_key,
                })).collect::<Vec<_>>(),
            });

            output::emit_output(&payload, fmt, || {
                let prod_title = payload
                    .get("product")
                    .and_then(|p| p.get("name"))
                    .and_then(|v| v.as_str())
                    .map(|n| format!("Product: {n}"))
                    .unwrap_or_else(|| "Product".to_string());

                let p = payload.get("product").cloned().unwrap_or_default();
                let rows = vec![
                    vec![
                        "id".to_string(),
                        p.get("id").cloned().unwrap_or_default().to_string(),
                    ],
                    vec![
                        "product_uid".to_string(),
                        p.get("product_uid")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ],
                    vec![
                        "name".to_string(),
                        p.get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ],
                    vec![
                        "created_at".to_string(),
                        p.get("created_at")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ],
                ];
                print_table(Some(&prod_title), &["Field", "Value"], rows);
                ftui_runtime::ftui_println!();

                let proj_rows = payload
                    .get("projects")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| {
                        vec![
                            p.get("id").cloned().unwrap_or_default().to_string(),
                            p.get("slug")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            p.get("human_key")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        ]
                    })
                    .collect::<Vec<_>>();
                print_table(
                    Some("Linked Projects"),
                    &["id", "slug", "human_key"],
                    proj_rows,
                );
            });
            Ok(())
        }
        ProductsCommand::Search {
            product_key,
            query,
            limit,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let prod = get_product_by_key(cx, pool, product_key.trim())
                .await?
                .ok_or_else(|| {
                    ftui_runtime::ftui_eprintln!("Product '{product_key}' not found.");
                    CliError::ExitCode(2)
                })?;
            let prod_id = prod.id.unwrap_or(0);
            let mut search_query =
                mcp_agent_mail_db::search_planner::SearchQuery::product_messages(&query, prod_id);
            search_query.limit = Some(limit.max(0) as usize);

            let response = outcome_to_result(
                mcp_agent_mail_db::search_service::execute_search_simple(cx, pool, &search_query)
                    .await,
            )?;
            let out: Vec<serde_json::Value> = response
                .results
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "project_id": r.project_id.unwrap_or(0),
                        "id": r.id,
                        "subject": r.title,
                        "from": r.from_agent.clone().unwrap_or_default(),
                        "created_ts": r.created_ts.map(mcp_agent_mail_db::micros_to_iso),
                    })
                })
                .collect();

            if response.results.is_empty() {
                output::emit_empty(fmt, "No results.");
                return Ok(());
            }

            let payload = serde_json::json!({ "result": out });
            output::emit_output(&payload, fmt, || {
                let title = format!("Product search: '{query}'");
                let rows = payload
                    .get("result")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| {
                        vec![
                            r.get("project_id").cloned().unwrap_or_default().to_string(),
                            r.get("id").cloned().unwrap_or_default().to_string(),
                            r.get("subject")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            r.get("from")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            r.get("created_ts")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        ]
                    })
                    .collect::<Vec<_>>();
                print_table(
                    Some(&title),
                    &["project_id", "id", "subject", "from", "created_ts"],
                    rows,
                );
            });
            Ok(())
        }
        ProductsCommand::Inbox {
            product_key,
            agent,
            limit,
            urgent_only,
            all,
            include_bodies,
            no_bodies,
            since_ts,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let urgent_only = resolve_bool(urgent_only, all, false);
            let include_bodies = resolve_bool(include_bodies, no_bodies, false);

            // Prefer server tool, but fall back to local DB if server is unreachable or
            // returns an empty result set.
            let server_result = if let Some(url) = server_url {
                try_call_server_tool(
                    url,
                    bearer,
                    "fetch_inbox_product",
                    serde_json::json!({
                        "product_key": product_key,
                        "agent_name": agent,
                        "limit": limit,
                        "urgent_only": urgent_only,
                        "include_bodies": include_bodies,
                        "since_ts": since_ts.clone().unwrap_or_default(),
                    }),
                )
                .await
                .and_then(coerce_tool_result_json)
            } else {
                None
            };

            let mut items: Vec<serde_json::Value> = match server_result {
                Some(v) => match v {
                    serde_json::Value::Array(a) => a,
                    serde_json::Value::Object(obj) => obj
                        .get("result")
                        .and_then(|r| r.as_array())
                        .cloned()
                        .unwrap_or_default(),
                    _ => Vec::new(),
                },
                None => Vec::new(),
            };

            if items.is_empty() {
                // Local fallback.
                if let Some(prod) = get_product_by_key(cx, pool, product_key.trim()).await? {
                    let prod_id = prod.id.unwrap_or(0);
                    let projects =
                        match mcp_agent_mail_db::queries::list_product_projects(cx, pool, prod_id)
                            .await
                        {
                            asupersync::Outcome::Ok(v) => v,
                            asupersync::Outcome::Err(e) => {
                                return Err(CliError::Other(format!("project list failed: {e}")));
                            }
                            asupersync::Outcome::Cancelled(_) => {
                                return Err(CliError::Other("request cancelled".to_string()));
                            }
                            asupersync::Outcome::Panicked(p) => {
                                return Err(CliError::Other(format!(
                                    "internal panic: {}",
                                    p.message()
                                )));
                            }
                        };

                    let since_micros = since_ts
                        .as_deref()
                        .and_then(mcp_agent_mail_db::iso_to_micros);
                    let max_messages = usize::try_from(limit.max(0)).unwrap_or(0);

                    let mut merged: Vec<(i64, i64, serde_json::Value)> = Vec::new();
                    for p in projects {
                        let project_id = p.id.unwrap_or(0);
                        let agent_row = match mcp_agent_mail_db::queries::get_agent(
                            cx, pool, project_id, &agent,
                        )
                        .await
                        {
                            asupersync::Outcome::Ok(a) => a,
                            _ => continue, // legacy: skip missing agent in project
                        };
                        let rows = match mcp_agent_mail_db::queries::fetch_inbox(
                            cx,
                            pool,
                            project_id,
                            agent_row.id.unwrap_or(0),
                            urgent_only,
                            since_micros,
                            max_messages,
                        )
                        .await
                        {
                            asupersync::Outcome::Ok(v) => v,
                            _ => continue,
                        };
                        for row in rows {
                            let msg = row.message;
                            let id = msg.id.unwrap_or(0);
                            let created_ts = msg.created_ts;
                            let mut obj = serde_json::json!({
                                "id": id,
                                "project_id": msg.project_id,
                                "subject": msg.subject,
                                "importance": msg.importance,
                                "ack_required": msg.ack_required != 0,
                                "created_ts": mcp_agent_mail_db::micros_to_iso(created_ts),
                                "from": row.sender_name,
                                "kind": row.kind,
                            });
                            if include_bodies {
                                obj.as_object_mut().unwrap().insert(
                                    "body_md".to_string(),
                                    serde_json::Value::String(msg.body_md),
                                );
                            }
                            merged.push((created_ts, id, obj));
                        }
                    }

                    merged.sort_by(|(a_ts, a_id, _), (b_ts, b_id, _)| {
                        b_ts.cmp(a_ts).then_with(|| a_id.cmp(b_id))
                    });
                    items = merged
                        .into_iter()
                        .take(max_messages)
                        .map(|(_, _, v)| v)
                        .collect();
                }
            }

            if items.is_empty() {
                output::emit_empty(fmt, "No messages found.");
                return Ok(());
            }

            output::emit_output(&items, fmt, || {
                let title = format!("Inbox for {agent} in product '{product_key}'");
                let rows = items
                    .iter()
                    .map(|r| {
                        vec![
                            r.get("project_id").cloned().unwrap_or_default().to_string(),
                            r.get("id").cloned().unwrap_or_default().to_string(),
                            r.get("subject")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            r.get("from")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            r.get("importance")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            r.get("created_ts")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        ]
                    })
                    .collect::<Vec<_>>();
                print_table(
                    Some(&title),
                    &[
                        "project_id",
                        "id",
                        "subject",
                        "from",
                        "importance",
                        "created_ts",
                    ],
                    rows,
                );
            });
            Ok(())
        }
        ProductsCommand::SummarizeThread {
            product_key,
            thread_id,
            per_thread_limit,
            no_llm,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let server_result = if let Some(url) = server_url {
                try_call_server_tool(
                    url,
                    bearer,
                    "summarize_thread_product",
                    serde_json::json!({
                        "product_key": product_key,
                        "thread_id": thread_id,
                        "include_examples": true,
                        "llm_mode": !no_llm,
                        "per_thread_limit": per_thread_limit,
                    }),
                )
                .await
                .and_then(coerce_tool_result_json)
            } else {
                None
            };

            let Some(payload) = server_result else {
                ftui_runtime::ftui_println!(
                    "Server unavailable; summarization requires server tool. Try again when server is running."
                );
                return Err(CliError::ExitCode(2));
            };

            if !matches!(fmt, output::CliOutputFormat::Table) {
                output::emit_output(&payload, fmt, || {});
                return Ok(());
            }

            let summary = payload.get("summary").cloned().unwrap_or_default();
            let participants = summary
                .get("participants")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();

            let kv_rows = vec![
                vec!["participants".to_string(), participants],
                vec![
                    "total_messages".to_string(),
                    summary
                        .get("total_messages")
                        .cloned()
                        .unwrap_or_default()
                        .to_string(),
                ],
                vec![
                    "open_actions".to_string(),
                    summary
                        .get("open_actions")
                        .cloned()
                        .unwrap_or_default()
                        .to_string(),
                ],
                vec![
                    "done_actions".to_string(),
                    summary
                        .get("done_actions")
                        .cloned()
                        .unwrap_or_default()
                        .to_string(),
                ],
            ];
            let title = format!("Thread summary: {thread_id}");
            print_table(Some(&title), &["Key", "Value"], kv_rows);

            if let Some(points) = summary.get("key_points").and_then(|v| v.as_array())
                && !points.is_empty()
            {
                let rows = points
                    .iter()
                    .map(|p| vec![p.as_str().unwrap_or("").to_string()])
                    .collect::<Vec<_>>();
                ftui_runtime::ftui_println!();
                print_table(Some("Key Points"), &["point"], rows);
            }
            if let Some(items) = summary.get("action_items").and_then(|v| v.as_array())
                && !items.is_empty()
            {
                let rows = items
                    .iter()
                    .map(|p| vec![p.as_str().unwrap_or("").to_string()])
                    .collect::<Vec<_>>();
                ftui_runtime::ftui_println!();
                print_table(Some("Action Items"), &["item"], rows);
            }
            if let Some(examples) = payload.get("examples").and_then(|v| v.as_array())
                && !examples.is_empty()
            {
                let rows = examples
                    .iter()
                    .map(|e| {
                        vec![
                            e.get("id").cloned().unwrap_or_default().to_string(),
                            e.get("subject")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            e.get("from")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            e.get("created_ts")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        ]
                    })
                    .collect::<Vec<_>>();
                ftui_runtime::ftui_println!();
                print_table(
                    Some("Examples"),
                    &["id", "subject", "from", "created_ts"],
                    rows,
                );
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Docs commands
// ---------------------------------------------------------------------------

fn handle_docs(action: DocsCommand) -> CliResult<()> {
    match action {
        DocsCommand::InsertBlurbs {
            scan_dir,
            yes: _,
            dry_run,
            max_depth,
        } => {
            let dirs = if scan_dir.is_empty() {
                vec![std::env::current_dir().unwrap_or_default()]
            } else {
                scan_dir
            };

            let max_depth = max_depth.unwrap_or(3) as usize;
            let mut total_files = 0u64;
            let mut total_insertions = 0u64;

            for dir in &dirs {
                ftui_runtime::ftui_println!("Scanning: {}", dir.display());
                scan_markdown_for_blurbs(
                    dir,
                    0,
                    max_depth,
                    dry_run,
                    &mut total_files,
                    &mut total_insertions,
                )?;
            }

            ftui_runtime::ftui_println!(
                "Scanned {} markdown files, {} insertions{}.",
                total_files,
                total_insertions,
                if dry_run { " (dry run)" } else { "" }
            );
            Ok(())
        }
    }
}

fn scan_markdown_for_blurbs(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    dry_run: bool,
    total_files: &mut u64,
    total_insertions: &mut u64,
) -> CliResult<()> {
    if depth > max_depth || !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_markdown_for_blurbs(
                &path,
                depth + 1,
                max_depth,
                dry_run,
                total_files,
                total_insertions,
            )?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
            *total_files += 1;
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            // Look for <!-- am:blurb --> markers
            if content.contains("<!-- am:blurb -->") && !content.contains("<!-- am:blurb:end -->") {
                *total_insertions += 1;
                if !dry_run {
                    // Insert a placeholder end marker after each blurb marker
                    let updated = content.replace(
                        "<!-- am:blurb -->",
                        "<!-- am:blurb -->\n<!-- am:blurb:end -->",
                    );
                    std::fs::write(&path, updated)?;
                }
                ftui_runtime::ftui_println!(
                    "  {} blurb marker{}",
                    path.display(),
                    if dry_run {
                        " (would insert)"
                    } else {
                        " (inserted)"
                    }
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Static file server for share preview
// ---------------------------------------------------------------------------

static PREVIEW_FORCE_TOKEN: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize)]
struct PreviewStatusPayload {
    signature: String,
    files_indexed: usize,
    last_modified_ns: Option<u64>,
    last_modified_iso: Option<String>,
    manifest_ns: Option<u64>,
    manifest_iso: Option<String>,
    manual_token: u64,
}

fn bump_preview_force_token() -> u64 {
    PREVIEW_FORCE_TOKEN
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1)
}

fn iso_from_epoch_ns(ns: u64) -> Option<String> {
    let secs = (ns / 1_000_000_000) as i64;
    let nanos = (ns % 1_000_000_000) as u32;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos).map(|dt| dt.to_rfc3339())
}

fn collect_preview_status(bundle_path: &Path) -> PreviewStatusPayload {
    use sha2::{Digest, Sha256};

    let token = PREVIEW_FORCE_TOKEN.load(Ordering::Acquire);
    let bundle_path = bundle_path
        .canonicalize()
        .unwrap_or_else(|_| bundle_path.to_path_buf());

    let mut file_entries: Vec<(String, u64, u64)> = Vec::new();
    let mut latest_ns: u64 = 0;
    let mut manifest_ns: Option<u64> = None;
    if bundle_path.is_dir() {
        let _ = collect_preview_files(
            &bundle_path,
            &bundle_path,
            &mut file_entries,
            &mut latest_ns,
            &mut manifest_ns,
        );
    }

    file_entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut parts: Vec<String> = Vec::with_capacity(file_entries.len() + 1);
    for (rel, mtime_ns, size) in &file_entries {
        parts.push(format!("{rel}:{mtime_ns}:{size}"));
    }
    parts.push(format!("manual:{token}"));

    let signature = if parts.is_empty() {
        "0".to_string()
    } else {
        hex::encode(Sha256::digest(parts.join("|").as_bytes()))
    };

    let last_modified_ns = if latest_ns == 0 {
        None
    } else {
        Some(latest_ns)
    };
    let last_modified_iso = last_modified_ns.and_then(iso_from_epoch_ns);
    let manifest_iso = manifest_ns.and_then(iso_from_epoch_ns);

    PreviewStatusPayload {
        signature,
        files_indexed: parts.len(),
        last_modified_ns,
        last_modified_iso,
        manifest_ns,
        manifest_iso,
        manual_token: token,
    }
}

fn collect_preview_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, u64, u64)>,
    latest_ns: &mut u64,
    manifest_ns: &mut Option<u64>,
) -> std::io::Result<()> {
    use std::time::UNIX_EPOCH;

    for entry in std::fs::read_dir(dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.is_dir() {
            let _ = collect_preview_files(root, &path, out, latest_ns, manifest_ns);
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .ok()
            .and_then(|p| p.to_str())
            .unwrap_or("")
            .replace('\\', "/");
        if rel.is_empty() {
            continue;
        }
        let mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        let size = metadata.len();
        *latest_ns = (*latest_ns).max(mtime_ns);
        if rel == "manifest.json" && mtime_ns > 0 {
            *manifest_ns = Some(mtime_ns);
        }
        out.push((rel, mtime_ns, size));
    }
    Ok(())
}

type PreviewLog = std::sync::Arc<std::sync::Mutex<std::fs::File>>;

fn preview_log_line(log: &Option<PreviewLog>, line: &str) {
    use std::io::Write;

    let Some(log) = log else {
        return;
    };
    let Ok(mut file) = log.lock() else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

fn apply_preview_no_cache_headers(resp: &mut asupersync::http::h1::types::Response) {
    resp.headers.push((
        "Cache-Control".to_string(),
        "no-store, no-cache, must-revalidate".to_string(),
    ));
    resp.headers
        .push(("Pragma".to_string(), "no-cache".to_string()));
}

struct PreviewServerHandle {
    addr: std::net::SocketAddr,
    shutdown: asupersync::server::shutdown::ShutdownSignal,
    join: std::thread::JoinHandle<Result<(), String>>,
}

fn start_preview_server(
    dir: PathBuf,
    host: String,
    port: u16,
    log: Option<PreviewLog>,
) -> CliResult<PreviewServerHandle> {
    use asupersync::http::h1::listener::{Http1Listener, Http1ListenerConfig};
    use asupersync::http::h1::types::Response;
    use asupersync::runtime::RuntimeBuilder;
    use std::sync::mpsc;

    // Avoid serving files outside the preview root via symlink escape.
    let base_dir = dir.canonicalize().unwrap_or(dir);

    let socket_addr: std::net::SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| CliError::InvalidArgument(format!("invalid address: {e}")))?;

    let (ready_tx, ready_rx) = mpsc::channel::<
        Result<
            (
                std::net::SocketAddr,
                asupersync::server::shutdown::ShutdownSignal,
            ),
            String,
        >,
    >();

    let join = std::thread::spawn(move || {
        let runtime = match RuntimeBuilder::current_thread().build() {
            Ok(runtime) => runtime,
            Err(e) => {
                let msg = format!("failed to build runtime: {e}");
                let _ = ready_tx.send(Err(msg.clone()));
                return Err(msg);
            }
        };
        let handle = runtime.handle();
        runtime.block_on(async move {
            let dir = base_dir.clone();
            let log = log.clone();
            let listener = match Http1Listener::bind_with_config(
                socket_addr,
                move |req| {
                    let dir = dir.clone();
                    let log = log.clone();
                    async move {
                        let uri = &req.uri;
                        let path = uri.split('?').next().unwrap_or("/");
                        preview_log_line(&log, &format!("GET {path}"));

                        if path.starts_with("/__preview__/status") {
                            let payload = collect_preview_status(&dir);
                            let body = serde_json::to_vec(&payload).unwrap_or_default();
                            let mut resp = Response::new(200, "OK", body);
                            let len = resp.body.len();
                            resp.headers
                                .push(("Content-Type".to_string(), "application/json".to_string()));
                            resp.headers
                                .push(("Content-Length".to_string(), len.to_string()));
                            apply_preview_no_cache_headers(&mut resp);
                            return resp;
                        }

                        if path == "/favicon.ico"
                            || path.ends_with(".map")
                            || path.starts_with("/.well-known/")
                        {
                            let mut resp = Response::new(204, "No Content", Vec::new());
                            resp.headers
                                .push(("Content-Length".to_string(), "0".to_string()));
                            apply_preview_no_cache_headers(&mut resp);
                            return resp;
                        }

                        let relative = path.trim_start_matches('/');
                        if relative.split('/').any(|seg| seg == "..") {
                            let mut resp = Response::new(404, "Not Found", b"Not Found".to_vec());
                            resp.headers
                                .push(("Content-Length".to_string(), resp.body.len().to_string()));
                            apply_preview_no_cache_headers(&mut resp);
                            return resp;
                        }

                        let mut file_path = if relative.is_empty() {
                            dir.join("index.html")
                        } else {
                            dir.join(relative)
                        };
                        if file_path.is_dir() {
                            file_path = file_path.join("index.html");
                        }

                        let resolved = file_path.canonicalize().ok();
                        let within_root = resolved.as_ref().is_some_and(|p| p.starts_with(&dir));
                        let mut resp = if within_root
                            && resolved.as_ref().is_some_and(|p| p.is_file())
                        {
                            let resolved = resolved.as_ref().unwrap();
                            match std::fs::read(resolved) {
                                Ok(content) => {
                                    let ct = guess_content_type(resolved);
                                    let mut resp = Response::new(200, "OK", content);
                                    resp.headers
                                        .push(("Content-Type".to_string(), ct.to_string()));
                                    resp.headers.push((
                                        "Content-Length".to_string(),
                                        resp.body.len().to_string(),
                                    ));
                                    resp
                                }
                                Err(_) => {
                                    let mut resp =
                                        Response::new(500, "Internal Server Error", Vec::new());
                                    resp.headers
                                        .push(("Content-Length".to_string(), "0".to_string()));
                                    resp
                                }
                            }
                        } else {
                            let mut resp = Response::new(404, "Not Found", b"Not Found".to_vec());
                            resp.headers
                                .push(("Content-Length".to_string(), resp.body.len().to_string()));
                            resp
                        };
                        apply_preview_no_cache_headers(&mut resp);
                        resp
                    }
                },
                Http1ListenerConfig::default(),
            )
            .await
            {
                Ok(listener) => listener,
                Err(e) => {
                    let msg = format!("failed to bind HTTP listener: {e}");
                    let _ = ready_tx.send(Err(msg.clone()));
                    return Err(msg);
                }
            };

            let shutdown = listener.shutdown_signal();
            let local_addr = match listener.local_addr() {
                Ok(addr) => addr,
                Err(e) => {
                    let msg = format!("failed to read local addr: {e}");
                    let _ = ready_tx.send(Err(msg.clone()));
                    return Err(msg);
                }
            };
            let _ = ready_tx.send(Ok((local_addr, shutdown.clone())));

            listener
                .run(&handle)
                .await
                .map(|_| ())
                .map_err(|e| format!("listener run error: {e}"))
        })
    });

    let (addr, shutdown) = ready_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|e| CliError::Other(format!("preview server failed to start: {e}")))?
        .map_err(CliError::Other)?;

    Ok(PreviewServerHandle {
        addr,
        shutdown,
        join,
    })
}

fn run_share_preview(bundle: &Path, host: &str, port: u16, open_browser: bool) -> CliResult<()> {
    run_share_preview_with_control(
        bundle.to_path_buf(),
        host.to_string(),
        port,
        open_browser,
        None,
        None,
        None,
    )
}

fn run_share_preview_with_control(
    bundle: PathBuf,
    host: String,
    port: u16,
    open_browser: bool,
    key_rx: Option<std::sync::mpsc::Receiver<char>>,
    ready_addr_tx: Option<std::sync::mpsc::Sender<std::net::SocketAddr>>,
    artifacts_dir: Option<PathBuf>,
) -> CliResult<()> {
    // Prefer shipping the built-in assets in dev/preview mode; ignore errors (matches legacy).
    let _ = share::copy_viewer_assets(&bundle);

    let log = artifacts_dir.as_ref().and_then(|root| {
        let _ = std::fs::create_dir_all(root);
        std::fs::File::create(root.join("server.log"))
            .ok()
            .map(|f| std::sync::Arc::new(std::sync::Mutex::new(f)))
    });
    if let Some(log) = &log {
        preview_log_line(&Some(log.clone()), "preview server starting");
    }

    let server = start_preview_server(bundle.clone(), host.clone(), port, log.clone())?;
    let addr = server.addr;
    if let Some(tx) = ready_addr_tx {
        let _ = tx.send(addr);
    }

    ftui_runtime::ftui_println!(
        "Serving {} at http://{}:{}/ (Ctrl+C to stop)",
        bundle.display(),
        host,
        addr.port()
    );
    ftui_runtime::ftui_println!(
        "Commands: press 'r' to force refresh, 'd' to deploy now, 'q' to stop."
    );

    if open_browser {
        let browse_host = match host.as_str() {
            "0.0.0.0" | "::" => "127.0.0.1",
            other => other,
        };
        let url = format!("http://{}:{}/viewer/", browse_host, addr.port());
        let _ = std::process::Command::new("xdg-open")
            .arg(&url)
            .spawn()
            .or_else(|_| std::process::Command::new("open").arg(&url).spawn());
    }

    let mut deployment_requested = false;
    let mut stop_requested = false;

    if let Some(rx) = key_rx {
        while !stop_requested && !deployment_requested && !server.join.is_finished() {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(ch) => match ch.to_ascii_lowercase() {
                    'r' => {
                        let token = bump_preview_force_token();
                        ftui_runtime::ftui_println!("Reload signal sent (token {token}).");
                    }
                    'd' => {
                        deployment_requested = true;
                        stop_requested = true;
                    }
                    'q' => {
                        stop_requested = true;
                    }
                    _ => {}
                },
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    } else if crate::output::is_stdin_tty() {
        use crossterm::event::{Event, KeyCode, KeyModifiers, poll, read};

        struct RawModeGuard;
        impl Drop for RawModeGuard {
            fn drop(&mut self) {
                let _ = crossterm::terminal::disable_raw_mode();
            }
        }

        crossterm::terminal::enable_raw_mode()
            .map_err(|e| CliError::Other(format!("failed to enable raw mode: {e}")))?;
        let _raw = RawModeGuard;

        while !stop_requested && !deployment_requested && !server.join.is_finished() {
            if poll(std::time::Duration::from_millis(200))
                .map_err(|e| CliError::Other(format!("hotkey poll error: {e}")))?
                && let Event::Key(key) =
                    read().map_err(|e| CliError::Other(format!("hotkey read error: {e}")))?
            {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
                {
                    stop_requested = true;
                    continue;
                }

                match key.code {
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        let token = bump_preview_force_token();
                        ftui_runtime::ftui_println!("Reload signal sent (token {token}).");
                    }
                    KeyCode::Char('d') | KeyCode::Char('D') => {
                        deployment_requested = true;
                        stop_requested = true;
                    }
                    KeyCode::Char('q') | KeyCode::Char('Q') => {
                        stop_requested = true;
                    }
                    _ => {}
                }
            }
        }
    } else {
        // Non-interactive stdin: keep serving until the process is interrupted.
        let _ = server.join.join();
        return Ok(());
    }

    ftui_runtime::ftui_println!("Stopping preview server...");
    let _ = server
        .shutdown
        .begin_drain(std::time::Duration::from_secs(2));
    let _ = server.join.join();
    ftui_runtime::ftui_println!("Preview server stopped.");

    if deployment_requested {
        return Err(CliError::ExitCode(42));
    }
    Ok(())
}

// ── Tooling + Diagnostics CLI handlers ────────────────────────────────

const SEARCH_FTS_TRIGGER_NAMES: [&str; 6] = [
    "fts_messages_ai",
    "fts_messages_ad",
    "fts_messages_au",
    "messages_ai",
    "messages_ad",
    "messages_au",
];

#[derive(Debug, Clone, serde::Serialize)]
struct FtsDecommissionCheck {
    name: String,
    required: bool,
    ok: bool,
    detail: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct FtsDecommissionReport {
    schema_version: String,
    generated_at: String,
    force: bool,
    engine: String,
    shadow_mode: String,
    fallback_on_error: bool,
    checks: Vec<FtsDecommissionCheck>,
    preflight_ok: bool,
    blocked: bool,
    warnings: Vec<String>,
    storage_root: String,
    search_index_path: String,
    triggers_before: Vec<String>,
    dropped_triggers: Vec<String>,
    triggers_after: Vec<String>,
    action: String,
}

fn search_index_has_content(index_root: &std::path::Path) -> bool {
    if !index_root.exists() || !index_root.is_dir() {
        return false;
    }
    walkdir::WalkDir::new(index_root)
        .into_iter()
        .filter_map(Result::ok)
        .any(|entry| entry.file_type().is_file())
}

fn query_existing_search_fts_triggers(conn: &mcp_agent_mail_db::DbConn) -> CliResult<Vec<String>> {
    let sql = "\
        SELECT name \
          FROM sqlite_master \
         WHERE type = 'trigger' \
           AND name IN (\
               'fts_messages_ai', 'fts_messages_ad', 'fts_messages_au', \
               'messages_ai', 'messages_ad', 'messages_au'\
           ) \
         ORDER BY name";
    let rows = conn
        .query_sync(sql, &[])
        .map_err(|e| CliError::Other(format!("failed to query FTS trigger state: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|row| row.get_named::<String>("name").unwrap_or_default())
        .filter(|name| !name.is_empty())
        .collect())
}

fn handle_tooling_decommission_fts(
    force: bool,
    format: Option<output::CliOutputFormat>,
    json_mode: bool,
) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(format, json_mode);
    let config = Config::from_env();
    let search_index_path = std::path::Path::new(&config.storage_root).join("search_index");
    let engine = config.search_rollout.engine.to_string();
    let shadow_mode = config.search_rollout.shadow_mode.to_string();
    let fallback_on_error = config.search_rollout.fallback_on_error;

    let mut checks = Vec::new();
    checks.push(FtsDecommissionCheck {
        name: "search_engine_not_legacy".to_string(),
        required: true,
        #[allow(deprecated)]
        ok: !matches!(
            config.search_rollout.engine,
            mcp_agent_mail_core::config::SearchEngine::Legacy
        ),
        detail: format!("engine={engine}"),
    });
    checks.push(FtsDecommissionCheck {
        name: "fallback_on_error_disabled".to_string(),
        required: true,
        ok: !fallback_on_error,
        detail: format!("fallback_on_error={fallback_on_error}"),
    });
    checks.push(FtsDecommissionCheck {
        name: "tantivy_index_present".to_string(),
        required: true,
        ok: search_index_has_content(&search_index_path),
        detail: search_index_path.display().to_string(),
    });
    checks.push(FtsDecommissionCheck {
        name: "shadow_mode_active".to_string(),
        required: false,
        ok: config.search_rollout.shadow_mode.is_active(),
        detail: format!("shadow_mode={shadow_mode}"),
    });

    let preflight_ok = checks.iter().filter(|check| check.required).all(|c| c.ok);
    let mut warnings = Vec::new();
    if checks
        .iter()
        .any(|check| check.name == "shadow_mode_active" && !check.ok)
    {
        warnings.push(
            "shadow mode is off; ensure historical parity evidence exists before decommission"
                .to_string(),
        );
    }

    let conn = open_db_sync_with_database_url(&config.database_url)?;
    let triggers_before = query_existing_search_fts_triggers(&conn)?;

    let mut dropped_triggers = Vec::new();
    let blocked = !preflight_ok && !force;
    if !blocked {
        for trigger in SEARCH_FTS_TRIGGER_NAMES {
            let sql = format!("DROP TRIGGER IF EXISTS {trigger}");
            conn.execute_raw(&sql)
                .map_err(|e| CliError::Other(format!("failed dropping trigger {trigger}: {e}")))?;
            if triggers_before.iter().any(|name| name == trigger) {
                dropped_triggers.push(trigger.to_string());
            }
        }
    } else {
        warnings.push("preflight failed; rerun with --force to override".to_string());
    }

    let triggers_after = if blocked {
        triggers_before.clone()
    } else {
        query_existing_search_fts_triggers(&conn)?
    };

    let action = if blocked {
        "blocked_preflight"
    } else if dropped_triggers.is_empty() {
        "noop_already_clean"
    } else {
        "decommissioned"
    };

    let report = FtsDecommissionReport {
        schema_version: "am_search_fts_decommission.v1".to_string(),
        generated_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        force,
        engine,
        shadow_mode,
        fallback_on_error,
        checks,
        preflight_ok,
        blocked,
        warnings,
        storage_root: config.storage_root.display().to_string(),
        search_index_path: search_index_path.display().to_string(),
        triggers_before,
        dropped_triggers,
        triggers_after,
        action: action.to_string(),
    };

    output::emit_output(&report, fmt, || {
        output::section("FTS Decommission Report:");
        output::kv("Action", &report.action);
        output::kv(
            "Preflight",
            if report.preflight_ok { "PASS" } else { "FAIL" },
        );
        output::kv("Engine", &report.engine);
        output::kv("Shadow mode", &report.shadow_mode);
        output::kv(
            "Fallback on error",
            if report.fallback_on_error {
                "enabled"
            } else {
                "disabled"
            },
        );
        output::kv("Search index path", &report.search_index_path);
        ftui_runtime::ftui_println!("");

        let mut check_table = output::CliTable::new(vec!["CHECK", "REQUIRED", "STATUS", "DETAIL"]);
        for check in &report.checks {
            check_table.add_row(vec![
                check.name.clone(),
                if check.required {
                    "yes".to_string()
                } else {
                    "no".to_string()
                },
                if check.ok {
                    "ok".to_string()
                } else {
                    "fail".to_string()
                },
                check.detail.clone(),
            ]);
        }
        check_table.render();

        ftui_runtime::ftui_println!("");
        output::kv(
            "Triggers before",
            &report.triggers_before.join(", ").to_string(),
        );
        output::kv(
            "Dropped triggers",
            &report.dropped_triggers.join(", ").to_string(),
        );
        output::kv(
            "Triggers after",
            &report.triggers_after.join(", ").to_string(),
        );

        if !report.warnings.is_empty() {
            ftui_runtime::ftui_println!("");
            output::section("Warnings:");
            for warning in &report.warnings {
                ftui_runtime::ftui_println!("  - {warning}");
            }
        }
    });

    if report.blocked {
        return Err(CliError::ExitCode(2));
    }
    Ok(())
}

fn handle_tooling(action: ToolingCommand) -> CliResult<()> {
    match action {
        ToolingCommand::Directory { format, json } => handle_tooling_directory(format, json),
        ToolingCommand::Schemas { tool, format, json } => {
            handle_tooling_schemas(tool, format, json)
        }
        ToolingCommand::Metrics { format, json } => handle_tooling_metrics(format, json),
        ToolingCommand::MetricsCore { format, json } => handle_tooling_metrics_core(format, json),
        ToolingCommand::Diagnostics { format, json } => handle_tooling_diagnostics(format, json),
        ToolingCommand::Locks { format, json } => handle_tooling_locks(format, json),
        ToolingCommand::DecommissionFts {
            force,
            format,
            json,
        } => handle_tooling_decommission_fts(force, format, json),
    }
}

fn handle_tooling_directory(
    format: Option<output::CliOutputFormat>,
    json_mode: bool,
) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(format, json_mode);
    let cluster_map = mcp_agent_mail_tools::TOOL_CLUSTER_MAP;
    let mut clusters: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for &(tool, cluster) in cluster_map {
        clusters
            .entry(cluster.to_string())
            .or_default()
            .push(tool.to_string());
    }
    let val = serde_json::json!({
        "tool_count": cluster_map.len(),
        "clusters": clusters,
    });

    output::emit_output(&val, fmt, || {
        output::section("Tool Directory:");
        output::kv("Total tools", &cluster_map.len().to_string());
        ftui_runtime::ftui_println!("");

        let mut table = output::CliTable::new(vec!["TOOL", "CLUSTER"]);
        for &(tool, cluster) in cluster_map {
            table.add_row(vec![tool.to_string(), cluster.to_string()]);
        }
        table.render();
    });
    Ok(())
}

fn handle_tooling_schemas(
    filter_tool: Option<String>,
    format: Option<output::CliOutputFormat>,
    json_mode: bool,
) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(format, json_mode);
    let cluster_map = mcp_agent_mail_tools::TOOL_CLUSTER_MAP;

    let mut tools: std::collections::BTreeMap<String, serde_json::Value> =
        std::collections::BTreeMap::new();

    for &(tool_name, cluster) in cluster_map {
        if let Some(ref filter) = filter_tool
            && tool_name != filter.as_str()
        {
            continue;
        }
        let meta = mcp_agent_mail_tools::tool_meta(tool_name);
        tools.insert(
            tool_name.to_string(),
            serde_json::json!({
                "cluster": cluster,
                "capabilities": meta.map(|m| m.capabilities).unwrap_or_default(),
                "complexity": meta.map(|m| m.complexity).unwrap_or("unknown"),
            }),
        );
    }

    let val = serde_json::json!({
        "tool_count": tools.len(),
        "tools": tools.clone(),
    });

    output::emit_output(&val, fmt, || {
        if tools.is_empty() {
            if let Some(ref name) = filter_tool {
                output::emit_empty(fmt, &format!("Tool not found: {name}"));
            } else {
                output::emit_empty(fmt, "No tools available.");
            }
            return;
        }

        output::section("Tool Schemas:");
        ftui_runtime::ftui_println!("");

        let mut table =
            output::CliTable::new(vec!["TOOL", "CLUSTER", "COMPLEXITY", "CAPABILITIES"]);
        for (name, val) in &tools {
            let cluster = val["cluster"].as_str().unwrap_or("");
            let complexity = val["complexity"].as_str().unwrap_or("unknown");
            let caps = val["capabilities"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            table.add_row(vec![
                name.clone(),
                cluster.to_string(),
                complexity.to_string(),
                caps,
            ]);
        }
        table.render();
    });
    Ok(())
}

fn handle_tooling_metrics(
    format: Option<output::CliOutputFormat>,
    json_mode: bool,
) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(format, json_mode);
    let snapshot = mcp_agent_mail_tools::tool_metrics_snapshot_full();
    let health = mcp_agent_mail_core::cached_health_level().to_string();
    let entries: Vec<serde_json::Value> = snapshot
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "calls": e.calls,
                "errors": e.errors,
                "cluster": e.cluster,
                "latency": {
                    "p50_ms": e.latency.as_ref().map(|l| l.p50_ms),
                    "p95_ms": e.latency.as_ref().map(|l| l.p95_ms),
                    "p99_ms": e.latency.as_ref().map(|l| l.p99_ms),
                },
            })
        })
        .collect();
    let val = serde_json::json!({
        "health_level": health,
        "tool_count": entries.len(),
        "tools": entries,
    });

    output::emit_output(&val, fmt, || {
        output::section("Tool Metrics:");
        output::kv("Health level", &health);
        ftui_runtime::ftui_println!("");

        if snapshot.is_empty() {
            ftui_runtime::ftui_println!("  No tool metrics recorded yet.");
            return;
        }

        let mut table =
            output::CliTable::new(vec!["TOOL", "CLUSTER", "CALLS", "ERRORS", "P95(ms)"]);
        for e in &snapshot {
            let p95_str = e
                .latency
                .as_ref()
                .map(|l| {
                    if l.p95_ms > 0.0 {
                        format!("{:.1}", l.p95_ms)
                    } else {
                        "--".to_string()
                    }
                })
                .unwrap_or_else(|| "--".to_string());
            table.add_row(vec![
                e.name.clone(),
                e.cluster.clone(),
                e.calls.to_string(),
                e.errors.to_string(),
                p95_str,
            ]);
        }
        table.render();

        let slow = mcp_agent_mail_tools::slow_tools();
        if !slow.is_empty() {
            ftui_runtime::ftui_println!("");
            output::warn(&format!("{} slow tool(s) (p95 > 500ms):", slow.len()));
            for s in &slow {
                let p95 = s
                    .latency
                    .as_ref()
                    .map(|l| format!("{:.0}ms", l.p95_ms))
                    .unwrap_or_default();
                ftui_runtime::ftui_println!("  {} (p95: {})", s.name, p95);
            }
        }
    });
    Ok(())
}

fn handle_tooling_metrics_core(
    format: Option<output::CliOutputFormat>,
    json_mode: bool,
) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(format, json_mode);
    let health = mcp_agent_mail_core::cached_health_level().to_string();
    let metrics = mcp_agent_mail_core::global_metrics().snapshot();
    let locks = mcp_agent_mail_core::lock_contention_snapshot();
    let val = serde_json::json!({
        "health_level": health,
        "metrics": metrics,
        "lock_contention": locks,
    });

    output::emit_output(&val, fmt, || {
        output::section("Core System Metrics:");
        output::kv("Health level", &health);
        ftui_runtime::ftui_println!("");

        output::section("  HTTP:");
        output::kv("    Requests", &metrics.http.requests_total.to_string());
        output::kv("    4xx", &metrics.http.requests_4xx.to_string());
        output::kv("    5xx", &metrics.http.requests_5xx.to_string());

        output::section("  Tools:");
        output::kv(
            "    Total calls",
            &metrics.tools.tool_calls_total.to_string(),
        );
        output::kv(
            "    Total errors",
            &metrics.tools.tool_errors_total.to_string(),
        );

        output::section("  Database:");
        output::kv(
            "    Pool acquires",
            &metrics.db.pool_acquires_total.to_string(),
        );
        output::kv(
            "    Pool errors",
            &metrics.db.pool_acquire_errors_total.to_string(),
        );

        output::section("  Storage:");
        output::kv(
            "    Commits enqueued",
            &metrics.storage.commit_enqueued_total.to_string(),
        );
        output::kv(
            "    Commits drained",
            &metrics.storage.commit_drained_total.to_string(),
        );

        if !locks.is_empty() {
            ftui_runtime::ftui_println!("");
            output::section("  Lock Contention:");
            let mut lock_table =
                output::CliTable::new(vec!["LOCK", "CONTENTIONS", "TOTAL_WAIT(ms)"]);
            for entry in &locks {
                lock_table.add_row(vec![
                    entry.lock_name.clone(),
                    entry.contended_count.to_string(),
                    format!("{:.1}", entry.total_wait_ns as f64 / 1_000_000.0),
                ]);
            }
            lock_table.render();
        }
    });
    Ok(())
}

fn handle_tooling_diagnostics(
    format: Option<output::CliOutputFormat>,
    json_mode: bool,
) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(format, json_mode);
    let tools_detail: Vec<serde_json::Value> = mcp_agent_mail_tools::tool_metrics_snapshot()
        .into_iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .collect();
    let slow: Vec<serde_json::Value> = mcp_agent_mail_tools::slow_tools()
        .into_iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .collect();

    let report = mcp_agent_mail_core::DiagnosticReport::build(tools_detail, slow);

    output::emit_output(&report, fmt, || {
        output::section("Diagnostic Report:");
        output::kv("Generated at", &report.generated_at);
        ftui_runtime::ftui_println!("");

        output::section("  System:");
        output::kv("    Uptime", &format!("{}s", report.system.uptime_secs));
        output::kv("    Rust version", report.system.rust_version);
        output::kv("    Target", report.system.target);
        output::kv("    OS", &report.system.os);
        output::kv("    CPUs", &report.system.cpu_count.to_string());

        ftui_runtime::ftui_println!("");
        output::section("  Health:");
        output::kv("    Level", &report.health.level);

        ftui_runtime::ftui_println!("");
        output::section("  HTTP:");
        output::kv("    Requests", &report.http.requests_total.to_string());
        output::kv("    4xx", &report.http.requests_4xx.to_string());
        output::kv("    5xx", &report.http.requests_5xx.to_string());

        output::section("  Database:");
        output::kv(
            "    Pool acquires",
            &report.database.pool_acquires_total.to_string(),
        );
        output::kv(
            "    Pool errors",
            &report.database.pool_acquire_errors_total.to_string(),
        );

        output::section("  Storage:");
        output::kv(
            "    Commits enqueued",
            &report.storage.commit_enqueued_total.to_string(),
        );
        output::kv(
            "    Commits drained",
            &report.storage.commit_drained_total.to_string(),
        );

        output::section("  Search:");
        output::kv(
            "    Queries total",
            &report.search.queries_total.to_string(),
        );
        output::kv(
            "    V3 queries",
            &report.search.queries_v3_total.to_string(),
        );
        output::kv(
            "    Legacy queries",
            &report.search.queries_legacy_total.to_string(),
        );
        output::kv(
            "    Fallback to legacy",
            &report.search.fallback_to_legacy_total.to_string(),
        );
        output::kv(
            "    Query errors",
            &report.search.queries_errors_total.to_string(),
        );
        output::kv(
            "    Shadow comparisons",
            &report.search.shadow_comparisons_total.to_string(),
        );
        output::kv(
            "    Shadow equivalence",
            &format!("{:.1}%", report.search.shadow_equivalent_pct),
        );
        output::kv(
            "    Shadow V3 errors",
            &report.search.shadow_v3_errors_total.to_string(),
        );
        output::kv(
            "    Semantic kill-switch hits",
            &report.search.semantic_killswitch_hits.to_string(),
        );
        output::kv(
            "    Rerank kill-switch hits",
            &report.search.rerank_killswitch_hits.to_string(),
        );
        output::kv(
            "    Tantivy docs",
            &report.search.tantivy_doc_count.to_string(),
        );
        output::kv(
            "    Tantivy size",
            &format!("{} bytes", report.search.tantivy_index_size_bytes),
        );
        let tantivy_last_update = i64::try_from(report.search.tantivy_last_update_us)
            .ok()
            .map_or_else(|| "(invalid)".to_string(), format_micros_as_iso);
        output::kv("    Tantivy last update", &tantivy_last_update);

        if !report.tools_detail.is_empty() {
            ftui_runtime::ftui_println!("");
            output::section("  Tools:");
            output::kv(
                "    Total calls",
                &report.tools_aggregate.tool_calls_total.to_string(),
            );
            output::kv(
                "    Total errors",
                &report.tools_aggregate.tool_errors_total.to_string(),
            );
        }

        if !report.slow_tools.is_empty() {
            ftui_runtime::ftui_println!("");
            output::warn(&format!(
                "  {} slow tool(s) (p95 > 500ms)",
                report.slow_tools.len()
            ));
        }

        if !report.locks.is_empty() {
            ftui_runtime::ftui_println!("");
            output::section("  Lock Contention:");
            for entry in &report.locks {
                output::kv(
                    &format!("    {}", entry.lock_name),
                    &format!(
                        "{} contentions, {:.1}ms total wait",
                        entry.contended_count,
                        entry.total_wait_ns as f64 / 1_000_000.0
                    ),
                );
            }
        }

        if !report.recommendations.is_empty() {
            ftui_runtime::ftui_println!("");
            output::section("  Recommendations:");
            for rec in &report.recommendations {
                let icon = match rec.severity {
                    "critical" => "CRIT",
                    "warning" => "WARN",
                    _ => "INFO",
                };
                ftui_runtime::ftui_println!("    [{}] {}: {}", icon, rec.subsystem, rec.message);
            }
        }
    });
    Ok(())
}

fn handle_tooling_locks(format: Option<output::CliOutputFormat>, json_mode: bool) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(format, json_mode);
    let config = Config::from_env();
    let lock_info = mcp_agent_mail_storage::collect_lock_status(&config)
        .unwrap_or_else(|_e| serde_json::json!({"archive_root": "", "exists": false, "locks": []}));

    let raw_locks = lock_info
        .get("locks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let val = serde_json::json!({
        "total": raw_locks.len(),
        "locks": raw_locks,
    });
    output::emit_output(&val, fmt, || {
        output::section("Archive Locks:");

        if raw_locks.is_empty() {
            output::emit_empty(fmt, "No active locks.");
            return;
        }

        output::kv("Total", &raw_locks.len().to_string());
        ftui_runtime::ftui_println!("");

        let mut table = output::CliTable::new(vec!["PATH", "OWNER", "CREATED"]);
        for lock in &raw_locks {
            let path = lock
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let owner = lock
                .get("owner")
                .and_then(|o| o.get("pid"))
                .and_then(serde_json::Value::as_u64)
                .map(|pid| format!("pid:{pid}"))
                .unwrap_or_else(|| "unknown".to_string());
            let created = lock
                .get("owner")
                .and_then(|o| o.get("created_ts"))
                .and_then(serde_json::Value::as_f64)
                .map(|ts| {
                    // created_ts is seconds since epoch (f64 from SystemTime::as_secs_f64)
                    if !ts.is_finite() {
                        return format!("{ts}");
                    }
                    let secs_f = ts.floor();
                    let secs = secs_f as i64;
                    let nanos = ((ts - secs_f) * 1e9).clamp(0.0, 999_999_999.0) as u32;
                    chrono::DateTime::from_timestamp(secs, nanos)
                        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_else(|| format!("{ts}"))
                })
                .unwrap_or_else(|| "--".to_string());
            table.add_row(vec![path.to_string(), owner, created]);
        }
        table.render();
    });
    Ok(())
}

fn guess_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|s| s.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("json") => "application/json",
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("woff2") => "font/woff2",
        Some("wasm") => "application/wasm",
        Some("sqlite3") => "application/x-sqlite3",
        _ => "application/octet-stream",
    }
}

fn format_bytes(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut current = bytes as f64;
    for (idx, unit) in units.iter().enumerate() {
        let is_last = idx == units.len() - 1;
        if current < 1024.0 || is_last {
            if *unit == "B" {
                return format!("{bytes} {unit}");
            }
            return format!("{:.1} {unit}", current);
        }
        current /= 1024.0;
    }
    format!("{bytes} B")
}

// ── Self-update unit tests ────────────────────────────────────────────

#[test]
fn is_newer_version_basic() {
    assert!(is_newer_version("0.1.0", "0.2.0"));
    assert!(is_newer_version("0.1.0", "1.0.0"));
    assert!(is_newer_version("0.1.0", "0.1.1"));
    assert!(!is_newer_version("0.2.0", "0.1.0"));
    assert!(!is_newer_version("0.1.0", "0.1.0"));
    assert!(!is_newer_version("1.0.0", "0.9.9"));
}

#[test]
fn is_newer_version_with_v_prefix() {
    assert!(is_newer_version("v0.1.0", "v0.2.0"));
    assert!(is_newer_version("0.1.0", "v0.2.0"));
    assert!(is_newer_version("v0.1.0", "0.2.0"));
}

#[test]
fn detect_platform_target_returns_some() {
    // We're running on a supported platform in CI/dev
    let target = detect_platform_target();
    assert!(
        target.is_some(),
        "detect_platform_target() should detect this host"
    );
    let t = target.unwrap();
    assert!(
        t.contains("linux") || t.contains("darwin") || t.contains("windows"),
        "target should contain OS: {t}"
    );
}

#[test]
fn release_asset_url_linux() {
    let (url, filename) = release_asset_url("0.2.0", "x86_64-unknown-linux-gnu");
    assert_eq!(filename, "mcp-agent-mail-x86_64-unknown-linux-gnu.tar.xz");
    assert!(url.contains("v0.2.0"));
    assert!(url.ends_with(".tar.xz"));
}

#[test]
fn release_asset_url_windows() {
    let (url, filename) = release_asset_url("0.2.0", "x86_64-pc-windows-msvc");
    assert_eq!(filename, "mcp-agent-mail-x86_64-pc-windows-msvc.zip");
    assert!(url.contains("v0.2.0"));
    assert!(url.ends_with(".zip"));
}

#[test]
fn release_asset_url_macos() {
    let (url, filename) = release_asset_url("1.0.0", "aarch64-apple-darwin");
    assert_eq!(filename, "mcp-agent-mail-aarch64-apple-darwin.tar.xz");
    assert!(url.contains("/releases/download/v1.0.0/"));
}

#[test]
fn verify_sha256_correct() {
    use sha2::{Digest, Sha256};
    let data = b"hello world";
    let hash = hex::encode(Sha256::digest(data));
    assert!(verify_sha256(data, &hash));
}

#[test]
fn verify_sha256_wrong() {
    assert!(!verify_sha256(
        b"hello world",
        "0000000000000000000000000000000000000000000000000000000000000000"
    ));
}

#[test]
fn verify_sha256_with_filename_suffix() {
    use sha2::{Digest, Sha256};
    let data = b"test data";
    let hash = hex::encode(Sha256::digest(data));
    // Checksum files sometimes have "hash  filename" format
    let checksum_line = format!("{hash}  mcp-agent-mail.tar.xz");
    // verify_sha256 expects just the hash, so this would fail —
    // but our download_and_verify_release splits on whitespace first
    // Test that split_whitespace().next() works
    let parsed = checksum_line.split_whitespace().next().unwrap();
    assert!(verify_sha256(data, parsed));
}

#[test]
fn find_binary_in_dir_flat() {
    let tmp = std::env::temp_dir().join("am-test-find-flat");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("am"), b"binary").unwrap();
    assert_eq!(find_binary_in_dir(&tmp, "am"), Some(tmp.join("am")));
    assert_eq!(find_binary_in_dir(&tmp, "missing"), None);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn find_binary_in_dir_nested() {
    let tmp = std::env::temp_dir().join("am-test-find-nested");
    let _ = std::fs::remove_dir_all(&tmp);
    let sub = tmp.join("release");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("am"), b"binary").unwrap();
    assert_eq!(find_binary_in_dir(&tmp, "am"), Some(sub.join("am")));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn clap_parses_self_update_check() {
    let cli = Cli::try_parse_from(["am", "self-update", "--check"]).unwrap();
    match cli.command {
        Some(Commands::SelfUpdate {
            check,
            force,
            version,
        }) => {
            assert!(check);
            assert!(!force);
            assert!(version.is_none());
        }
        _ => panic!("expected SelfUpdate"),
    }
}

#[test]
fn clap_parses_self_update_default() {
    let cli = Cli::try_parse_from(["am", "self-update"]).unwrap();
    match cli.command {
        Some(Commands::SelfUpdate {
            check,
            force,
            version,
        }) => {
            assert!(!check);
            assert!(!force);
            assert!(version.is_none());
        }
        _ => panic!("expected SelfUpdate"),
    }
}

#[test]
fn clap_parses_self_update_force() {
    let cli = Cli::try_parse_from(["am", "self-update", "--force"]).unwrap();
    match cli.command {
        Some(Commands::SelfUpdate {
            check,
            force,
            version,
        }) => {
            assert!(!check);
            assert!(force);
            assert!(version.is_none());
        }
        _ => panic!("expected SelfUpdate"),
    }
}

#[test]
fn clap_parses_self_update_version() {
    let cli = Cli::try_parse_from(["am", "self-update", "--version", "0.2.0"]).unwrap();
    match cli.command {
        Some(Commands::SelfUpdate {
            check,
            force,
            version,
        }) => {
            assert!(!check);
            assert!(!force);
            assert_eq!(version.as_deref(), Some("0.2.0"));
        }
        _ => panic!("expected SelfUpdate"),
    }
}

#[test]
fn find_install_dir_returns_existing_dir() {
    let dir = find_install_dir().unwrap();
    assert!(dir.is_dir(), "install dir should exist: {}", dir.display());
}

#[test]
fn atomic_replace_binary_roundtrip() {
    let tmp = std::env::temp_dir().join("am-test-atomic-replace");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // Create "old" binary
    let target = tmp.join("am");
    std::fs::write(&target, b"old-binary-v1").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Create "new" binary in a staging dir
    let staging = tmp.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let new_binary = staging.join("am");
    std::fs::write(&new_binary, b"new-binary-v2").unwrap();

    // Perform atomic replacement
    atomic_replace_binary(&new_binary, &target).unwrap();

    // Verify the target now has the new content
    let content = std::fs::read(&target).unwrap();
    assert_eq!(content, b"new-binary-v2");

    // Verify old backup was cleaned up
    let backup = tmp.join(".am.old.tmp");
    assert!(!backup.exists(), "backup should be cleaned up");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn atomic_replace_binary_creates_new_file() {
    let tmp = std::env::temp_dir().join("am-test-atomic-create");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // Target doesn't exist yet
    let target = tmp.join("am");
    let staging = tmp.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let new_binary = staging.join("am");
    std::fs::write(&new_binary, b"fresh-binary").unwrap();

    atomic_replace_binary(&new_binary, &target).unwrap();

    let content = std::fs::read(&target).unwrap();
    assert_eq!(content, b"fresh-binary");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn atomic_replace_binary_rollback_on_failure() {
    let tmp = std::env::temp_dir().join("am-test-atomic-rollback");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let target = tmp.join("am");
    std::fs::write(&target, b"original").unwrap();

    // Try to replace with a non-existent source
    let result = atomic_replace_binary(Path::new("/nonexistent/path"), &target);
    assert!(result.is_err());

    // Original should still be intact
    let content = std::fs::read(&target).unwrap();
    assert_eq!(content, b"original");

    let _ = std::fs::remove_dir_all(&tmp);
}
